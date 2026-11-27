//! 守护管道的访问控制：谁能连、连上之后能做什么。
//!
//! 服务以 LocalSystem 运行，而 `inject` 会让服务替调用方注入键鼠（穿透 UIPI）。
//! 这等于一个提权原语，所以分两层设防：
//!
//! - **管道 DACL** 只放行 SYSTEM、管理员，以及本机交互登录的用户（`IU`）；
//! - **逐请求鉴权**（见 [`crate::protocol::authorize`]）：特权操作还要核对
//!   调用方进程所在会话与用户。
//!
//! 取不到调用方身份时一律按「最严格」处理（拒绝）。

#![allow(unsafe_code)] // 安全描述符 / 令牌查询 FFI 边界

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, LocalFree, BOOL, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows::Win32::Security::{
    GetLengthSid, GetTokenInformation, TokenElevation, TokenSessionId, TokenUser,
    PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
use windows::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use rc_appcore::named_pipe::encode_wide;

/// 管道 DACL：SYSTEM 与管理员完全控制；本机交互登录的用户读写。
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";

/// 一个进程的身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerIdentity {
    /// 进程所在的 Windows 会话号（0 = 服务会话）。
    pub session_id: u32,
    /// 用户 SID（原始字节）。
    pub sid: Vec<u8>,
    /// 进程令牌是否已提权（UAC 下「以管理员身份运行」，或服务自身）。
    pub elevated: bool,
}

/// 管道的安全属性（持有底层安全描述符，离开作用域自动释放）。
pub struct PipeSecurity {
    descriptor: PSECURITY_DESCRIPTOR,
    attributes: SECURITY_ATTRIBUTES,
}

impl PipeSecurity {
    /// 只允许本机交互登录的用户、SYSTEM 与管理员连接。
    pub fn interactive_only() -> windows::core::Result<Self> {
        let sddl = encode_wide(PIPE_SDDL);
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                1,
                &mut descriptor,
                None,
            )?;
        }
        Ok(Self {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: BOOL(0),
            },
        })
    }

    /// 传给 `CreateNamedPipeW` 的安全属性。
    pub fn attributes(&self) -> *const SECURITY_ATTRIBUTES {
        &self.attributes
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(HLOCAL(self.descriptor.0));
        }
    }
}

/// 管道对端进程的身份；取不到返回 `None`（调用方据此拒绝）。
pub fn pipe_client_identity(pipe: HANDLE) -> Option<CallerIdentity> {
    let mut pid = 0u32;
    unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) }.ok()?;
    process_identity(pid)
}

/// 指定桌面会话登录用户的 SID（服务以 LocalSystem 运行时可用）。
pub fn session_user_sid(session_id: u32) -> Option<Vec<u8>> {
    let mut token = HANDLE::default();
    unsafe { WTSQueryUserToken(session_id, &mut token) }.ok()?;
    let sid = token_identity(token).map(|identity| identity.sid);
    unsafe {
        let _ = CloseHandle(token);
    }
    sid
}

fn process_identity(pid: u32) -> Option<CallerIdentity> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut token = HANDLE::default();
    let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
    unsafe {
        let _ = CloseHandle(process);
    }
    opened.ok()?;
    let identity = token_identity(token);
    unsafe {
        let _ = CloseHandle(token);
    }
    identity
}

fn token_identity(token: HANDLE) -> Option<CallerIdentity> {
    Some(CallerIdentity {
        session_id: token_u32(token, TokenSessionId)?,
        sid: token_user_sid(token)?,
        elevated: token_u32(token, TokenElevation)? != 0,
    })
}

/// 读取一个 `u32` 大小的令牌信息（会话号 / 提权标志）。
fn token_u32(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Option<u32> {
    let mut value = 0u32;
    let mut returned = 0u32;
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(&mut value as *mut u32 as *mut core::ffi::c_void),
            std::mem::size_of::<u32>() as u32,
            &mut returned,
        )
    }
    .ok()?;
    Some(value)
}

/// 读取令牌的用户 SID，并拷出原始字节（令牌关闭后仍可比较）。
fn token_user_sid(token: HANDLE) -> Option<Vec<u8>> {
    // TOKEN_USER 内含指针：缓冲区按 8 字节对齐
    const WORDS: usize = 16;
    let mut buffer = vec![0u64; WORDS];
    let mut returned = 0u32;
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr() as *mut core::ffi::c_void),
            (WORDS * std::mem::size_of::<u64>()) as u32,
            &mut returned,
        )
    }
    .ok()?;
    let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
    let sid = user.User.Sid;
    if sid.0.is_null() {
        return None;
    }
    let len = unsafe { GetLengthSid(sid) } as usize;
    if len == 0 {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(sid.0 as *const u8, len) }.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_has_a_usable_identity() {
        // 本进程一定查得到自己的身份；字段之间满足基本不变量
        let identity = process_identity(std::process::id()).expect("查询自身身份");
        assert!(identity.session_id > 0, "测试进程应运行在交互会话");
        assert!(identity.sid.len() >= 8, "SID 至少包含 8 字节头");
        // 重复查询得到相同结果
        assert_eq!(
            process_identity(std::process::id()).as_ref(),
            Some(&identity)
        );
    }

    #[test]
    fn an_unknown_pid_has_no_identity() {
        // PID 0（System Idle Process）打不开：必须返回 None，而不是伪造一个身份
        assert!(process_identity(0).is_none());
    }

    #[test]
    fn the_pipe_dacl_is_the_expected_sddl() {
        // 文本本身就是要写进管道的授权清单：改动必须是有意的
        assert_eq!(PIPE_SDDL, "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)");
        let security = PipeSecurity::interactive_only().expect("解析 SDDL");
        assert!(!security.attributes().is_null());
    }
}

//! 「复制并分享」的文案与设备码的显示格式。
//!
//! 主界面的「开始协助」页与后台被控进程（`rc-host-agent`）的托盘菜单共用同一份：
//! 两个入口复制出来的内容必须一字不差。

/// 把设备码按 3-3-3 分组（`419450905` → `419 450 905`）；不是 9 位数字就原样返回。
pub fn group_device_id(id: &str) -> String {
    let digits: Vec<char> = id.chars().collect();
    if digits.len() == 9 && digits.iter().all(char::is_ascii_digit) {
        format!("{} {} {}", id[..3].to_owned(), &id[3..6], &id[6..])
    } else {
        id.to_string()
    }
}

/// 「复制并分享」放到剪贴板的内容（与 macOS 的同一功能一字不差）：
/// 设备 ID 一行，临时验证码一行；被控没开（没有验证码）就只有设备 ID 那一行。
pub fn share_text(device_code: &str, temp_code: &str) -> String {
    if temp_code.is_empty() {
        format!("设备 ID: {device_code}")
    } else {
        format!("设备 ID: {device_code}\n验证码: {temp_code}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_code_is_grouped_in_threes() {
        assert_eq!(group_device_id("419450905"), "419 450 905");
        assert_eq!(group_device_id("12345"), "12345");
        assert_eq!(group_device_id("41945O905"), "41945O905");
    }

    #[test]
    fn share_text_matches_the_macos_wording() {
        assert_eq!(
            share_text("419450905", "BAM78E4U"),
            "设备 ID: 419450905\n验证码: BAM78E4U"
        );
        assert_eq!(share_text("419450905", ""), "设备 ID: 419450905");
    }
}

use reqwest::{blocking::Client, redirect::Policy, StatusCode, Url};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Deserialize)]
pub struct Session {
    access_token: String,
    refresh_token: String,
    pub user: User,
}
#[derive(Deserialize)]
pub struct User {
    pub email: String,
    pub must_change_password: bool,
}
#[derive(Serialize)]
struct Credentials<'a> {
    email: &'a str,
    password: &'a str,
}

fn client() -> Result<Client, &'static str> {
    Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|_| "网络客户端初始化失败。")
}

pub fn login(server: &Url, email: &str, password: &str) -> Result<Session, &'static str> {
    let mut endpoint = server.clone();
    endpoint.set_path("/api/v1/auth/login");
    let response = client()?
        .post(endpoint)
        .json(&Credentials { email, password })
        .send()
        .map_err(|_| "连接失败，请检查服务器地址、网络和 HTTPS 证书。")?;
    match response.status() {
        StatusCode::OK => {
            let session: Session = response
                .json()
                .map_err(|_| "服务器返回了无效的登录信息。")?;
            if session.access_token.is_empty()
                || session.refresh_token.is_empty()
                || session.user.email.is_empty()
            {
                return Err("服务器返回了无效的登录信息。");
            }
            if session.user.must_change_password {
                let _ = logout(server, session);
                return Err("该账号需要先在 RabbitControl 管理网页修改初始密码。");
            }
            Ok(session)
        }
        StatusCode::UNAUTHORIZED => Err("邮箱或密码不正确。"),
        StatusCode::FORBIDDEN => Err("账号已停用或无登录权限。"),
        StatusCode::TOO_MANY_REQUESTS => Err("登录尝试过于频繁，请稍后再试。"),
        _ => Err("服务器未接受登录请求，请检查地址或稍后重试。"),
    }
}

pub fn logout(server: &Url, session: Session) -> Result<(), &'static str> {
    #[derive(Serialize)]
    struct Logout {
        refresh_token: String,
    }
    let mut endpoint = server.clone();
    endpoint.set_path("/api/v1/auth/logout");
    client()?
        .post(endpoint)
        .bearer_auth(session.access_token)
        .json(&Logout {
            refresh_token: session.refresh_token,
        })
        .send()
        .and_then(|response| response.error_for_status())
        .map(|_| ())
        .map_err(|_| "本地已退出。服务器未确认会话注销。")
}

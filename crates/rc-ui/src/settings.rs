use reqwest::Url;
use std::io::{self, ErrorKind};
use winreg::{enums::HKEY_CURRENT_USER, RegKey};

const KEY: &str = r"Software\RabbitControl\Windows";
const DEFAULT: &str = "https://127.0.0.1:21118";

pub fn parse_origin(input: &str) -> Result<Url, &'static str> {
    let input = input.trim();
    let bare = !input.contains("://");
    let value = if bare {
        format!("https://{input}")
    } else {
        input.to_owned()
    };
    let invalid = "请输入有效的服务器地址，例如 https://example.com:21118。";
    let mut url = Url::parse(&value).map_err(|_| invalid)?;
    let host = url.host_str().ok_or(invalid)?;
    let local = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if url.scheme() != "https" && !(local && url.scheme() == "http") {
        return Err("远程服务器需使用 HTTPS。本机调试可使用 HTTP。");
    }
    if url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("仅填写协议、主机和端口，请移除路径、账号或查询参数。");
    }
    // Url normalizes explicit :443 to no port. Preserve it if the user supplied it.
    if bare && url.port().is_none() && !input.trim_end_matches('/').ends_with(":443") {
        url.set_port(Some(21118)).map_err(|_| invalid)?;
    }
    Ok(url)
}

pub fn load() -> io::Result<Url> {
    let result = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(KEY)
        .and_then(|key| key.get_value::<String, _>("ServerOrigin"));
    let value = match result {
        Ok(value) => value,
        Err(error) if error.kind() == ErrorKind::NotFound => DEFAULT.to_owned(),
        Err(error) => return Err(error),
    };
    parse_origin(&value).map_err(|message| io::Error::new(ErrorKind::InvalidData, message))
}

pub fn save(url: &Url) -> io::Result<()> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(KEY)?;
    key.set_value("ServerOrigin", &url.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolves_server_origins() {
        assert_eq!(
            parse_origin("host.local").unwrap().as_str(),
            "https://host.local:21118/"
        );
        assert_eq!(
            parse_origin("host.local:443").unwrap().as_str(),
            "https://host.local/"
        );
        assert_eq!(
            parse_origin("https://host.local")
                .unwrap()
                .port_or_known_default(),
            Some(443)
        );
        assert!(parse_origin("http://[::1]:32100").is_ok());
        for invalid in [
            "",
            "https://one.test/api",
            "http://one.test",
            "https://user:pass@one.test",
            "https://one.test/#f",
        ] {
            assert!(parse_origin(invalid).is_err(), "{invalid}");
        }
    }
}

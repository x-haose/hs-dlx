//! 认出一个代理端口说的协议：SOCKS5（RFC 1928，认证见 RFC 1929）或 HTTP（CONNECT 建隧道），并核对用户名与密码。
//! 设置里填了主机与端口、没写协议时照它定协议；写了协议时照它核对。
//!
//! 两种同时试：SOCKS5 的握手只在本机与代理之间，一来一回即知；HTTP 要代理真去连 [`CONNECT_TARGET`]、回 200 才算
//! （网页服务器对 CONNECT 也回 HTTP 的状态，只看回没回 HTTP 认不出是不是代理）。两种都通（Clash 等的混合端口）时取
//! SOCKS5。

use std::time::Duration;

use base64::Engine as _;
use hs_m3u8_core::Url;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// 连上代理最多等多久：代理多在本机或近处。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// 连上后等 SOCKS5 的握手回应最多多久：代理自己回，一来一回。HTTP 代理收到 SOCKS5 的握手多半一直等着，这里等满。
const SOCKS_REPLY_TIMEOUT: Duration = Duration::from_secs(3);
/// 连上后等 HTTP 代理回应最多多久：它要先连上 [`CONNECT_TARGET`] 才回，经远处的节点时要几秒。
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// HTTP 代理的 CONNECT 连去哪里：要是代理连得上、回 200 的地方。
const CONNECT_TARGET: &str = "www.gstatic.com:443";
/// HTTP 回应的头最多读多少字节：只要状态行。
const HEAD_LIMIT: usize = 8 * 1024;

/// 代理说的协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyProtocol {
    /// 存成 `socks5h://`：主机名交给代理解析
    Socks5,
    /// 存成 `http://`
    Http,
}

/// 要认哪种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyWanted {
    /// 哪种都行，两种都通时取 SOCKS5
    Any,
    Only(ProxyProtocol),
}

/// 认不出时的原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProxyProbeError {
    /// 填的不是主机与端口（为原因）
    #[error("{0}")]
    Invalid(String),
    /// 解析不出主机、连不上、连上就断（为原因）：多为代理软件没开或端口不对
    #[error("{0}")]
    Unreachable(String),
    /// 连得上，但两种协议都不像（为原因）
    #[error("{0}")]
    NotProxy(String),
    /// 是要的协议，但要用户名与密码，没给
    #[error("这个代理要用户名与密码")]
    LoginNeeded,
    /// 给了用户名与密码，代理不认
    #[error("代理不认这个用户名与密码")]
    LoginRefused,
    /// 要 SOCKS5，这个端口只说 HTTP
    #[error("这个端口是 HTTP 代理：要填代理的 SOCKS5 端口（Clash 等的混合端口两种都行）")]
    HttpOnly,
}

/// 用户名与密码（明文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyLogin {
    pub username: String,
    pub password: String,
}

/// 认出 `host:port` 上的代理（照 `wanted`），返回存进设置的地址：SOCKS5 为 `socks5h://`，HTTP 为 `http://`，带上
/// `login`。`host` 为主机名、IPv4 地址或 IPv6 地址（可不带方括号）。
pub async fn identify(
    host: &str,
    port: u16,
    login: Option<&ProxyLogin>,
    wanted: ProxyWanted,
) -> Result<Url, ProxyProbeError> {
    let host = host.trim();
    if host.is_empty() {
        return Err(ProxyProbeError::Invalid(
            "要填代理的主机，本机的为 127.0.0.1".to_owned(),
        ));
    }
    if port == 0 {
        return Err(ProxyProbeError::Invalid(
            "端口要在 1 到 65535 之间".to_owned(),
        ));
    }
    let bracketed = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let at = format!("{bracketed}:{port}");
    let protocol = match wanted {
        ProxyWanted::Only(ProxyProtocol::Socks5) => socks5(&at, login).await?,
        ProxyWanted::Only(ProxyProtocol::Http) => http(&at, login).await?,
        ProxyWanted::Any => either(&at, login).await?,
    };
    let scheme = match protocol {
        ProxyProtocol::Socks5 => "socks5h",
        ProxyProtocol::Http => "http",
    };
    let mut url = Url::parse(&format!("{scheme}://{at}"))
        .map_err(|error| ProxyProbeError::Invalid(format!("{at} 不是主机与端口：{error}")))?;
    if let Some(login) = login {
        // 能设主机的地址都能设用户名与密码；按百分号编码写进去
        url.set_username(&login.username)
            .expect("有主机的地址能设用户名");
        url.set_password(Some(&login.password))
            .expect("有主机的地址能设密码");
    }
    Ok(url)
}

/// 两种同时试：SOCKS5 通了即取它，否则看 HTTP。两种都不通时报更说明问题的那个（连不上、要登录优先于不像代理）。
async fn either(at: &str, login: Option<&ProxyLogin>) -> Result<ProxyProtocol, ProxyProbeError> {
    let socks = socks5(at, login);
    let http = http(at, login);
    tokio::pin!(socks, http);
    // SOCKS5 没通的原因；HTTP 的结果（通了也等 SOCKS5 试完：两种都通时取 SOCKS5）
    let mut socks_failed: Option<ProxyProbeError> = None;
    let mut http_result: Option<Result<ProxyProtocol, ProxyProbeError>> = None;
    loop {
        tokio::select! {
            result = &mut socks, if socks_failed.is_none() => match result {
                Ok(protocol) => return Ok(protocol),
                Err(error) => socks_failed = Some(error),
            },
            result = &mut http, if http_result.is_none() => http_result = Some(result),
        }
        if let (Some(socks), Some(http)) = (&socks_failed, &http_result) {
            return match http {
                Ok(protocol) => Ok(*protocol),
                Err(http) => Err(worse(socks.clone(), http.clone())),
            };
        }
    }
}

/// 两种都没认出时报哪个：要登录与登录被拒说明是代理，连不上说明端口没人听，都比「不像代理」有用。
fn worse(socks: ProxyProbeError, http: ProxyProbeError) -> ProxyProbeError {
    let rank = |error: &ProxyProbeError| match error {
        ProxyProbeError::LoginRefused => 0,
        ProxyProbeError::LoginNeeded => 1,
        ProxyProbeError::Unreachable(_) => 2,
        ProxyProbeError::Invalid(_) | ProxyProbeError::HttpOnly => 3,
        ProxyProbeError::NotProxy(_) => 4,
    };
    if rank(&socks) <= rank(&http) {
        socks
    } else {
        http
    }
}

async fn connect(at: &str) -> Result<TcpStream, ProxyProbeError> {
    match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(at)).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(error)) => Err(ProxyProbeError::Unreachable(format!(
            "连不上 {at}（{error}）：代理软件没开，或端口不对"
        ))),
        Err(_) => Err(ProxyProbeError::Unreachable(format!(
            "{} 秒内没连上 {at}：主机或端口不对",
            CONNECT_TIMEOUT.as_secs()
        ))),
    }
}

/// 照 RFC 1928 握手（有 `login` 时提供免认证与用户名密码两种，由代理选），要认证时照 RFC 1929 认证。
async fn socks5(at: &str, login: Option<&ProxyLogin>) -> Result<ProxyProtocol, ProxyProbeError> {
    let mut stream = connect(at).await?;
    let talk = async {
        let offered: &[u8] = if login.is_some() {
            &[5, 2, 0, 2]
        } else {
            &[5, 1, 0]
        };
        stream.write_all(offered).await?;
        let mut reply = [0; 2];
        stream.read_exact(&mut reply).await?;
        Ok::<_, std::io::Error>((stream, reply))
    };
    let not_socks = || ProxyProbeError::NotProxy(format!("{at} 不是 SOCKS5 代理"));
    let (mut stream, reply) = match tokio::time::timeout(SOCKS_REPLY_TIMEOUT, talk).await {
        Ok(Ok(talked)) => talked,
        // 断开或没回：不说 SOCKS5（HTTP 代理收到这几个字节多半断开或等着）
        Ok(Err(_)) | Err(_) => return Err(not_socks()),
    };
    if reply[0] != 5 {
        return Err(not_socks());
    }
    match (reply[1], login) {
        (0, _) => Ok(ProxyProtocol::Socks5),
        (2, Some(login)) => {
            let mut request = vec![1, credential_len(&login.username)?];
            request.extend_from_slice(login.username.as_bytes());
            request.push(credential_len(&login.password)?);
            request.extend_from_slice(login.password.as_bytes());
            let talk = async {
                stream.write_all(&request).await?;
                let mut reply = [0; 2];
                stream.read_exact(&mut reply).await?;
                Ok::<_, std::io::Error>(reply)
            };
            match tokio::time::timeout(SOCKS_REPLY_TIMEOUT, talk).await {
                Ok(Ok([1, 0])) => Ok(ProxyProtocol::Socks5),
                Ok(Ok(_)) => Err(ProxyProbeError::LoginRefused),
                Ok(Err(error)) => Err(ProxyProbeError::Unreachable(format!(
                    "代理 {at} 认证时断开了：{error}"
                ))),
                Err(_) => Err(ProxyProbeError::Unreachable(format!(
                    "代理 {at} 认证时没回应"
                ))),
            }
        }
        // 不接受免认证：没给用户名与密码时为要登录，给了而它也不接受用户名密码时为不认
        (_, None) => Err(ProxyProbeError::LoginNeeded),
        (_, Some(_)) => Err(ProxyProbeError::LoginRefused),
    }
}

/// RFC 1929 的用户名与密码各最长 255 字节。
fn credential_len(text: &str) -> Result<u8, ProxyProbeError> {
    u8::try_from(text.len()).map_err(|_| {
        ProxyProbeError::Invalid("SOCKS5 的用户名与密码各不能超过 255 字节".to_owned())
    })
}

/// 发 CONNECT 到 [`CONNECT_TARGET`]（有 `login` 时带 Basic 认证）：回 200 为 HTTP 代理，407 为要登录（带了时为不认）。
async fn http(at: &str, login: Option<&ProxyLogin>) -> Result<ProxyProtocol, ProxyProbeError> {
    let mut stream = connect(at).await?;
    let mut request = format!("CONNECT {CONNECT_TARGET} HTTP/1.1\r\nHost: {CONNECT_TARGET}\r\n");
    if let Some(login) = login {
        let token = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", login.username, login.password));
        request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    request.push_str("\r\n");
    let talk = async {
        stream.write_all(request.as_bytes()).await?;
        let mut head = Vec::new();
        let mut buffer = [0; 1024];
        // 读到状态行结束（或读满、对方断开）
        while !head.contains(&b'\n') && head.len() < HEAD_LIMIT {
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            head.extend_from_slice(&buffer[..read]);
        }
        Ok::<_, std::io::Error>(head)
    };
    let head = match tokio::time::timeout(REPLY_TIMEOUT, talk).await {
        Ok(Ok(head)) => head,
        Ok(Err(_)) | Err(_) => {
            return Err(ProxyProbeError::NotProxy(format!("{at} 不是 HTTP 代理")));
        }
    };
    let line = String::from_utf8_lossy(&head);
    let status = line
        .strip_prefix("HTTP/1.")
        .and_then(|rest| rest.get(2..5))
        .and_then(|code| code.parse::<u16>().ok());
    match (status, login) {
        (Some(200), _) => Ok(ProxyProtocol::Http),
        (Some(407), None) => Err(ProxyProbeError::LoginNeeded),
        (Some(407), Some(_)) => Err(ProxyProbeError::LoginRefused),
        (Some(code), _) => Err(ProxyProbeError::NotProxy(format!(
            "{at} 对 CONNECT 回了 HTTP {code}：不是代理，或代理连不上外网"
        ))),
        (None, _) => Err(ProxyProbeError::NotProxy(format!("{at} 不是 HTTP 代理"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// 在本机开一个端口，每个连接交给 `serve`；返回端口。
    async fn listen<F, Fut>(serve: F) -> u16
    where
        F: Fn(TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream));
            }
        });
        port
    }

    /// SOCKS5：`login` 为 Some 时只接受用户名密码认证，且要对得上。
    async fn socks_server(login: Option<(&'static str, &'static str)>) -> u16 {
        listen(move |mut stream| async move {
            let mut head = [0; 2];
            if stream.read_exact(&mut head).await.is_err() || head[0] != 5 {
                return;
            }
            let mut methods = vec![0; usize::from(head[1])];
            stream.read_exact(&mut methods).await.unwrap();
            let Some((user, pass)) = login else {
                stream.write_all(&[5, 0]).await.unwrap();
                return;
            };
            if !methods.contains(&2) {
                stream.write_all(&[5, 0xFF]).await.unwrap();
                return;
            }
            stream.write_all(&[5, 2]).await.unwrap();
            let mut version_len = [0; 2];
            stream.read_exact(&mut version_len).await.unwrap();
            let mut name = vec![0; usize::from(version_len[1])];
            stream.read_exact(&mut name).await.unwrap();
            let mut len = [0; 1];
            stream.read_exact(&mut len).await.unwrap();
            let mut password = vec![0; usize::from(len[0])];
            stream.read_exact(&mut password).await.unwrap();
            let ok = name == user.as_bytes() && password == pass.as_bytes();
            stream.write_all(&[1, u8::from(!ok)]).await.unwrap();
        })
        .await
    }

    /// 读到请求头结束后回 `reply`。
    async fn http_server(reply: &'static str) -> u16 {
        listen(move |mut stream| async move {
            let mut head = Vec::new();
            let mut buffer = [0; 512];
            while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buffer).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => head.extend_from_slice(&buffer[..read]),
                }
            }
            let _closed = stream.write_all(reply.as_bytes()).await;
        })
        .await
    }

    fn login(username: &str, password: &str) -> ProxyLogin {
        ProxyLogin {
            username: username.to_owned(),
            password: password.to_owned(),
        }
    }

    #[tokio::test]
    async fn a_socks5_port_is_saved_as_socks5h_and_its_login_checked() {
        let open = socks_server(None).await;
        assert_eq!(
            identify("127.0.0.1", open, None, ProxyWanted::Any)
                .await
                .unwrap()
                .as_str(),
            format!("socks5h://127.0.0.1:{open}")
        );
        let guarded = socks_server(Some(("u", "p@ss"))).await;
        assert_eq!(
            identify("127.0.0.1", guarded, None, ProxyWanted::Any).await,
            Err(ProxyProbeError::LoginNeeded)
        );
        assert_eq!(
            identify(
                "127.0.0.1",
                guarded,
                Some(&login("u", "bad")),
                ProxyWanted::Any
            )
            .await,
            Err(ProxyProbeError::LoginRefused)
        );
        assert_eq!(
            identify(
                "127.0.0.1",
                guarded,
                Some(&login("u", "p@ss")),
                ProxyWanted::Any
            )
            .await
            .unwrap()
            .as_str(),
            format!("socks5h://u:p%40ss@127.0.0.1:{guarded}")
        );
    }

    #[tokio::test]
    async fn an_http_proxy_answers_connect_and_bt_refuses_it() {
        let proxy = http_server("HTTP/1.1 200 Connection established\r\n\r\n").await;
        assert_eq!(
            identify("localhost", proxy, None, ProxyWanted::Any)
                .await
                .unwrap()
                .as_str(),
            format!("http://localhost:{proxy}/")
        );
        assert_eq!(
            identify(
                "127.0.0.1",
                proxy,
                None,
                ProxyWanted::Only(ProxyProtocol::Socks5)
            )
            .await,
            Err(ProxyProbeError::NotProxy(format!(
                "127.0.0.1:{proxy} 不是 SOCKS5 代理"
            )))
        );
        let guarded = http_server("HTTP/1.1 407 Proxy Authentication Required\r\n\r\n").await;
        assert_eq!(
            identify("127.0.0.1", guarded, None, ProxyWanted::Any).await,
            Err(ProxyProbeError::LoginNeeded)
        );
    }

    #[tokio::test]
    async fn a_web_server_or_a_closed_port_is_not_taken_for_a_proxy() {
        let web = http_server("HTTP/1.1 405 Method Not Allowed\r\n\r\n").await;
        assert!(matches!(
            identify("127.0.0.1", web, None, ProxyWanted::Any).await,
            Err(ProxyProbeError::NotProxy(_))
        ));
        // 开过又关掉的端口：没人在听
        let closed = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(matches!(
            identify("127.0.0.1", closed, None, ProxyWanted::Any).await,
            Err(ProxyProbeError::Unreachable(_))
        ));
        assert!(matches!(
            identify(" ", 1080, None, ProxyWanted::Any).await,
            Err(ProxyProbeError::Invalid(_))
        ));
    }
}

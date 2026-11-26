//! 填代理的一组输入：主机（默认本机）、端口、协议（默认连上去自己认）与可选的用户名、密码。保存时经
//! [`hs_dlx_app::Service::identify_proxy`] 连上去核对，认出了才交出地址（[`ProxyEditorEvent::Identified`]），由所在的
//! 一行存进设置。保存时主机框里是完整的代理地址（`socks5://u:p@host:1080` 之类）时拆进各框、协议照它写的，是
//! `主机:端口` 时拆成两框（IPv6 地址除外）。存着的地址是 HTTP 或 SOCKS5 时协议显示为自动判断，换端口后重新认。

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, IntoElement, ParentElement as _, Render,
    Styled as _, Subscription, Task, Window, div, px,
};
use hs_dlx_app::{ProxyLogin, ProxyProbeError, ProxyProtocol, ProxyWanted};
use hs_m3u8_core::Url;

use crate::tasks::Tasks;
use crate::ui::{self, Tone};

/// 主机框一开始填的：代理软件多在本机。
const LOCAL_HOST: &str = "127.0.0.1";

/// 协议怎么定。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Choice {
    /// 连上去自己认（见 [`ProxyWanted::Any`]）
    Auto,
    Fixed(ProxyProtocol),
    /// 粘贴或存着的地址里写的、不在上面几种里的（https、socks4 等）：照写的存，不核对
    Written(String),
}

/// 交给所在一行的事件。
pub enum ProxyEditorEvent {
    /// 认出了，为存进设置的地址
    Identified(Url),
    /// 改了哪个框或协议（所在一行据此清掉上次没存进的原因）
    Changed,
}

pub struct ProxyEditor {
    tasks: Entity<Tasks>,
    /// BT 用：只认 SOCKS5，不给选协议
    socks_only: bool,
    host: Entity<InputState>,
    port: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    choice: Choice,
    /// 显示用户名与密码两框：存着的地址带用户名、代理说要登录、或用户点开了
    login: bool,
    /// 改过、还没存进（见 [`ProxyEditor::identified`]）
    edited: bool,
    /// 在认时有（释放即取消）
    identifying: Option<Task<()>>,
    /// 上次没认出的原因
    error: Option<String>,
    _typed: Vec<Subscription>,
}

impl EventEmitter<ProxyEditorEvent> for ProxyEditor {}

impl ProxyEditor {
    /// `saved` 为存着的地址（没有时主机为本机、端口空着）。
    pub fn new(
        tasks: Entity<Tasks>,
        socks_only: bool,
        saved: Option<&Url>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let host = cx.new(|cx| InputState::new(window, cx).default_value(LOCAL_HOST));
        let port = cx.new(|cx| InputState::new(window, cx).placeholder("端口"));
        let username = cx.new(|cx| InputState::new(window, cx).placeholder("用户名"));
        let password = cx.new(|cx| InputState::new(window, cx).placeholder("密码").masked(true));
        let typed = [&host, &port, &username, &password]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(
                    input,
                    window,
                    |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                        InputEvent::PressEnter { .. } => this.save(window, cx),
                        InputEvent::Change => this.changed(cx),
                        InputEvent::Focus | InputEvent::Blur => {}
                    },
                )
            })
            .collect();
        let mut editor = ProxyEditor {
            tasks,
            socks_only,
            host,
            port,
            username,
            password,
            choice: Choice::Auto,
            login: false,
            edited: false,
            identifying: None,
            error: None,
            _typed: typed,
        };
        if let Some(url) = saved {
            editor.choice = match written_choice(url) {
                Choice::Fixed(_) => Choice::Auto,
                choice => choice,
            };
            editor.fill(url, window, cx);
        }
        editor
    }

    /// 照 `url` 填各框（协议不动），算作没改过。
    pub fn fill(&mut self, url: &Url, window: &mut Window, cx: &mut Context<Self>) {
        let host = url.host_str().unwrap_or_default();
        // IPv6 地址在地址里带方括号，框里不带
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host)
            .to_owned();
        let port = url
            .port_or_known_default()
            .map(|port| port.to_string())
            .unwrap_or_default();
        let decoded = |text: &str| {
            percent_encoding::percent_decode_str(text)
                .decode_utf8_lossy()
                .into_owned()
        };
        let username = decoded(url.username());
        let password = url.password().map(decoded).unwrap_or_default();
        self.login = !username.is_empty();
        for (input, value) in [
            (&self.host, host),
            (&self.port, port),
            (&self.username, username),
            (&self.password, password),
        ] {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        self.edited = false;
        self.error = None;
        cx.notify();
    }

    /// 改过、还没保存。
    pub fn edited(&self) -> bool {
        self.edited
    }

    /// 填了端口（选「经代理」时据此决定要不要随即保存）。
    pub fn has_port(&self, cx: &gpui_kit::App) -> bool {
        !self.port.read(cx).value().trim().is_empty()
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.edited = true;
        self.error = None;
        // 改了就不再等上次的结果
        self.identifying = None;
        cx.emit(ProxyEditorEvent::Changed);
        cx.notify();
    }

    /// 不再等在做的核对（所在一行改选了直连、跟随系统时）：它的结果不再交出。
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.identifying = None;
        cx.notify();
    }

    /// 主机框里是完整的代理地址时拆进各框、协议照它写的；是 `主机:端口`（不是 IPv6 地址）时拆成两框。认不出的完整
    /// 地址为原因。
    fn split_host(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        let text = self.host.read(cx).value().trim().to_owned();
        if text.contains("://") {
            let url = Url::parse(&text)
                .ok()
                .filter(|url| url.host_str().is_some())
                .ok_or_else(|| format!("{text} 不是代理地址"))?;
            self.choice = written_choice(&url);
            self.fill(&url, window, cx);
            self.edited = true;
            return Ok(());
        }
        if text.parse::<std::net::Ipv6Addr>().is_err()
            && let Some((host, port)) = text.rsplit_once(':')
            && !host.contains(':')
            && port.parse::<u16>().is_ok()
        {
            let (host, port) = (host.to_owned(), port.to_owned());
            self.host
                .update(cx, |input, cx| input.set_value(host, window, cx));
            self.port
                .update(cx, |input, cx| input.set_value(port, window, cx));
        }
        Ok(())
    }

    fn pick(&mut self, choice: Choice, cx: &mut Context<Self>) {
        self.choice = choice;
        self.changed(cx);
    }

    /// 核对各框，连上去认；认出了交出地址。
    pub fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(reason) = self.split_host(window, cx) {
            self.error = Some(reason);
            cx.notify();
            return;
        }
        let host = self.host.read(cx).value().trim().to_owned();
        let port = self.port.read(cx).value().trim().to_owned();
        let Ok(port) = port.parse::<u16>() else {
            self.error = Some("端口要是 1 到 65535 之间的数".to_owned());
            cx.notify();
            return;
        };
        let username = self.username.read(cx).value().to_string();
        let login = (self.login && !username.is_empty()).then(|| ProxyLogin {
            username,
            password: self.password.read(cx).value().to_string(),
        });
        let wanted = match &self.choice {
            _ if self.socks_only => ProxyWanted::Only(ProxyProtocol::Socks5),
            Choice::Auto => ProxyWanted::Any,
            Choice::Fixed(protocol) => ProxyWanted::Only(*protocol),
            Choice::Written(scheme) => {
                match written(scheme, &host, port, login.as_ref()) {
                    Ok(url) => self.identified(url, cx),
                    Err(reason) => self.error = Some(reason),
                }
                cx.notify();
                return;
            }
        };
        let handle = self
            .tasks
            .read(cx)
            .service()
            .identify_proxy(host, port, login, wanted);
        self.error = None;
        self.identifying = Some(cx.spawn(async move |this, cx| {
            let result = handle.await;
            // 关了时不再需要结果
            let _closed = this.update(cx, |this, cx| {
                this.identifying = None;
                match result {
                    Ok(Ok(url)) => this.identified(url, cx),
                    Ok(Err(ProxyProbeError::LoginNeeded)) => {
                        this.login = true;
                        this.error = Some(ProxyProbeError::LoginNeeded.to_string());
                    }
                    Ok(Err(error)) => this.error = Some(error.to_string()),
                    Err(error) => this.error = Some(format!("核对代理的任务没做完：{error}")),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 交出认出的地址；所在一行存进了才 [`ProxyEditor::fill`]（算作没改过），没存进时照旧算改过。
    fn identified(&mut self, url: Url, cx: &mut Context<Self>) {
        cx.emit(ProxyEditorEvent::Identified(url));
    }
}

/// 照地址里写的协议定：HTTP、SOCKS5 为手选的那种，别的照写的存。
fn written_choice(url: &Url) -> Choice {
    match url.scheme() {
        "http" => Choice::Fixed(ProxyProtocol::Http),
        "socks5" | "socks5h" => Choice::Fixed(ProxyProtocol::Socks5),
        other => Choice::Written(other.to_owned()),
    }
}

/// 照写的协议拼地址，不核对。
fn written(scheme: &str, host: &str, port: u16, login: Option<&ProxyLogin>) -> Result<Url, String> {
    let host = if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let mut url = Url::parse(&format!("{scheme}://{host}:{port}"))
        .map_err(|error| format!("{host}:{port} 不是主机与端口：{error}"))?;
    if let Some(login) = login {
        url.set_username(&login.username)
            .map_err(|()| "这个地址设不了用户名".to_owned())?;
        url.set_password(Some(&login.password))
            .map_err(|()| "这个地址设不了密码".to_owned())?;
    }
    Ok(url)
}

impl Render for ProxyEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let field = |input: &Entity<InputState>| {
            ui::input_frame()
                .px(px(6.))
                .child(Input::new(input).appearance(false))
        };
        let busy = self.identifying.is_some();
        let address = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(field(&self.host).flex_1())
            .child(field(&self.port).w(px(96.)))
            .child(ui::button(
                "save-proxy",
                if busy { "在核对…" } else { "保存" },
                Tone::Glow,
                cx.listener(|this, _, window, cx| {
                    if this.identifying.is_none() {
                        this.save(window, cx);
                    }
                }),
            ));
        let chip = |id: &'static str, label: String, choice: Choice, cx: &mut Context<Self>| {
            let selected = self.choice == choice;
            ui::chip(
                id,
                label,
                selected,
                cx.listener(move |this, _, _, cx| this.pick(choice.clone(), cx)),
            )
        };
        let protocols = (!self.socks_only).then(|| {
            let mut chips = div()
                .flex()
                .flex_wrap()
                .gap(px(8.))
                .child(chip("proxy-auto", "自动判断".to_owned(), Choice::Auto, cx))
                .child(chip(
                    "proxy-http",
                    "HTTP".to_owned(),
                    Choice::Fixed(ProxyProtocol::Http),
                    cx,
                ))
                .child(chip(
                    "proxy-socks5",
                    "SOCKS5".to_owned(),
                    Choice::Fixed(ProxyProtocol::Socks5),
                    cx,
                ));
            if let Choice::Written(scheme) = &self.choice {
                chips = chips.child(chip(
                    "proxy-written",
                    scheme.clone(),
                    self.choice.clone(),
                    cx,
                ));
            }
            chips
        });
        let login = if self.login {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(field(&self.username).flex_1())
                .child(field(&self.password).flex_1())
        } else {
            div().flex().child(ui::button(
                "proxy-login",
                "要用户名与密码",
                Tone::Ghost,
                cx.listener(|this, _, _, cx| {
                    this.login = true;
                    cx.notify();
                }),
            ))
        };
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(address)
            .children(protocols)
            .child(login)
            .children(self.error.clone().map(ui::error_text))
    }
}

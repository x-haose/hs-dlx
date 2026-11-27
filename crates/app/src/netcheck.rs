//! 检测 BT 用的网络：DHT（直连与经 BT 代理）、UDP 的 tracker、IPv6、监听与端口映射、外部连入，各下结论，再按「BT
//! 现在经不经代理」给一句建议。只发握手类的包（DHT 的 ping 与 find_node、BEP 15 的 connect，事务号随机），不带任何
//! 种子的信息哈希。探测与下结论分开：各探测函数量出 [`Measured`]（由 [`crate::Handle::check_network`] 汇总），
//! [`report`] 只照它下结论。

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher as _, Hasher as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hs_m3u8_core::Url;
use tokio::net::UdpSocket;

use crate::diagnosis::CheckLevel;
use crate::socks::{ProxyCheck, UdpRelay};

/// DHT 的引导节点：BT 会话照它引导 DHT（见 [`crate::torrent`] 建会话处），探 BT 代理与网络检测也问它们。
pub(crate) const DHT_BOOTSTRAP: [(&str, u16); 4] = [
    ("dht.libtorrent.org", 25401),
    ("router.bittorrent.com", 6881),
    ("router.utorrent.com", 6881),
    ("dht.transmissionbt.com", 6881),
];
/// 解析一个引导节点最多等多久。
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);
/// 每一轮发出后等回应多久；没回的再发一次、再等这么久（UDP 会丢包）。DHT 节点与 tracker 正常都在一两秒内回。
const REPLY_WAIT: Duration = Duration::from_secs(2);
/// 从引导节点要来的普通节点最多 ping 多少个：样本太少时回应率不可信。
const NODES_PINGED: usize = 32;

/// 问了几个、回了几个。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ratio {
    pub asked: usize,
    pub replied: usize,
}

/// 一路 UDP 的通畅程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Health {
    /// 普通 DHT 节点至少四分之一回应（节点表里常有已下线的，正常网络也回不满），且至少问了 8 个
    Healthy,
    /// 介于两者之间，或样本太少：再测一次
    Uncertain,
    /// 普通节点至多十分之一回应，引导节点（运营稳定的服务器）也至多一半回应
    Blocked,
    /// 解析不出引导节点的地址，没测
    Untested,
}

/// 一次 DHT 探测量到的。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Dht {
    /// 解析得出的引导节点（`asked`）与回了的
    pub bootstrap: Ratio,
    /// 解析不出地址的引导节点
    pub unresolved: usize,
    /// 引导节点给的普通节点
    pub nodes: Ratio,
    /// 回应里写的「对方看到的本机地址」（BEP 42 的 ip），至少两个回应一致才算
    pub external: Option<IpAddr>,
    /// 引导节点（有的）解析成了 198.18.0.0/15：系统里的 TUN 代理（fake-ip）接管了，「直连」其实经它
    pub tun: bool,
    /// 有引导节点解析成了真实地址（不在 198.18.0.0/15 里）
    pub real: bool,
    /// 收发本身出的错（见 [`Udp::exchange`]）；有时上面的回应数不可信
    pub failure: Option<String>,
}

impl Dht {
    pub(crate) fn health(&self) -> Health {
        let (nodes, bootstrap) = (self.nodes, self.bootstrap);
        if bootstrap.asked == 0 {
            Health::Untested
        } else if nodes.asked >= 8 && nodes.replied * 4 >= nodes.asked {
            Health::Healthy
        } else if nodes.replied * 10 <= nodes.asked && bootstrap.replied * 2 <= bootstrap.asked {
            Health::Blocked
        } else {
            Health::Uncertain
        }
    }
}

/// BT 会话报的监听与端口映射。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Session {
    /// 没在监听时为原因
    pub listen_error: Option<String>,
    pub mapping: Mapping,
    /// 有过节点从外面连进来
    pub incoming: bool,
}

/// 路由器的端口映射。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mapping {
    /// 映射到的外部端口
    Mapped(u16),
    /// 路由器回了失败，为说明
    Failed(String),
    /// 还没有结果
    Pending,
}

/// 经 BT 代理量到的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Proxied {
    /// 去掉凭据的地址
    pub address: String,
    pub check: ProxyCheck,
    /// 经它的 DHT；用不了时为 None
    pub dht: Option<Dht>,
}

/// 检测量到的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Measured {
    pub dht: Dht,
    /// UDP 的 tracker 握手；没测成时为原因（公共 tracker 列表还没取到、收发出错等）
    pub trackers: Result<Ratio, String>,
    /// IPv6：没有公网地址时为 None；有时为 IPv6 的 DHT 引导节点（解析不出的不算），没测成（开不了套接字、收发出错）
    /// 时为原因
    pub ipv6: Option<Result<Ratio, String>>,
    /// 设了 BT 代理时
    pub proxy: Option<Proxied>,
    /// BT 会话的；还没建（没用过 BT）时为 None
    pub session: Option<Session>,
}

/// 检测的一项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetItem {
    Dht,
    UdpTrackers,
    Ipv6,
    Proxy,
    Incoming,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetCheck {
    pub item: NetItem,
    pub level: CheckLevel,
    pub text: String,
}

/// 检测的结果：各项与一句总的建议。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkReport {
    pub checks: Vec<NetCheck>,
    pub verdict: String,
}

/// 设了 BT 代理而经它的引导节点一个都没回、直连的却有回的：代理接了 UDP 转发却没转发出去（节点不支持 UDP 等）。引导
/// 节点解析成了 TUN 代理的假地址时（见 [`Dht::tun`]，假地址经别的代理本来就到不了）、经代理收发出错时（原因另有）不下
/// 这个结论。
pub(crate) fn drops_udp(direct: &Dht, proxied: &Dht) -> bool {
    !proxied.tun
        && proxied.failure.is_none()
        && direct.bootstrap.replied > 0
        && proxied.bootstrap.asked > 0
        && proxied.bootstrap.replied == 0
}

/// 代理接了 UDP 转发却没转发出去时的说明。
pub(crate) const DROPS_UDP: &str =
    "代理接了 UDP 转发，但没转发出去：代理的节点可能不支持 UDP，换节点或在代理里打开 UDP";

/// 经代理问的 DHT 引导节点被 TUN 代理解析成假地址、一个都没回时的说明。libtorrent 解析引导节点同样用本机的 DNS，
/// 拿到的也是这些假地址。
pub(crate) static PROXY_FAKE_IP: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let hosts: Vec<&str> = DHT_BOOTSTRAP.iter().map(|(host, _)| *host).collect();
    format!(
        "DHT 引导节点被系统里的 TUN 代理解析成了假地址（198.18.0.0/15），经这个代理一个都没回；BT 同样照这些地址问，没有存下的节点时只靠 DHT 的磁力取不到种子信息。BT 代理不是 TUN 代理自己的 SOCKS 端口时，在 TUN 代理里把 {} 排除出 fake-ip（改完重启 hs-dlx：BT 只在启动时解析一次），或改用它的 SOCKS 端口；已是时，是它的节点没转发 UDP，换节点或打开 UDP",
        hosts.join("、")
    )
});

/// TUN 代理的 fake-ip 改好之后、BT 还拿着之前解析到的假地址时的说明（见 [`crate::torrent::Bt::set_proxy_check`]）。
pub(crate) const RESTART_AFTER_FAKE_IP: &str = "DHT 引导节点现在解析得到真实地址了，但 BT 还用着之前解析到的假地址（只在启动时解析一次）：重启 hs-dlx 后生效";

/// 经代理问的引导节点解析得怎样；没量（握手没成）或一个都没解析出来时都为假。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Resolved {
    /// 有解析成 TUN 的假地址的
    pub fake: bool,
    /// 有解析成真实地址的
    pub real: bool,
}

pub(crate) fn resolved(through: &Result<Dht, ProxyCheck>) -> Resolved {
    match through {
        Ok(through) => Resolved {
            fake: through.tun,
            real: through.real,
        },
        Err(_) => Resolved::default(),
    }
}

/// 经 BT 代理问 DHT 引导节点（`through` 为经它量到的，握手或申请转发 UDP 没成时为 [`crate::socks::associate`] 的
/// 结论）的结论，网络检测与后台看护记进 BT 引擎的都照它：只看经代理的，不拿直连的比。收发出错、引导节点一个都没回为
/// [`ProxyCheck::Silent`]（解析成 TUN 的假地址时说明为 [`PROXY_FAKE_IP`]）。引导节点都解析不出、没问成时握手成了即
/// 算能用。
pub(crate) fn judge_proxy(through: &Result<Dht, ProxyCheck>) -> ProxyCheck {
    match through {
        Err(check) => check.clone(),
        Ok(Dht {
            failure: Some(why), ..
        }) => ProxyCheck::Silent(why.clone()),
        Ok(through) if through.bootstrap.asked == 0 => ProxyCheck::Usable,
        Ok(through) if through.bootstrap.replied == 0 => ProxyCheck::Silent(if through.tun {
            PROXY_FAKE_IP.clone()
        } else {
            PROXY_SILENT.to_owned()
        }),
        Ok(_) => ProxyCheck::Usable,
    }
}

/// 经 BT 代理 `url` 探 DHT：握手、申请转发 UDP，再经它向引导节点 `hosts` 要节点（`hosts` 为空时只握手）。握手或申请
/// 没成时内层为 [`crate::socks::associate`] 的结论；开不了本机的 UDP 套接字时为外层的错。
pub(crate) async fn through_proxy(
    url: &Url,
    hosts: &[(&'static str, u16)],
) -> std::io::Result<Result<Dht, ProxyCheck>> {
    let relay = match crate::socks::associate(url).await {
        Ok(relay) => relay,
        Err(check) => return Ok(Err(check)),
    };
    let udp = match Udp::via(relay).await? {
        Ok(udp) => udp,
        Err(why) => {
            return Ok(Ok(Dht {
                failure: Some(why),
                ..Dht::default()
            }));
        }
    };
    let (found, unresolved) = bootstrap(hosts, false).await;
    Ok(Ok(dht(&udp, &found, unresolved).await))
}

/// 经代理问 DHT 引导节点一个都没回时的说明（只经代理问，分不清是代理还是网络的问题）。
pub(crate) const PROXY_SILENT: &str = "经代理问 DHT 引导节点一个都没回：代理可能没转发 UDP（换节点或在代理里打开 UDP），也可能网络不通";

/// 后台看着 BT 代理时探它（只经代理发包，不从本机直连发）：握手、申请转发 UDP，再经它向引导节点 `hosts` 各发一个
/// find_node（不 ping 别的节点），照 [`judge_proxy`] 下结论，并给出引导节点解析得怎样。开不了本机的 UDP 套接字时也
/// 算用不了，原因写明。
pub(crate) async fn probe_proxy(
    url: &Url,
    hosts: &[(&'static str, u16)],
) -> (ProxyCheck, Resolved) {
    let relay = match crate::socks::associate(url).await {
        Ok(relay) => relay,
        Err(check) => return (check, Resolved::default()),
    };
    let udp = match Udp::via(relay).await {
        Ok(Ok(udp)) => udp,
        Ok(Err(why)) => return (ProxyCheck::Silent(why), Resolved::default()),
        Err(error) => {
            return (
                ProxyCheck::Silent(format!("开不了 UDP 套接字：{error}")),
                Resolved::default(),
            );
        }
    };
    let (found, unresolved) = bootstrap(hosts, false).await;
    let requests: Vec<_> = found.iter().map(|to| (*to, krpc("find_node"))).collect();
    let exchanged = udp.exchange(&requests, is_krpc_reply).await;
    let through = Ok(Dht {
        bootstrap: Ratio {
            asked: found.len(),
            replied: exchanged.replies.len(),
        },
        unresolved,
        nodes: Ratio::default(),
        external: None,
        tun: found.iter().any(fake_ip),
        real: found.iter().any(|address| !fake_ip(address)),
        failure: exchanged.failure,
    });
    (judge_proxy(&through), resolved(&through))
}

/// 198.18.0.0/15 里的地址：TUN 代理（fake-ip）给域名的假地址。
fn fake_ip(address: &SocketAddr) -> bool {
    match address.ip() {
        IpAddr::V4(ip) => ip.octets()[0] == 198 && (ip.octets()[1] & 0xfe) == 18,
        IpAddr::V6(_) => false,
    }
}

/// 照量到的下结论。
pub(crate) fn report(measured: &Measured) -> NetworkReport {
    let check = |item, level, text: String| NetCheck { item, level, text };
    let dht = &measured.dht;
    // 收发出错时回应数不可信
    let direct = if dht.failure.is_some() {
        Health::Uncertain
    } else {
        dht.health()
    };
    let tun = if dht.tun {
        "；引导节点解析成了 198.18 开头的地址，系统里的 TUN 代理接管了，这里的「直连」其实经它"
    } else {
        ""
    };
    let mut checks = vec![match direct {
        _ if dht.failure.is_some() => check(
            NetItem::Dht,
            CheckLevel::Bad,
            format!(
                "直连的 DHT 没测完：{}",
                dht.failure.as_deref().unwrap_or_default()
            ),
        ),
        Health::Untested => check(
            NetItem::Dht,
            CheckLevel::Warn,
            "解析不出 DHT 引导节点的地址（DNS 可能有问题），没测 DHT".to_owned(),
        ),
        Health::Healthy => check(
            NetItem::Dht,
            CheckLevel::Ok,
            format!(
                "直连的 DHT 正常：{} 个节点里 {} 个回应{tun}",
                dht.nodes.asked, dht.nodes.replied
            ),
        ),
        Health::Uncertain => check(
            NetItem::Dht,
            CheckLevel::Warn,
            format!(
                "直连的 DHT 说不准：{} 个节点回 {} 个（引导节点 {} 个回 {} 个），可再测一次{tun}",
                dht.nodes.asked, dht.nodes.replied, dht.bootstrap.asked, dht.bootstrap.replied
            ),
        ),
        Health::Blocked => check(
            NetItem::Dht,
            CheckLevel::Bad,
            format!(
                "直连的 DHT 大多不通：{} 个节点回 {} 个（引导节点 {} 个回 {} 个），UDP 多半被运营商或路由器限制{tun}",
                dht.nodes.asked, dht.nodes.replied, dht.bootstrap.asked, dht.bootstrap.replied
            ),
        ),
    }];
    checks.push(match &measured.trackers {
        Err(why) => check(
            NetItem::UdpTrackers,
            CheckLevel::Warn,
            format!("UDP 的 tracker 没测成：{why}"),
        ),
        Ok(ratio) if ratio.asked == 0 => check(
            NetItem::UdpTrackers,
            CheckLevel::Warn,
            "公共 tracker 列表里没有写成 IP 地址的 UDP tracker，没测".to_owned(),
        ),
        Ok(ratio) if ratio.replied > 0 => check(
            NetItem::UdpTrackers,
            CheckLevel::Ok,
            format!(
                "UDP 的 tracker {} 个里 {} 个回应（列表里常有已下线的）",
                ratio.asked, ratio.replied
            ),
        ),
        Ok(ratio) => check(
            NetItem::UdpTrackers,
            CheckLevel::Bad,
            format!("UDP 的 tracker {} 个都没回", ratio.asked),
        ),
    });
    checks.push(match &measured.ipv6 {
        None => check(NetItem::Ipv6, CheckLevel::Ok, "没有公网 IPv6".to_owned()),
        Some(Err(why)) => check(
            NetItem::Ipv6,
            CheckLevel::Warn,
            format!("有公网 IPv6，IPv6 的 UDP 没测完：{why}"),
        ),
        Some(Ok(ratio)) if ratio.asked == 0 => check(
            NetItem::Ipv6,
            CheckLevel::Warn,
            "有公网 IPv6，但解析不出引导节点的 IPv6 地址（DNS 可能滤掉了），没测 IPv6 的 UDP"
                .to_owned(),
        ),
        Some(Ok(ratio)) if ratio.replied > 0 => check(
            NetItem::Ipv6,
            CheckLevel::Ok,
            "有公网 IPv6，IPv6 的 DHT 通".to_owned(),
        ),
        Some(Ok(_)) => check(
            NetItem::Ipv6,
            CheckLevel::Warn,
            "有公网 IPv6，但 IPv6 的 UDP 发出去没有回应".to_owned(),
        ),
    });
    // 经代理的：用不了的为大多不通；握手正常而引导节点都解析不出的为没测
    let proxied = measured.proxy.as_ref().map(|proxy| {
        let health = match (&proxy.check, &proxy.dht) {
            (check, _) if check.problem().is_some() => Health::Blocked,
            (_, Some(through)) => through.health(),
            (_, None) => Health::Blocked,
        };
        (proxy, health)
    });
    checks.push(match &proxied {
        None => check(NetItem::Proxy, CheckLevel::Ok, "没设 BT 代理".to_owned()),
        Some((proxy, health)) => match (proxy.check.problem(), &proxy.dht, health) {
            (_, Some(through), _) if drops_udp(dht, through) => check(
                NetItem::Proxy,
                CheckLevel::Bad,
                format!("BT 代理 {}：{DROPS_UDP}", proxy.address),
            ),
            (Some(RESTART_AFTER_FAKE_IP), _, _) => check(
                NetItem::Proxy,
                CheckLevel::Bad,
                format!(
                    "BT 代理 {} 是通的，但 BT 还用着之前解析到的 DHT 引导节点的假地址：重启 hs-dlx 后生效",
                    proxy.address
                ),
            ),
            (Some(why), _, _) => check(
                NetItem::Proxy,
                CheckLevel::Bad,
                format!("BT 代理 {} 用不了：{why}", proxy.address),
            ),
            (None, _, Health::Untested) => check(
                NetItem::Proxy,
                CheckLevel::Warn,
                format!(
                    "BT 代理 {} 握手正常，但解析不出 DHT 引导节点的地址，没测它转发的 UDP",
                    proxy.address
                ),
            ),
            (None, Some(through), Health::Healthy) => check(
                NetItem::Proxy,
                CheckLevel::Ok,
                format!(
                    "经 BT 代理 {} 的 DHT 正常：{} 个节点里 {} 个回应",
                    proxy.address, through.nodes.asked, through.nodes.replied
                ),
            ),
            (_, through, _) => {
                let through = through.clone().unwrap_or_default();
                check(
                    NetItem::Proxy,
                    CheckLevel::Warn,
                    format!(
                        "经 BT 代理 {} 的 DHT 说不准或大多不通：{} 个节点回 {} 个",
                        proxy.address, through.nodes.asked, through.nodes.replied
                    ),
                )
            }
        },
    });
    // 出口 IP 与路由器的外网 IPv4 一比即知是不是运营商级 NAT；经 TUN 代理时量到的是代理的出口，不比
    let nat_hint = match dht.external {
        Some(ip) if !dht.tun => format!(
            "；DHT 节点看到的出口 IP 为 {ip}：没开 VPN 或全局代理时，路由器管理页里的外网 IPv4 与它不同即是运营商级 NAT，映射也没用"
        ),
        _ => String::new(),
    };
    checks.push(match (&measured.proxy, &measured.session) {
        (Some(_), _) => check(
            NetItem::Incoming,
            CheckLevel::Ok,
            "经 BT 代理时不映射端口、不接受外部连入".to_owned(),
        ),
        (None, None) => check(
            NetItem::Incoming,
            CheckLevel::Warn,
            "BT 还没用过，没测监听、端口映射与外部连入".to_owned(),
        ),
        (None, Some(session)) => {
            match (&session.listen_error, session.incoming, &session.mapping) {
                (Some(why), _, _) => check(
                    NetItem::Incoming,
                    CheckLevel::Bad,
                    format!("BT 没在监听（{why}）：外面连不进来"),
                ),
                (None, true, _) => check(
                    NetItem::Incoming,
                    CheckLevel::Ok,
                    "有节点从外面连进来过".to_owned(),
                ),
                (None, false, Mapping::Mapped(port)) => check(
                    NetItem::Incoming,
                    CheckLevel::Ok,
                    format!("路由器已映射端口（外部 {port}），还没有节点连进来{nat_hint}"),
                ),
                (None, false, Mapping::Pending) => check(
                    NetItem::Incoming,
                    CheckLevel::Warn,
                    "路由器还没回应端口映射".to_owned(),
                ),
                (None, false, Mapping::Failed(why)) => check(
                    NetItem::Incoming,
                    CheckLevel::Warn,
                    format!("路由器没映射端口（{why}），外面连不进来{nat_hint}"),
                ),
            }
        }
    });
    let verdict = match &proxied {
        _ if dht.failure.is_some() => "直连的 DHT 没测完：可再测一次".to_owned(),
        Some((proxy, _)) if proxy.check.problem() == Some(RESTART_AFTER_FAKE_IP) => {
            "重启 hs-dlx：BT 还用着之前解析到的假地址，见上面「BT 代理」一项".to_owned()
        }
        Some((proxy, _)) if proxy.check.problem() == Some(PROXY_FAKE_IP.as_str()) => {
            "经 BT 代理的 DHT 不通，与系统里的 TUN 代理有关：见上面「BT 代理」一项".to_owned()
        }
        None => match direct {
            Health::Healthy => "网络正常：BT 能直接连".to_owned(),
            Health::Uncertain => "结果说不准：可再测一次".to_owned(),
            Health::Blocked => {
                "直连的 UDP 大多不通：DHT 用不了，公开网络里人少的磁力很难取到种子信息。可在「BT 代理」设能转发 UDP 的 SOCKS5 代理"
                    .to_owned()
            }
            Health::Untested => "解析不出 DHT 引导节点：先检查网络与 DNS".to_owned(),
        },
        Some((proxy, health)) => {
            // 改法：没转发 UDP 时换节点或打开 UDP；连不上、被拒时看上面那一项写的原因
            let fix = if matches!(
                proxy.check,
                ProxyCheck::Unreachable(_) | ProxyCheck::Refused(_)
            ) {
                "见上面「BT 代理」一项"
            } else {
                "换节点或在代理里打开 UDP"
            };
            match (health, direct) {
            (Health::Healthy, Health::Healthy) => {
                "经 BT 代理正常；直连也正常，可关掉 BT 代理（经代理时外面连不进来）".to_owned()
            }
            (Health::Healthy, _) => "直连受限，经 BT 代理正常：保持开着 BT 代理".to_owned(),
            (Health::Uncertain, _) => "经 BT 代理的结果说不准：可再测一次".to_owned(),
            (Health::Untested, _) => "BT 代理握手正常，经它的 UDP 没测成：见上面「BT 代理」一项".to_owned(),
            (_, Health::Healthy) => {
                format!("BT 现在经的代理不通，直连正常：关掉 BT 代理，或修好它（{fix}）")
            }
            (_, Health::Uncertain) => format!(
                "BT 现在经的代理不通；直连的结果说不准：先修好代理（{fix}），或关掉它再测一次直连"
            ),
            (_, Health::Blocked) => {
                format!("BT 代理不通，直连的 UDP 也大多不通：先修好代理（{fix}）")
            }
            (_, Health::Untested) => "BT 代理不通，也解析不出 DHT 引导节点：先检查网络与 DNS".to_owned(),
            }
        }
    };
    NetworkReport { checks, verdict }
}

/// `N` 个随机字节：节点 ID、查找目标与事务号，只用于这次探测。用标准库给每个哈希器的随机键，不需要密码学强度。
fn random<const N: usize>() -> [u8; N] {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut out = [0; N];
    for chunk in out.chunks_mut(8) {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        let bytes = hasher.finish().to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    out
}

/// 发 UDP 包的通道：直连，或经 SOCKS5 代理转发。
pub(crate) struct Udp {
    socket: UdpSocket,
    relay: Option<UdpRelay>,
}

impl Udp {
    /// 直连的 IPv4 或 IPv6 通道。
    pub async fn direct(v6: bool) -> std::io::Result<Udp> {
        let any: IpAddr = if v6 {
            Ipv6Addr::UNSPECIFIED.into()
        } else {
            Ipv4Addr::UNSPECIFIED.into()
        };
        Ok(Udp {
            socket: UdpSocket::bind((any, 0)).await?,
            relay: None,
        })
    }

    /// 经已谈好的 UDP 转发：套接字连到转发地址，只收它发来的，转发端口没开时收包即报错。开不了本机的套接字时为外层的
    /// 错；连不到转发地址（没有到它的路由等）时内层为原因。
    pub async fn via(relay: UdpRelay) -> std::io::Result<Result<Udp, String>> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
        if let Err(error) = socket.connect(relay.address).await {
            return Ok(Err(format!(
                "发不到代理的 UDP 转发地址 {}：{error}",
                relay.address
            )));
        }
        Ok(Ok(Udp {
            socket,
            relay: Some(relay),
        }))
    }

    /// 发一个包。直连时发往单个节点发不出（网络不可达等）算那个节点没回，不报错；经代理时都发往代理的转发地址，发不
    /// 出即经不了代理，为原因。
    async fn send(&self, to: SocketAddr, data: &[u8]) -> Result<(), String> {
        match (&self.relay, to) {
            (None, _) => {
                let _unreachable = self.socket.send_to(data, to).await;
                Ok(())
            }
            (Some(relay), SocketAddr::V4(to)) => {
                let mut wrapped = vec![0, 0, 0, 1];
                wrapped.extend_from_slice(&to.ip().octets());
                wrapped.extend_from_slice(&to.port().to_be_bytes());
                wrapped.extend_from_slice(data);
                self.socket.send(&wrapped).await.map(drop).map_err(|error| {
                    format!("发不到代理的 UDP 转发地址 {}：{error}", relay.address)
                })
            }
            // 经代理只转发 IPv4（libtorrent 同样只用 IPv4）
            (Some(_), SocketAddr::V6(_)) => Ok(()),
        }
    }

    /// 收一个包，返回内容与真正的来源；经代理时去掉 SOCKS5 的头，认不出的包丢掉接着收。直连时之前发往已下线的地址回来
    /// 的「端口不可达」（Windows 上未连接的 UDP 套接字照样报给收的一方）不算失败，接着收；经代理时它只能是代理的转发
    /// 端口没开，报错。
    async fn recv(&self, buffer: &mut [u8]) -> std::io::Result<(Vec<u8>, SocketAddr)> {
        loop {
            let (len, from) = match self.socket.recv_from(buffer).await {
                Ok(received) => received,
                Err(error)
                    if self.relay.is_none()
                        && matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionRefused
                        ) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            let datagram = &buffer[..len];
            if self.relay.is_none() {
                return Ok((datagram.to_vec(), from));
            }
            if datagram.len() > 10 && datagram[3] == 1 {
                let ip = Ipv4Addr::new(datagram[4], datagram[5], datagram[6], datagram[7]);
                let port = u16::from_be_bytes([datagram[8], datagram[9]]);
                return Ok((datagram[10..].to_vec(), SocketAddr::new(ip.into(), port)));
            }
        }
    }

    /// 发出 `requests`（地址与内容），在 [`REPLY_WAIT`] 内收各地址的回应（每个地址算一次，`accept` 认的才算）；没回的
    /// 再发一次、再等 [`REPLY_WAIT`]。发（见 [`Udp::send`]）或收出错即停，记下原因。
    async fn exchange(
        &self,
        requests: &[(SocketAddr, Vec<u8>)],
        accept: impl Fn(&[u8]) -> bool,
    ) -> Exchanged {
        let mut replies = Vec::new();
        let mut seen = HashSet::new();
        let mut buffer = vec![0; 65536];
        for _round in 0..2 {
            for (to, data) in requests.iter().filter(|(to, _)| !seen.contains(to)) {
                if let Err(why) = self.send(*to, data).await {
                    return Exchanged {
                        replies,
                        failure: Some(why),
                    };
                }
            }
            let deadline = tokio::time::Instant::now() + REPLY_WAIT;
            while seen.len() < requests.len() {
                let (data, from) =
                    match tokio::time::timeout_at(deadline, self.recv(&mut buffer)).await {
                        Ok(Ok(received)) => received,
                        Ok(Err(error)) => {
                            let failure = match &self.relay {
                                Some(relay)
                                    if matches!(
                                        error.kind(),
                                        std::io::ErrorKind::ConnectionReset
                                            | std::io::ErrorKind::ConnectionRefused
                                    ) =>
                                {
                                    format!("代理回的 UDP 转发地址 {} 没开", relay.address)
                                }
                                _ => format!("收 UDP 包出错：{error}"),
                            };
                            return Exchanged {
                                replies,
                                failure: Some(failure),
                            };
                        }
                        Err(_) => break,
                    };
                let wanted = requests.iter().any(|(to, _)| *to == from);
                if wanted && accept(&data) && seen.insert(from) {
                    replies.push((from, data));
                }
            }
        }
        Exchanged {
            replies,
            failure: None,
        }
    }
}

/// 一次收发的结果。
struct Exchanged {
    /// 回了的地址与回应
    replies: Vec<(SocketAddr, Vec<u8>)>,
    /// 发或收出的错；有时 `replies` 只是出错之前收到的
    failure: Option<String>,
}

/// 并行解析各引导节点的地址（各最多等 [`RESOLVE_TIMEOUT`]）：`v6` 时取 IPv6 的（不含 IPv4 映射的），否则取 IPv4 的。
/// 返回解析出的与解析不出的个数。
pub(crate) async fn bootstrap(hosts: &[(&'static str, u16)], v6: bool) -> (Vec<SocketAddr>, usize) {
    let mut lookups = tokio::task::JoinSet::new();
    for &(host, port) in hosts {
        lookups.spawn(async move {
            let found =
                tokio::time::timeout(RESOLVE_TIMEOUT, tokio::net::lookup_host((host, port)))
                    .await
                    .ok()?
                    .ok()?;
            found.into_iter().find(|address| match address {
                SocketAddr::V4(_) => !v6,
                SocketAddr::V6(v6_address) => v6 && v6_address.ip().to_ipv4_mapped().is_none(),
            })
        });
    }
    let mut found = Vec::new();
    let mut unresolved = 0;
    while let Some(result) = lookups.join_next().await {
        match result.expect("解析不会 panic，本函数返回前不取消") {
            Some(address) => found.push(address),
            None => unresolved += 1,
        }
    }
    (found, unresolved)
}

/// DHT 的请求（KRPC）：`query` 为 `ping` 或 `find_node`（带随机的目标），节点 ID 与事务号随机。
fn krpc(query: &str) -> Vec<u8> {
    let mut message = b"d1:ad2:id20:".to_vec();
    message.extend(random::<20>());
    if query == "find_node" {
        message.extend_from_slice(b"6:target20:");
        message.extend(random::<20>());
    }
    message.extend_from_slice(format!("e1:q{}:{query}1:t2:", query.len()).as_bytes());
    message.extend(random::<2>());
    message.extend_from_slice(b"1:y1:qe");
    message
}

/// 是 KRPC 的回应（y 为 r）。
fn is_krpc_reply(data: &[u8]) -> bool {
    bencode::decode(data)
        .is_some_and(|message| message.get(b"y").and_then(bencode::Value::bytes) == Some(b"r"))
}

/// 经 `udp` 探 DHT：向解析得出的引导节点 `bootstrap` 要节点，再 ping 它们给的（最多 [`NODES_PINGED`] 个）。
/// `unresolved` 为解析不出的引导节点个数。
pub(crate) async fn dht(udp: &Udp, bootstrap: &[SocketAddr], unresolved: usize) -> Dht {
    let requests: Vec<_> = bootstrap
        .iter()
        .map(|to| (*to, krpc("find_node")))
        .collect();
    let Exchanged { replies, failure } = udp.exchange(&requests, is_krpc_reply).await;
    let mut seen_ips: HashMap<IpAddr, usize> = HashMap::new();
    let mut nodes = Vec::new();
    for (_, data) in &replies {
        let Some(message) = bencode::decode(data) else {
            continue;
        };
        if let Some(ip) = message.get(b"ip").and_then(external_ip) {
            *seen_ips.entry(ip).or_default() += 1;
        }
        if let Some(compact) = message
            .get(b"r")
            .and_then(|r| r.get(b"nodes"))
            .and_then(bencode::Value::bytes)
        {
            for node in compact.as_chunks::<26>().0 {
                let ip = Ipv4Addr::new(node[20], node[21], node[22], node[23]);
                let address = SocketAddr::new(ip.into(), u16::from_be_bytes([node[24], node[25]]));
                if !nodes.contains(&address) && !bootstrap.contains(&address) {
                    nodes.push(address);
                }
            }
        }
    }
    nodes.truncate(NODES_PINGED);
    let requests: Vec<_> = nodes.iter().map(|to| (*to, krpc("ping"))).collect();
    let pinged = if failure.is_none() {
        udp.exchange(&requests, is_krpc_reply).await
    } else {
        Exchanged {
            replies: Vec::new(),
            failure: None,
        }
    };
    let failure = failure.or(pinged.failure);
    let pinged = pinged.replies;
    for (_, data) in &pinged {
        if let Some(ip) =
            bencode::decode(data).and_then(|message| message.get(b"ip").and_then(external_ip))
        {
            *seen_ips.entry(ip).or_default() += 1;
        }
    }
    let external = seen_ips
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .max_by_key(|(_, count)| *count)
        .map(|(ip, _)| ip);
    Dht {
        bootstrap: Ratio {
            asked: bootstrap.len(),
            replied: replies.len(),
        },
        unresolved,
        nodes: Ratio {
            asked: nodes.len(),
            replied: pinged.len(),
        },
        external,
        tun: bootstrap.iter().any(fake_ip),
        real: bootstrap.iter().any(|address| !fake_ip(address)),
        failure,
    }
}

/// BEP 42 的 ip：IPv4 或 IPv6 地址，后跟端口。
fn external_ip(value: &bencode::Value) -> Option<IpAddr> {
    match value.bytes()? {
        [a, b, c, d, _, _] => Some(Ipv4Addr::new(*a, *b, *c, *d).into()),
        compact if compact.len() == 18 => {
            let octets: [u8; 16] = compact[..16].try_into().ok()?;
            Some(Ipv6Addr::from(octets).into())
        }
        _ => None,
    }
}

/// 对 `trackers` 里 udp:// 开头、主机为 IP 的做 BEP 15 的 connect（事务号随机），看回不回；收发出错时为原因。
pub(crate) async fn udp_trackers(udp: &Udp, trackers: &[String]) -> Result<Ratio, String> {
    let addresses: Vec<SocketAddr> = trackers
        .iter()
        .filter_map(|tracker| Url::parse(tracker).ok())
        .filter(|url| url.scheme() == "udp")
        .filter_map(|url| Some(SocketAddr::new(url.host_str()?.parse().ok()?, url.port()?)))
        .collect();
    let transaction = random::<4>();
    let mut connect = 0x0417_2710_1980_u64.to_be_bytes().to_vec();
    connect.extend_from_slice(&0_u32.to_be_bytes());
    connect.extend_from_slice(&transaction);
    let requests: Vec<_> = addresses.iter().map(|to| (*to, connect.clone())).collect();
    let exchanged = udp
        .exchange(&requests, |data| {
            data.len() >= 16 && data[..4] == [0, 0, 0, 0] && data[4..8] == transaction
        })
        .await;
    match exchanged.failure {
        Some(why) => Err(why),
        None => Ok(Ratio {
            asked: addresses.len(),
            replied: exchanged.replies.len(),
        }),
    }
}

/// 本机有没有公网 IPv6 地址（2000::/3 里的）：向一个公网地址「连接」UDP 套接字（不发包）看系统选的源地址。
pub(crate) async fn has_public_ipv6() -> bool {
    let Ok(socket) = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).await else {
        return false;
    };
    let target = (
        Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888),
        53,
    );
    if socket.connect(target).await.is_err() {
        return false;
    }
    matches!(
        socket.local_addr().map(|address| address.ip()),
        Ok(IpAddr::V6(address)) if address.segments()[0] & 0xe000 == 0x2000
    )
}

/// 只认 DHT 回应要用到的那点 bencode：整数、字节串、列表、字典。
mod bencode {
    #[derive(Debug, PartialEq, Eq)]
    pub enum Value {
        Int(i64),
        Bytes(Vec<u8>),
        List(Vec<Value>),
        Dict(Vec<(Vec<u8>, Value)>),
    }

    impl Value {
        pub fn get(&self, key: &[u8]) -> Option<&Value> {
            match self {
                Value::Dict(entries) => entries
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value),
                _ => None,
            }
        }

        pub fn bytes(&self) -> Option<&[u8]> {
            match self {
                Value::Bytes(bytes) => Some(bytes),
                _ => None,
            }
        }
    }

    /// 解出整段；不是一个完整的 bencode 值（或后面还有多余的字节）时为 None。
    pub fn decode(data: &[u8]) -> Option<Value> {
        let (value, rest) = value(data, 0)?;
        rest.is_empty().then_some(value)
    }

    /// 嵌套最多这么深：DHT 的回应只有两三层，防恶意的包把栈撑爆。
    const DEPTH: usize = 16;

    fn value(data: &[u8], depth: usize) -> Option<(Value, &[u8])> {
        if depth > DEPTH {
            return None;
        }
        match data.first()? {
            b'i' => {
                let end = data.iter().position(|&b| b == b'e')?;
                let number = std::str::from_utf8(&data[1..end]).ok()?.parse().ok()?;
                Some((Value::Int(number), &data[end + 1..]))
            }
            b'l' => {
                let mut rest = &data[1..];
                let mut items = Vec::new();
                while *rest.first()? != b'e' {
                    let (item, next) = value(rest, depth + 1)?;
                    items.push(item);
                    rest = next;
                }
                Some((Value::List(items), &rest[1..]))
            }
            b'd' => {
                let mut rest = &data[1..];
                let mut entries = Vec::new();
                while *rest.first()? != b'e' {
                    let (Value::Bytes(key), next) = value(rest, depth + 1)? else {
                        return None;
                    };
                    let (item, next) = value(next, depth + 1)?;
                    entries.push((key, item));
                    rest = next;
                }
                Some((Value::Dict(entries), &rest[1..]))
            }
            b'0'..=b'9' => {
                let colon = data.iter().position(|&b| b == b':')?;
                let len: usize = std::str::from_utf8(&data[..colon]).ok()?.parse().ok()?;
                let start = colon + 1;
                let bytes = data.get(start..start.checked_add(len)?)?;
                Some((Value::Bytes(bytes.to_vec()), &data[start + len..]))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ratio(asked: usize, replied: usize) -> Ratio {
        Ratio { asked, replied }
    }

    fn dht_of(bootstrap: Ratio, nodes: Ratio) -> Dht {
        Dht {
            bootstrap,
            unresolved: 0,
            nodes,
            external: Some(Ipv4Addr::new(203, 0, 113, 7).into()),
            tun: false,
            real: true,
            failure: None,
        }
    }

    fn healthy() -> Measured {
        Measured {
            dht: dht_of(ratio(4, 3), ratio(32, 14)),
            trackers: Ok(ratio(17, 12)),
            ipv6: None,
            proxy: None,
            session: Some(Session {
                listen_error: None,
                mapping: Mapping::Mapped(51413),
                incoming: false,
            }),
        }
    }

    /// 这台机器上量到的：普通节点几乎不回、引导节点只回一个，IPv6 有地址而 UDP 不通，路由器不映射。
    fn restricted() -> Measured {
        let mut measured = healthy();
        measured.dht = dht_of(ratio(4, 1), ratio(32, 1));
        measured.dht.external = Some(Ipv4Addr::new(120, 229, 78, 85).into());
        measured.trackers = Ok(ratio(17, 4));
        measured.ipv6 = Some(Ok(ratio(2, 0)));
        measured.session = Some(Session {
            listen_error: None,
            mapping: Mapping::Failed("no router".into()),
            incoming: false,
        });
        measured
    }

    fn proxied(check: ProxyCheck, dht: Option<Dht>) -> Option<Proxied> {
        Some(Proxied {
            address: "socks5://127.0.0.1:7897".into(),
            check,
            dht,
        })
    }

    fn level(report: &NetworkReport, item: NetItem) -> CheckLevel {
        report.checks.iter().find(|c| c.item == item).unwrap().level
    }

    fn text(report: &NetworkReport, item: NetItem) -> String {
        report
            .checks
            .iter()
            .find(|c| c.item == item)
            .unwrap()
            .text
            .clone()
    }

    #[test]
    fn health_needs_enough_replies_and_samples() {
        assert_eq!(dht_of(ratio(4, 3), ratio(32, 8)).health(), Health::Healthy);
        assert_eq!(dht_of(ratio(4, 1), ratio(32, 3)).health(), Health::Blocked);
        // 普通节点回得少而引导节点大多回了、样本太少、介于两者之间：说不准
        assert_eq!(
            dht_of(ratio(4, 4), ratio(32, 2)).health(),
            Health::Uncertain
        );
        assert_eq!(dht_of(ratio(4, 2), ratio(4, 2)).health(), Health::Uncertain);
        assert_eq!(
            dht_of(ratio(4, 2), ratio(32, 5)).health(),
            Health::Uncertain
        );
        assert_eq!(dht_of(ratio(0, 0), ratio(0, 0)).health(), Health::Untested);
    }

    #[test]
    fn a_healthy_network_is_said_to_be_fine() {
        let report = report(&healthy());
        assert!(report.checks.iter().all(|c| c.level == CheckLevel::Ok));
        assert!(report.verdict.contains("网络正常"));
    }

    #[test]
    fn restricted_udp_points_to_the_proxy() {
        let plain = report(&restricted());
        assert_eq!(level(&plain, NetItem::Dht), CheckLevel::Bad);
        assert_eq!(level(&plain, NetItem::Ipv6), CheckLevel::Warn);
        assert!(!text(&plain, NetItem::Ipv6).contains("关掉"));
        let incoming = text(&plain, NetItem::Incoming);
        assert!(incoming.contains("120.229.78.85") && incoming.contains("运营商级 NAT"));
        assert!(plain.verdict.contains("BT 代理"));
    }

    #[test]
    fn verdicts_follow_whether_bt_goes_through_the_proxy() {
        let good = dht_of(ratio(4, 2), ratio(32, 10));
        // 直连受限、经代理正常
        let mut measured = restricted();
        measured.proxy = proxied(ProxyCheck::Usable, Some(good.clone()));
        assert!(report(&measured).verdict.contains("保持开着 BT 代理"));
        // 直连也正常：可关掉
        let mut both = healthy();
        both.proxy = proxied(ProxyCheck::Usable, Some(good));
        assert!(report(&both).verdict.contains("可关掉 BT 代理"));
        // 代理用不了、直连正常：不能说「网络正常」
        let mut broken = healthy();
        broken.proxy = proxied(ProxyCheck::Unreachable("连不上代理".into()), None);
        let broken = report(&broken);
        assert_eq!(level(&broken, NetItem::Proxy), CheckLevel::Bad);
        assert!(
            broken.verdict.contains("关掉 BT 代理"),
            "{}",
            broken.verdict
        );
        // 代理用不了、直连说不准：不说直连大多不通
        let mut unsure = healthy();
        unsure.dht = dht_of(ratio(4, 2), ratio(4, 1));
        unsure.proxy = proxied(ProxyCheck::Unreachable("连不上代理".into()), None);
        let unsure = report(&unsure);
        assert!(!unsure.verdict.contains("大多不通"), "{}", unsure.verdict);
        // 代理接了 UDP 转发却没转发出去
        let mut dropping = restricted();
        dropping.proxy = proxied(ProxyCheck::Usable, Some(dht_of(ratio(4, 0), ratio(0, 0))));
        let dropping = report(&dropping);
        assert!(text(&dropping, NetItem::Proxy).contains("没转发出去"));
        assert!(dropping.verdict.contains("先修好代理"));
    }

    #[test]
    fn unknowns_are_not_blamed_on_the_network() {
        // DNS 解析不出：不说 UDP 被挡
        let mut no_dns = healthy();
        no_dns.dht = Dht {
            unresolved: 4,
            ..Dht::default()
        };
        let no_dns = report(&no_dns);
        assert!(text(&no_dns, NetItem::Dht).contains("DNS"));
        assert!(no_dns.verdict.contains("DNS"));
        // 没测到出口 IP：不写运营商级 NAT 的比较
        let mut unknown = restricted();
        unknown.dht.external = None;
        assert!(!text(&report(&unknown), NetItem::Incoming).contains("运营商级 NAT"));
        // 经 TUN 代理：不拿出口 IP 下结论
        let mut tun = restricted();
        tun.dht.tun = true;
        let tun = report(&tun);
        assert!(text(&tun, NetItem::Dht).contains("TUN"));
        assert!(!text(&tun, NetItem::Incoming).contains("运营商级 NAT"));
        // 公共 tracker 列表没取到：写明原因，不说「BT 用过之后才取」
        let mut failed = healthy();
        failed.trackers = Err("公共 tracker 列表没取到（连不上）".into());
        assert!(text(&report(&failed), NetItem::UdpTrackers).contains("连不上"));
        // 经 TUN 的假地址经别的代理到不了：不说代理没转发
        let mut proxied_tun = dht_of(ratio(4, 0), ratio(0, 0));
        proxied_tun.tun = true;
        assert!(!drops_udp(&healthy().dht, &proxied_tun));
        // 收发出错：不说网络正常，写明原因
        let mut broken = healthy();
        broken.dht.failure = Some("收 UDP 包出错：boom".into());
        let broken = report(&broken);
        assert!(text(&broken, NetItem::Dht).contains("boom"));
        assert!(!broken.verdict.contains("网络正常"));
        // BT 没用过、监听失败
        let mut fresh = healthy();
        fresh.session = None;
        assert!(text(&report(&fresh), NetItem::Incoming).contains("没用过"));
        fresh.session = Some(Session {
            listen_error: Some("address in use".into()),
            mapping: Mapping::Pending,
            incoming: false,
        });
        assert_eq!(level(&report(&fresh), NetItem::Incoming), CheckLevel::Bad);
    }

    #[test]
    fn the_engine_records_only_what_went_through_the_proxy() {
        let silent = dht_of(ratio(4, 0), ratio(0, 0));
        assert_eq!(
            judge_proxy(&Ok(silent.clone())),
            ProxyCheck::Silent(PROXY_SILENT.into())
        );
        assert_eq!(
            judge_proxy(&Ok(dht_of(ratio(4, 1), ratio(0, 0)))),
            ProxyCheck::Usable
        );
        // 问不成（解析不出）：按握手算
        assert_eq!(
            judge_proxy(&Ok(dht_of(ratio(0, 0), ratio(0, 0)))),
            ProxyCheck::Usable
        );
        // 解析成 TUN 的假地址而没回：BT 同样照这些地址问，用不了，写明怎么改
        let mut tun = silent.clone();
        tun.tun = true;
        assert_eq!(
            judge_proxy(&Ok(tun.clone())),
            ProxyCheck::Silent(PROXY_FAKE_IP.clone())
        );
        let mut report_of_tun = healthy();
        report_of_tun.proxy = proxied(ProxyCheck::Silent(PROXY_FAKE_IP.clone()), Some(tun));
        assert!(text(&report(&report_of_tun), NetItem::Proxy).contains("fake-ip"));
        let mut unsent = silent;
        unsent.failure = Some("发不到代理的 UDP 转发地址".into());
        assert_eq!(
            judge_proxy(&Ok(unsent)),
            ProxyCheck::Silent("发不到代理的 UDP 转发地址".into())
        );
        assert_eq!(
            judge_proxy(&Err(ProxyCheck::Unreachable("连不上代理".into()))),
            ProxyCheck::Unreachable("连不上代理".into())
        );
    }

    #[test]
    fn bencode_reads_what_dht_replies_carry() {
        let reply =
            b"d2:ip6:\x78\xe5\x4e\x55\x1a\xe11:rd2:id20:aaaaaaaaaaaaaaaaaaaa5:nodes0:e1:t2:hs1:y1:re";
        let message = bencode::decode(reply).unwrap();
        assert_eq!(
            message.get(b"ip").and_then(external_ip),
            Some(Ipv4Addr::new(120, 229, 78, 85).into())
        );
        assert!(is_krpc_reply(reply));
        assert_eq!(bencode::decode(b"d1:a"), None);
        assert_eq!(bencode::decode(b"i1ee"), None);
        assert_eq!(bencode::decode(b"99999999999999999999:x"), None);
        let deep = format!("{}{}", "l".repeat(64), "e".repeat(64));
        assert_eq!(bencode::decode(deep.as_bytes()), None);
        assert_ne!(krpc("ping"), krpc("ping"));
    }

    /// 本机的假 DHT 节点：回 find_node 时给出 `nodes`，回 ping；`silent` 的不回。
    async fn fake_node(nodes: Vec<SocketAddr>, silent: bool) -> SocketAddr {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = socket.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buffer = [0; 2048];
            while let Ok((len, from)) = socket.recv_from(&mut buffer).await {
                if silent {
                    continue;
                }
                let mut compact = Vec::new();
                for node in &nodes {
                    let SocketAddr::V4(node) = node else { continue };
                    compact.extend_from_slice(&[b'n'; 20]);
                    compact.extend_from_slice(&node.ip().octets());
                    compact.extend_from_slice(&node.port().to_be_bytes());
                }
                let find = buffer[..len].windows(9).any(|w| w == b"find_node");
                let mut reply =
                    b"d2:ip6:\x7f\x00\x00\x01\x00\x011:rd2:id20:bbbbbbbbbbbbbbbbbbbb".to_vec();
                if find {
                    reply.extend_from_slice(format!("5:nodes{}:", compact.len()).as_bytes());
                    reply.extend_from_slice(&compact);
                }
                reply.extend_from_slice(b"e1:t2:hs1:y1:re");
                let _sent = socket.send_to(&reply, from).await;
            }
        });
        address
    }

    /// 一个引导节点，给出四个普通节点，其中两个回。
    async fn fake_network() -> SocketAddr {
        let mut nodes = Vec::new();
        for silent in [false, false, true, true] {
            nodes.push(fake_node(Vec::new(), silent).await);
        }
        fake_node(nodes, false).await
    }

    #[tokio::test]
    async fn dht_probe_counts_who_answers() {
        let router = fake_network().await;
        let udp = Udp::direct(false).await.unwrap();
        let measured = dht(&udp, &[router], 1).await;
        assert_eq!(measured.bootstrap, ratio(1, 1));
        assert_eq!(measured.unresolved, 1);
        assert_eq!(measured.nodes, ratio(4, 2));
        // 引导节点与两个回了的节点都说是 127.0.0.1
        assert_eq!(measured.external, Some(Ipv4Addr::LOCALHOST.into()));
    }

    /// 本机的 SOCKS5 代理：只做免认证的 UDP ASSOCIATE，回 `bound` 作为转发地址（0.0.0.0 时照 libtorrent 的约定为本机），
    /// 照 RFC 1928 第 7 节转发；控制连接断开即停。
    async fn fake_socks(bound: Ipv4Addr) -> Url {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut control, _) = listener.accept().await.unwrap();
            let mut greeting = [0; 3];
            control.read_exact(&mut greeting).await.unwrap();
            control.write_all(&[5, 0]).await.unwrap();
            let mut request = [0; 10];
            control.read_exact(&mut request).await.unwrap();
            let relay = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let mut reply = vec![5, 0, 0, 1];
            reply.extend_from_slice(&bound.octets());
            reply.extend_from_slice(&relay.local_addr().unwrap().port().to_be_bytes());
            control.write_all(&reply).await.unwrap();
            let mut client = None;
            let mut buffer = [0; 4096];
            let mut closed = [0; 1];
            loop {
                tokio::select! {
                    _ = control.read(&mut closed) => return,
                    received = relay.recv_from(&mut buffer) => {
                        let (len, from) = received.unwrap();
                        let datagram = &buffer[..len];
                        if client.is_none_or(|client| client == from) && datagram.len() > 10 {
                            client = Some(from);
                            let ip = Ipv4Addr::new(datagram[4], datagram[5], datagram[6], datagram[7]);
                            let to = SocketAddr::new(
                                ip.into(),
                                u16::from_be_bytes([datagram[8], datagram[9]]),
                            );
                            let _sent = relay.send_to(&datagram[10..], to).await;
                        } else if let (Some(client), SocketAddr::V4(back)) = (client, from) {
                            let mut wrapped = vec![0, 0, 0, 1];
                            wrapped.extend_from_slice(&back.ip().octets());
                            wrapped.extend_from_slice(&back.port().to_be_bytes());
                            wrapped.extend_from_slice(datagram);
                            let _sent = relay.send_to(&wrapped, client).await;
                        }
                    }
                }
            }
        });
        Url::parse(&format!("socks5://127.0.0.1:{port}")).unwrap()
    }

    #[tokio::test]
    async fn dht_goes_through_the_socks_relay() {
        let router = fake_network().await;
        let proxy = fake_socks(Ipv4Addr::LOCALHOST).await;
        let relay = crate::socks::associate(&proxy).await.unwrap();
        let udp = Udp::via(relay).await.unwrap().unwrap();
        let measured = dht(&udp, &[router], 0).await;
        assert_eq!(measured.nodes, ratio(4, 2));
        // 回 0.0.0.0 的：libtorrent 收不到经它回来的包
        let unspecified = fake_socks(Ipv4Addr::UNSPECIFIED).await;
        let refused = crate::socks::associate(&unspecified).await.err().unwrap();
        assert!(
            matches!(&refused, ProxyCheck::Refused(why) if why.contains("0.0.0.0")),
            "{refused:?}"
        );
    }

    /// 后台探 BT 代理只经代理：引导节点经它有回应为能用，一个都不回为用不了，连不上代理写明原因。
    #[tokio::test]
    async fn the_background_probe_asks_bootstraps_through_the_proxy() {
        let answering = fake_network().await;
        let silent = fake_node(Vec::new(), true).await;
        let proxy = fake_socks(Ipv4Addr::LOCALHOST).await;
        assert_eq!(
            probe_proxy(&proxy, &[("127.0.0.1", answering.port())]).await,
            (
                ProxyCheck::Usable,
                Resolved {
                    fake: false,
                    real: true
                }
            )
        );
        let proxy = fake_socks(Ipv4Addr::LOCALHOST).await;
        assert_eq!(
            probe_proxy(&proxy, &[("127.0.0.1", silent.port())]).await,
            (
                ProxyCheck::Silent(PROXY_SILENT.to_owned()),
                Resolved {
                    fake: false,
                    real: true
                }
            )
        );
        let closed = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let unreachable = Url::parse(&format!(
            "socks5://127.0.0.1:{}",
            closed.local_addr().unwrap().port()
        ))
        .unwrap();
        assert!(matches!(
            probe_proxy(&unreachable, &[("127.0.0.1", answering.port())]).await,
            (ProxyCheck::Unreachable(why), _) if why.contains("连不上代理")
        ));
    }
}

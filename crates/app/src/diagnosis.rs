//! BT 任务下得慢、下不动时的逐项诊断：监听、路由器的端口映射、有没有节点从外面连进来、DHT、tracker、公共 tracker、
//! 种子的人数与上传限速，每项一个结论与说明。这里只按查到的情况下结论，查由服务做（[`crate::Service::diagnose`]）。

use std::num::NonZeroU32;

use crate::socks::ProxyCheck;

/// 诊断的一项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckItem {
    /// 本机在不在监听、别人能不能连进来
    Listen,
    /// 路由器有没有把端口映射到这台电脑（UPnP、NAT-PMP）
    PortMapping,
    /// 有没有节点从外面连进来：没有多半在运营商内网（CGNAT）或被防火墙挡着
    Incoming,
    Dht,
    Trackers,
    /// 补没补上公共 tracker（ngosang/trackerslist 的列表，补给不是私有的种子）
    PublicTrackers,
    /// tracker 报的做种与在下的人数
    Swarm,
    Upload,
}

/// 一项的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CheckLevel {
    Ok,
    /// 可能拖慢，或还说不准
    Warn,
    /// 这一项不通
    Bad,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub item: CheckItem,
    pub level: CheckLevel,
    /// 查到的情况与能做的事
    pub text: String,
}

/// 诊断时查到的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Facts {
    /// 经的 SOCKS5 代理；直连时为 None。经代理时不接受外部连入、不映射端口
    pub proxy: Option<ProxyFact>,
    /// 在监听的端口；0 为没在监听
    pub listen_port: u16,
    pub listen_error: String,
    /// UPnP、NAT-PMP 映射到的外部端口（0 为没映射成）与最近一次失败的说明
    pub upnp_port: u16,
    pub upnp_error: String,
    pub natpmp_port: u16,
    pub natpmp_error: String,
    /// DHT 路由表里的节点数；None 为没统计到
    pub dht_nodes: Option<u64>,
    pub incoming: bool,
    pub private: bool,
    /// 在做种（下完了）：人数一项看有没有人要，不看凑不凑得齐
    pub seeding: bool,
    /// 这个种子连着的节点数
    pub peers: u32,
    /// 种子自己的 tracker（补的公共 tracker 在 `public` 里）
    pub trackers: Vec<Tracker>,
    /// 种子的做种与在下的人数（tracker 报的，取各处最多的）；None 为没报
    pub seeds: Option<u32>,
    pub leechers: Option<u32>,
    pub public: Public,
    /// 上传限速，字节每秒；None 为不限
    pub upload_limit: Option<NonZeroU32>,
}

/// BT 代理的状况。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProxyFact {
    /// 去掉凭据的地址
    pub address: String,
    pub check: ProxyCheck,
    /// libtorrent 经它的 UDP 最近一次出的错；没有时为空
    pub error: String,
}

/// 公共 tracker 的状况。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Public {
    /// 不补（没有列表的来源）
    Off,
    /// 列表还在取
    Fetching,
    /// 列表这次没取到，为原因
    Failed(String),
    /// 列表取到了：给这个种子补上的（还没补时为空）
    Added(Vec<Tracker>),
    /// 列表取到了，里面的都已在种子自己的 tracker 里，不用补
    Covered,
}

/// 一组 tracker 有没有回应。
enum Responses {
    /// 有这么多个回应了
    Working(usize),
    /// 都连不上；为第一个说了原因的原因（本机路由不到的排在后面：它的说明盖住别的 tracker 的真正原因），带括号，没有时
    /// 为空
    AllFailed(String),
    /// 这么多个连不上（原因同上），其余的还没回应
    SomeFailed(usize, String),
    /// 都还没回应
    Pending,
}

/// 非空的一组 tracker 有没有回应。
fn responses(trackers: &[Tracker]) -> Responses {
    let working = trackers.iter().filter(|t| t.working).count();
    let failed: Vec<&Tracker> = trackers.iter().filter(|t| t.failed).collect();
    let why = failed
        .iter()
        .filter(|t| !t.message.is_empty())
        .min_by_key(|t| t.unroutable)
        .map_or_else(String::new, |t| format!("（{}）", t.message));
    match () {
        () if working > 0 => Responses::Working(working),
        () if failed.len() == trackers.len() => Responses::AllFailed(why),
        () if !failed.is_empty() => Responses::SomeFailed(failed.len(), why),
        () => Responses::Pending,
    }
}

/// 一个 tracker 的状况。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tracker {
    pub working: bool,
    /// 每个监听地址的通告都失败过，且没有成功的
    pub failed: bool,
    /// tracker 回的错误或连不上的原因；没有时为空
    pub message: String,
    /// 本机的网络地址都路由不到它
    pub unroutable: bool,
}

/// 按查到的下结论，顺序同 [`CheckItem`]。
pub(crate) fn checks(facts: &Facts) -> Vec<Check> {
    let check = |item, level, text: String| Check { item, level, text };
    let mut checks = Vec::new();
    if let Some(proxy) = &facts.proxy {
        let address = &proxy.address;
        checks.push(match &proxy.check {
            ProxyCheck::Silent(why) if why == crate::netcheck::RESTART_AFTER_FAKE_IP => check(
                CheckItem::Listen,
                CheckLevel::Bad,
                format!(
                    "BT 代理 {address} 是通的，但 BT 还用着之前解析到的 DHT 引导节点的假地址：重启 hs-dlx 后生效"
                ),
            ),
            ProxyCheck::Unreachable(why) | ProxyCheck::Refused(why) | ProxyCheck::Silent(why) => check(
                CheckItem::Listen,
                CheckLevel::Bad,
                format!("BT 代理用不了：{why}"),
            ),
            _ if !proxy.error.is_empty() => check(
                CheckItem::Listen,
                CheckLevel::Warn,
                format!("BT 代理 {address} 最近出错：{}", proxy.error),
            ),
            ProxyCheck::Usable => check(
                CheckItem::Listen,
                CheckLevel::Ok,
                format!(
                    "经 SOCKS5 代理 {address} 连节点、tracker 与 DHT：外面连不进来，只能主动去连别人"
                ),
            ),
            ProxyCheck::Direct | ProxyCheck::Unchecked | ProxyCheck::Checking => check(
                CheckItem::Listen,
                CheckLevel::Warn,
                format!("在探 BT 代理 {address}"),
            ),
        });
        checks.push(check(
            CheckItem::PortMapping,
            CheckLevel::Ok,
            "经代理时不映射端口".to_owned(),
        ));
        checks.push(check(
            CheckItem::Incoming,
            CheckLevel::Ok,
            "经代理时不接受外部连入".to_owned(),
        ));
        return checks_after_connectivity(facts, checks);
    }
    checks.push(if facts.listen_port > 0 {
        check(
            CheckItem::Listen,
            CheckLevel::Ok,
            format!("在端口 {} 接受连入", facts.listen_port),
        )
    } else {
        let why = if facts.listen_error.is_empty() {
            String::new()
        } else {
            format!("（{}）", facts.listen_error)
        };
        check(
            CheckItem::Listen,
            CheckLevel::Bad,
            format!("没在监听{why}：别人连不进来，只能主动去连别人"),
        )
    });
    let listening = facts.listen_port > 0;
    let mapped = [("UPnP", facts.upnp_port), ("NAT-PMP", facts.natpmp_port)]
        .into_iter()
        .find(|(_, port)| *port > 0);
    let errors: Vec<String> = [
        ("UPnP", &facts.upnp_error),
        ("NAT-PMP", &facts.natpmp_error),
    ]
    .into_iter()
    .filter(|(_, error)| !error.is_empty())
    .map(|(via, error)| format!("{via}：{error}"))
    .collect();
    checks.push(match mapped {
        _ if !listening => check(
            CheckItem::PortMapping,
            CheckLevel::Warn,
            "没在监听，不请求路由器映射端口".to_owned(),
        ),
        Some((via, port)) => check(
            CheckItem::PortMapping,
            CheckLevel::Ok,
            format!("路由器已映射端口（{via}，外部 {port}）"),
        ),
        None if errors.is_empty() => check(
            CheckItem::PortMapping,
            CheckLevel::Warn,
            "路由器还没回应端口映射".to_owned(),
        ),
        None => check(
            CheckItem::PortMapping,
            CheckLevel::Warn,
            format!(
                "路由器没映射端口（{}）：在路由器上打开 UPnP，或把端口 {} 手动映射到这台电脑；不在路由器后面的不用管",
                errors.join("；"),
                facts.listen_port
            ),
        ),
    });
    checks.push(if facts.incoming {
        check(
            CheckItem::Incoming,
            CheckLevel::Ok,
            "有节点从外面连了进来".to_owned(),
        )
    } else if !listening {
        check(
            CheckItem::Incoming,
            CheckLevel::Warn,
            "没在监听，别人连不进来".to_owned(),
        )
    } else {
        check(
            CheckItem::Incoming,
            CheckLevel::Warn,
            "到现在还没有节点从外面连进来：可能在运营商内网（CGNAT）或被防火墙挡着，只能主动去连别人，能连上的会少很多"
                .to_owned(),
        )
    });
    checks_after_connectivity(facts, checks)
}

/// 监听、端口映射与连入之后的各项：DHT、tracker、公共 tracker、人数与上传限速，接在 `checks` 后面。
fn checks_after_connectivity(facts: &Facts, mut checks: Vec<Check>) -> Vec<Check> {
    let check = |item, level, text: String| Check { item, level, text };
    checks.push(match (facts.private, facts.dht_nodes) {
        (true, _) => check(
            CheckItem::Dht,
            CheckLevel::Ok,
            "私有种子不用 DHT".to_owned(),
        ),
        // 第一项「BT 代理」报了问题（用不了、出过错、还在探）时原因与改法在那里
        (false, Some(0))
            if facts.proxy.as_ref().is_some_and(|proxy| {
                proxy.check != ProxyCheck::Usable || !proxy.error.is_empty()
            }) =>
        {
            check(
                CheckItem::Dht,
                CheckLevel::Warn,
                "DHT 里一个节点也没有：刚开始找要等一会儿；一直没有时原因与改法见第一项「BT 代理」".to_owned(),
            )
        }
        (false, Some(0)) if facts.proxy.is_some() => check(
            CheckItem::Dht,
            CheckLevel::Warn,
            "DHT 里一个节点也没有：刚开始找要等一会儿；代理探着能用而一直没有时，到设置里做一次网络检测，看经代理的 DHT 与引导节点能不能解析"
                .to_owned(),
        ),
        (false, Some(0)) => check(
            CheckItem::Dht,
            CheckLevel::Warn,
            "DHT 里一个节点也没有：刚开始找要等一会儿；一直没有多半是网络挡了 UDP 或 DHT，只能靠 tracker 找节点"
                .to_owned(),
        ),
        (false, Some(nodes)) => check(
            CheckItem::Dht,
            CheckLevel::Ok,
            format!("DHT 里有 {nodes} 个节点"),
        ),
        (false, None) => check(
            CheckItem::Dht,
            CheckLevel::Warn,
            "没统计到 DHT 的节点数".to_owned(),
        ),
    });
    let total = facts.trackers.len();
    checks.push(match responses(&facts.trackers) {
        _ if total == 0 && facts.private => check(
            CheckItem::Trackers,
            CheckLevel::Bad,
            "私有种子里没有 tracker：找不到节点".to_owned(),
        ),
        _ if total == 0 => match &facts.public {
            Public::Added(added) if !added.is_empty() => check(
                CheckItem::Trackers,
                CheckLevel::Ok,
                "种子里没有 tracker，靠 DHT 与补的公共 tracker 找节点".to_owned(),
            ),
            _ => check(
                CheckItem::Trackers,
                CheckLevel::Warn,
                "种子里没有 tracker，公共 tracker 也还没补上：只靠 DHT 找节点".to_owned(),
            ),
        },
        // 一层里找到一个能用的就不再挨个试同层别的，回应了的往往每层只有一个
        Responses::Working(working) => check(
            CheckItem::Trackers,
            CheckLevel::Ok,
            format!("种子里的 tracker 正常（{working} 个回应了）"),
        ),
        Responses::AllFailed(why) => check(
            CheckItem::Trackers,
            if facts.private {
                CheckLevel::Bad
            } else {
                CheckLevel::Warn
            },
            format!("种子里的 {total} 个 tracker 都连不上{why}"),
        ),
        Responses::SomeFailed(failed, why) => check(
            CheckItem::Trackers,
            CheckLevel::Warn,
            format!("种子里的 {failed} 个 tracker 连不上{why}，其余的还没回应"),
        ),
        Responses::Pending => check(
            CheckItem::Trackers,
            CheckLevel::Warn,
            format!("种子里的 {total} 个 tracker 还没回应"),
        ),
    });
    checks.push(match &facts.public {
        _ if facts.private => check(
            CheckItem::PublicTrackers,
            CheckLevel::Ok,
            "私有种子不补公共 tracker".to_owned(),
        ),
        Public::Off => check(
            CheckItem::PublicTrackers,
            CheckLevel::Ok,
            "不补公共 tracker".to_owned(),
        ),
        Public::Fetching => check(
            CheckItem::PublicTrackers,
            CheckLevel::Warn,
            "公共 tracker 列表还在取".to_owned(),
        ),
        Public::Failed(why) => check(
            CheckItem::PublicTrackers,
            CheckLevel::Warn,
            format!(
                "公共 tracker 列表没取到（{why}）：隔一阵再取；取到之前只靠种子里的 tracker 与 DHT 找节点，可检查设置里的「代理」"
            ),
        ),
        Public::Covered => check(
            CheckItem::PublicTrackers,
            CheckLevel::Ok,
            "公共 tracker 都已在种子自己的 tracker 里".to_owned(),
        ),
        Public::Added(added) if added.is_empty() => check(
            CheckItem::PublicTrackers,
            CheckLevel::Warn,
            "公共 tracker 列表取到了，还没补上".to_owned(),
        ),
        Public::Added(added) => {
            let total = added.len();
            match responses(added) {
                Responses::Working(working) => check(
                    CheckItem::PublicTrackers,
                    CheckLevel::Ok,
                    format!("补了 {total} 个公共 tracker，{working} 个回应了"),
                ),
                Responses::AllFailed(why) => check(
                    CheckItem::PublicTrackers,
                    CheckLevel::Warn,
                    format!("补的 {total} 个公共 tracker 都连不上{why}"),
                ),
                Responses::SomeFailed(failed, why) => check(
                    CheckItem::PublicTrackers,
                    CheckLevel::Warn,
                    format!("补了 {total} 个公共 tracker，{failed} 个连不上{why}，其余的还没回应"),
                ),
                Responses::Pending => check(
                    CheckItem::PublicTrackers,
                    CheckLevel::Warn,
                    format!("补了 {total} 个公共 tracker，都还没回应"),
                ),
            }
        }
    });
    let counted = |seeds: u32, leechers: Option<u32>| {
        format!(
            "tracker 报做种 {seeds} 人{}",
            leechers.map_or_else(String::new, |n| format!("、在下 {n} 人"))
        )
    };
    checks.push(match (facts.seeding, facts.seeds, facts.leechers) {
        (true, _, Some(0)) => check(
            CheckItem::Swarm,
            CheckLevel::Ok,
            "tracker 报现在没人在下：暂时没人要你传".to_owned(),
        ),
        (true, Some(seeds), leechers) => {
            check(CheckItem::Swarm, CheckLevel::Ok, counted(seeds, leechers))
        }
        (true, None, _) => check(
            CheckItem::Swarm,
            CheckLevel::Ok,
            "tracker 没报人数".to_owned(),
        ),
        (false, Some(0), Some(0)) if facts.peers > 0 => check(
            CheckItem::Swarm,
            CheckLevel::Warn,
            format!(
                "tracker 报没人，可连着 {} 个节点：多半是 tracker 不统计这个种子",
                facts.peers
            ),
        ),
        (false, Some(0), Some(0)) => check(
            CheckItem::Swarm,
            CheckLevel::Bad,
            "tracker 报现在没人有这个种子".to_owned(),
        ),
        (false, Some(0), leechers) => check(
            CheckItem::Swarm,
            CheckLevel::Warn,
            format!(
                "tracker 报现在没人做种{}：能不能下完要看在下的人凑不凑得齐",
                leechers.map_or_else(String::new, |n| format!("（在下的 {n} 人）"))
            ),
        ),
        (false, Some(seeds), leechers) => {
            check(CheckItem::Swarm, CheckLevel::Ok, counted(seeds, leechers))
        }
        (false, None, _) => check(
            CheckItem::Swarm,
            if facts.peers > 0 {
                CheckLevel::Ok
            } else {
                CheckLevel::Warn
            },
            format!("tracker 没报人数，连着 {} 个节点", facts.peers),
        ),
    });
    checks.push(match facts.upload_limit {
        Some(limit) => check(
            CheckItem::Upload,
            CheckLevel::Ok,
            format!("上传限速 {} KB/s", limit.get() >> 10),
        ),
        None => check(CheckItem::Upload, CheckLevel::Ok, "上传不限速".to_owned()),
    });
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> Facts {
        Facts {
            proxy: None,
            listen_port: 51413,
            listen_error: String::new(),
            upnp_port: 51413,
            upnp_error: String::new(),
            natpmp_port: 0,
            natpmp_error: String::new(),
            dht_nodes: Some(320),
            incoming: true,
            private: false,
            seeding: false,
            peers: 5,
            trackers: vec![Tracker {
                working: true,
                failed: false,
                message: String::new(),
                unroutable: false,
            }],
            seeds: Some(12),
            leechers: Some(3),
            public: Public::Added(vec![Tracker {
                working: true,
                failed: false,
                message: String::new(),
                unroutable: false,
            }]),
            upload_limit: None,
        }
    }

    fn level(facts: &Facts, item: CheckItem) -> CheckLevel {
        checks(facts)
            .into_iter()
            .find(|check| check.item == item)
            .unwrap()
            .level
    }

    #[test]
    fn a_healthy_session_passes_every_check() {
        assert!(
            checks(&healthy())
                .iter()
                .all(|check| check.level == CheckLevel::Ok)
        );
    }

    #[test]
    fn problems_are_told_apart() {
        let mut facts = healthy();
        facts.listen_port = 0;
        facts.listen_error = "address in use".into();
        assert_eq!(level(&facts, CheckItem::Listen), CheckLevel::Bad);
        // 没在监听：映射与连入无从判断
        facts.incoming = false;
        let text = |facts: &Facts, item| {
            checks(facts)
                .into_iter()
                .find(|check| check.item == item)
                .unwrap()
                .text
        };
        assert!(text(&facts, CheckItem::PortMapping).contains("没在监听"));
        assert!(text(&facts, CheckItem::Incoming).contains("没在监听"));
        let mut facts = healthy();
        facts.upnp_port = 0;
        facts.upnp_error = "no router".into();
        assert_eq!(level(&facts, CheckItem::PortMapping), CheckLevel::Warn);
        let mapping = text(&facts, CheckItem::PortMapping);
        assert!(mapping.contains("no router") && mapping.contains("51413"));
        let mut facts = healthy();
        facts.dht_nodes = Some(0);
        assert_eq!(level(&facts, CheckItem::Dht), CheckLevel::Warn);
        facts.private = true;
        assert_eq!(level(&facts, CheckItem::Dht), CheckLevel::Ok);
        // tracker：都连不上、一部分连不上、还没回应
        let mut facts = healthy();
        let tracker = |failed| Tracker {
            working: false,
            failed,
            message: if failed {
                "timed out".into()
            } else {
                String::new()
            },
            unroutable: false,
        };
        facts.trackers = vec![tracker(true), tracker(true)];
        assert_eq!(level(&facts, CheckItem::Trackers), CheckLevel::Warn);
        assert!(text(&facts, CheckItem::Trackers).contains("timed out"));
        // 本机路由不到的排在后面，说的是别的 tracker 的真正原因
        let unroutable = Tracker {
            message: "本机的网络地址都连不到它".into(),
            unroutable: true,
            ..tracker(true)
        };
        facts.trackers = vec![unroutable, tracker(true)];
        assert!(text(&facts, CheckItem::Trackers).contains("timed out"));
        // 私有种子只有自己的 tracker 能用：都连不上即不通
        facts.private = true;
        assert_eq!(level(&facts, CheckItem::Trackers), CheckLevel::Bad);
        facts.private = false;
        // 补的公共 tracker 与种子自己的分开下结论
        facts.public = Public::Added(vec![tracker(true), tracker(true)]);
        assert_eq!(level(&facts, CheckItem::PublicTrackers), CheckLevel::Warn);
        assert!(
            text(&facts, CheckItem::PublicTrackers).starts_with("补的 2 个公共 tracker 都连不上")
        );
        facts.trackers = vec![tracker(true), tracker(false)];
        assert_eq!(level(&facts, CheckItem::Trackers), CheckLevel::Warn);
        assert!(text(&facts, CheckItem::Trackers).contains("其余的还没回应"));
        facts.trackers = vec![tracker(false)];
        assert!(text(&facts, CheckItem::Trackers).contains("还没回应"));
        // 经代理：不接受连入、不映射端口不算问题；代理用不了、出过错时报在第一项
        let mut facts = healthy();
        facts.proxy = Some(ProxyFact {
            address: "socks5://127.0.0.1:1080".into(),
            check: ProxyCheck::Usable,
            error: String::new(),
        });
        facts.listen_port = 0;
        facts.upnp_port = 0;
        facts.incoming = false;
        assert!(
            checks(&facts)
                .iter()
                .all(|check| check.level == CheckLevel::Ok)
        );
        assert!(text(&facts, CheckItem::Listen).contains("socks5://127.0.0.1:1080"));
        assert_eq!(checks(&facts).len(), checks(&healthy()).len());
        let proxy = facts.proxy.as_mut().unwrap();
        proxy.error = "connection refused".into();
        assert_eq!(level(&facts, CheckItem::Listen), CheckLevel::Warn);
        facts.proxy.as_mut().unwrap().check = ProxyCheck::Refused("代理不转发 UDP".into());
        assert_eq!(level(&facts, CheckItem::Listen), CheckLevel::Bad);
        assert!(text(&facts, CheckItem::Listen).contains("不转发 UDP"));
        facts.dht_nodes = Some(0);
        assert!(text(&facts, CheckItem::Dht).contains("BT 代理"));
        // 公共 tracker：没取到时带上原因；私有种子不补
        let mut facts = healthy();
        facts.public = Public::Failed("connection refused".into());
        assert_eq!(level(&facts, CheckItem::PublicTrackers), CheckLevel::Warn);
        assert!(text(&facts, CheckItem::PublicTrackers).contains("connection refused"));
        facts.private = true;
        assert_eq!(level(&facts, CheckItem::PublicTrackers), CheckLevel::Ok);
        // 人数：没人且没连着节点为不通；tracker 报没人而连着节点的不算
        let mut facts = healthy();
        facts.seeds = Some(0);
        facts.leechers = Some(0);
        assert_eq!(level(&facts, CheckItem::Swarm), CheckLevel::Warn);
        facts.peers = 0;
        assert_eq!(level(&facts, CheckItem::Swarm), CheckLevel::Bad);
        facts.leechers = Some(4);
        assert_eq!(level(&facts, CheckItem::Swarm), CheckLevel::Warn);
        // 做种时没人在下不算问题
        facts.seeding = true;
        facts.leechers = Some(0);
        assert_eq!(level(&facts, CheckItem::Swarm), CheckLevel::Ok);
        let mut facts = healthy();
        facts.incoming = false;
        assert_eq!(level(&facts, CheckItem::Incoming), CheckLevel::Warn);
    }
}

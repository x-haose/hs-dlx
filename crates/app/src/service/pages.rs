//! 内置网页的名额：同时开着的网页不超过设置里的个数，要开的先到先得，其余排队。
//!
//! 一个任务从进到「在网页里找流」起有一个位子（[`Seat`]），排着或占着名额；位子放在任务的运行登记里，登记撤掉或
//! 换掉（暂停、失败、放弃、定下、被另一轮顶掉）即还，任务不再用网页时不用各自记得还。位子的增减与名额的放行都由服务在
//! `store` 锁内做（运行时关闭、丢弃任务时除外），与任务状态的改动不交错。

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use super::lock;
use crate::page::{PageHost, PagePace};

/// 名额的账。
pub(super) struct Pages {
    book: Arc<Book>,
}

struct Book {
    ledger: Mutex<Ledger>,
    /// 账每变一次加一：排着的任务据此重新看自己排到哪了
    changed: watch::Sender<u64>,
}

struct Ledger {
    /// 注册了的宿主与找流的几个时长
    host: Option<(Arc<dyn PageHost>, PagePace)>,
    /// 宿主此刻开得了网页；为否时不放行
    ready: bool,
    /// 宿主说过几回「开得了」（注册算一回）：占上名额时记下，之后宿主关掉了那个网页时据此分清这是不是它回来之前
    /// 的旧消息
    returns: u64,
    /// 同时最多占几个名额。调小时已占着的不收回
    limit: usize,
    /// 下一个位子的次序
    next: u64,
    /// 占着名额的位子
    seated: BTreeSet<u64>,
    /// 排着的位子：次序小的先放行
    queued: BTreeSet<u64>,
}

impl Ledger {
    /// 有空的名额、宿主开得了时，让排在前面的占上。
    fn admit(&mut self) {
        while self.ready && self.seated.len() < self.limit {
            let Some(first) = self.queued.pop_first() else {
                return;
            };
            self.seated.insert(first);
        }
    }
}

/// 一个位子此刻的情况。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Place {
    /// 占着名额：可以开网页
    Seated,
    /// 排着：前面还有 `ahead` 个；`away` 为宿主此刻开不了网页
    Queued { ahead: usize, away: bool },
}

/// 一个任务的位子；释放即让出（占着的名额还回去，排在后面的往前挪）。
pub(super) struct Seat {
    book: Arc<Book>,
    order: u64,
}

impl Pages {
    /// 没有宿主、同时最多占 `limit` 个名额的账。
    pub fn new(limit: usize) -> Pages {
        let (changed, _) = watch::channel(0);
        Pages {
            book: Arc::new(Book {
                ledger: Mutex::new(Ledger {
                    host: None,
                    ready: false,
                    returns: 0,
                    limit,
                    next: 0,
                    seated: BTreeSet::new(),
                    queued: BTreeSet::new(),
                }),
                changed,
            }),
        }
    }

    /// 注册了的宿主与找流的几个时长；没注册时为 None。
    pub fn host(&self) -> Option<(Arc<dyn PageHost>, PagePace)> {
        lock(&self.book.ledger).host.clone()
    }

    /// 注册宿主：它此刻开得了网页。
    pub fn set_host(&self, host: Arc<dyn PageHost>, pace: PagePace) {
        self.book.update(|ledger| {
            ledger.host = Some((host, pace));
            ledger.ready = true;
            ledger.returns += 1;
        });
    }

    /// 宿主此刻开不开得了网页：开得了时随即放行排着的。
    pub fn set_ready(&self, ready: bool) {
        self.book.update(|ledger| {
            ledger.ready = ready;
            if ready {
                ledger.returns += 1;
            }
        });
    }

    /// 宿主说过几回「开得了」：占上名额的那一刻读它，交回给 [`Pages::away`]、[`Pages::gone`]。
    pub fn returns(&self) -> u64 {
        lock(&self.book.ledger).returns
    }

    /// 改同时最多占几个名额：调大时随即放行排着的，调小时已占着的不收回。
    pub fn set_limit(&self, limit: usize) {
        self.book.update(|ledger| ledger.limit = limit);
    }

    /// 来了一个要开网页的任务：排到最后，有空的名额时随即占上。
    pub fn enter(&self) -> Seat {
        let mut order = 0;
        self.book.update(|ledger| {
            order = ledger.next;
            ledger.next += 1;
            ledger.queued.insert(order);
        });
        Seat {
            book: self.book.clone(),
            order,
        }
    }

    /// 账变了的通知：每变一次值加一。
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.book.changed.subscribe()
    }

    /// 次序为 `order` 的位子此刻的情况；位子已让出时为 None。
    pub fn place(&self, order: u64) -> Option<Place> {
        let ledger = lock(&self.book.ledger);
        if ledger.seated.contains(&order) {
            return Some(Place::Seated);
        }
        ledger.queued.contains(&order).then(|| Place::Queued {
            ahead: ledger.queued.range(..order).count(),
            away: !ledger.ready,
        })
    }

    /// 宿主关掉了次序为 `order` 的位子的网页（或说此刻开不了）：这个位子让出名额、照原来的次序排回去；`since` 为它
    /// 占上名额时的 [`Pages::returns`]，这之后宿主没说过「开得了」时记下宿主开不了（说过的是它回来之前的旧消息，
    /// 位子随即再占上）。
    pub fn away(&self, order: u64, since: u64) {
        self.book.update(|ledger| {
            if ledger.returns == since {
                ledger.ready = false;
            }
            if ledger.seated.remove(&order) {
                ledger.queued.insert(order);
            }
        });
    }

    /// 同 [`Pages::away`]，只记宿主开不了，不动位子（位子随它的登记撤掉而让出）。
    pub fn gone(&self, since: u64) {
        self.book.update(|ledger| {
            if ledger.returns == since {
                ledger.ready = false;
            }
        });
    }
}

impl Book {
    /// 改账，随后放行能放行的，并通知账变了。
    fn update(&self, change: impl FnOnce(&mut Ledger)) {
        {
            let mut ledger = lock(&self.ledger);
            change(&mut ledger);
            ledger.admit();
        }
        self.changed.send_modify(|version| *version += 1);
    }
}

impl Seat {
    /// 这个位子的次序：先到的小。
    pub fn order(&self) -> u64 {
        self.order
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        let order = self.order;
        self.book.update(|ledger| {
            ledger.seated.remove(&order);
            ledger.queued.remove(&order);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskId;
    use crate::page::{PageSession, Unopened};
    use hs_m3u8_core::{Proxy, Url};

    struct NoPages;

    impl PageHost for NoPages {
        fn open(&self, _: TaskId, _: &Url, _: &Proxy) -> Result<PageSession, Unopened> {
            Err(Unopened::Away)
        }
    }

    fn hosted(limit: usize) -> Pages {
        let pages = Pages::new(limit);
        pages.set_host(Arc::new(NoPages), PagePace::default());
        pages
    }

    fn queued(ahead: usize) -> Option<Place> {
        Some(Place::Queued { ahead, away: false })
    }

    #[test]
    fn seats_are_taken_in_arrival_order_up_to_the_limit() {
        let pages = hosted(2);
        let seats: Vec<Seat> = (0..4).map(|_| pages.enter()).collect();
        let place = |index: usize| pages.place(seats[index].order());
        assert_eq!(
            [place(0), place(1), place(2), place(3)],
            [
                Some(Place::Seated),
                Some(Place::Seated),
                queued(0),
                queued(1)
            ]
        );
        // 排着的走了：后面的往前挪；占着的走了：排最前的占上
        let mut seats = seats;
        let third = seats.remove(2);
        let fourth = seats[2].order();
        drop(third);
        assert_eq!(pages.place(fourth), queued(0));
        let first = seats.remove(0);
        let gone = first.order();
        drop(first);
        assert_eq!(pages.place(fourth), Some(Place::Seated));
        assert_eq!(pages.place(gone), None);
    }

    #[test]
    fn a_lower_limit_waits_for_seats_to_empty_and_a_higher_one_admits_at_once() {
        let pages = hosted(2);
        let mut seats: Vec<Seat> = (0..4).map(|_| pages.enter()).collect();
        let last = seats[3].order();
        pages.set_limit(1);
        // 已占着的两个不收回；走了一个还是超着，不放行
        assert_eq!(pages.place(seats[1].order()), Some(Place::Seated));
        drop(seats.remove(0));
        assert_eq!(pages.place(seats[1].order()), queued(0));
        drop(seats.remove(0));
        assert_eq!(pages.place(seats[0].order()), Some(Place::Seated));
        assert_eq!(pages.place(last), queued(0));
        pages.set_limit(3);
        assert_eq!(pages.place(last), Some(Place::Seated));
    }

    #[test]
    fn seats_whose_pages_the_host_closed_go_back_in_their_order_until_it_returns() {
        let pages = hosted(2);
        let seats: Vec<Seat> = (0..3).map(|_| pages.enter()).collect();
        let place = |index: usize| pages.place(seats[index].order());
        let mut changed = pages.subscribe();
        changed.borrow_and_update();
        // 宿主关掉了第二个、第一个的网页：都排回去，照原来的次序，在后到的前面
        let since = pages.returns();
        pages.away(seats[1].order(), since);
        pages.away(seats[0].order(), since);
        assert!(changed.has_changed().unwrap());
        let away = |ahead| Some(Place::Queued { ahead, away: true });
        assert_eq!([place(0), place(1), place(2)], [away(0), away(1), away(2)]);
        // 宿主开不了时来的也只排着
        let late = pages.enter();
        assert_eq!(pages.place(late.order()), away(3));
        pages.set_ready(true);
        assert_eq!(
            [place(0), place(1), place(2)],
            [Some(Place::Seated), Some(Place::Seated), queued(0)]
        );
        // 宿主回来之后才处理到的旧消息：不再记它开不了，位子排回去随即又占上
        pages.away(seats[0].order(), since);
        pages.gone(since);
        assert_eq!(
            [place(0), place(1), place(2)],
            [Some(Place::Seated), Some(Place::Seated), queued(0)]
        );
        // 回来之后占上的位子的网页又被关掉：记下开不了
        pages.gone(pages.returns());
        pages.away(seats[1].order(), pages.returns());
        assert_eq!(
            [place(0), place(1), place(2)],
            [Some(Place::Seated), away(0), away(1)]
        );
    }

    #[test]
    fn nothing_is_admitted_before_a_host_is_registered() {
        let pages = Pages::new(2);
        assert!(pages.host().is_none());
        let seat = pages.enter();
        assert_eq!(
            pages.place(seat.order()),
            Some(Place::Queued {
                ahead: 0,
                away: true
            })
        );
    }
}

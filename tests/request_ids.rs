#![allow(dead_code)]
//! Request ids: an actor asks another and gets the answer as one of its own
//! messages.
//!
//! Every test drives real actors. A `Callee` answers a request in the way a row
//! names; an `Asker` makes the requests and logs, in the order its `handle`
//! sees them, every reply and every other message. Nothing is ordered by a
//! sleep: the actors wait at gates the tests open, and the bounds only detect
//! a hang.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{oneshot, Semaphore};
use tokio::time::{sleep, timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, AskError, Down, RequestError, RequestId,
    RequestReply, StopReason,
};

mod common;

const BOUND: Duration = Duration::from_secs(10);

/// How long a thing that must not happen is watched for.
const WINDOW: Duration = Duration::from_millis(150);

/// Rounds a scenario may take to have its reply claimed before the cancel.
const ROUNDS: usize = 20;

async fn within<T>(what: &str, fut: impl std::future::Future<Output = T>) -> T {
    match timeout(BOUND, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for: {what}"),
    }
}

/// Polls `condition` until it holds.
async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    within(what, async {
        while !condition() {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
}

// ---------------------------------------------------------------------------
// The callee
// ---------------------------------------------------------------------------

enum Req {
    Echo(u32),
    Fail,
    Panic,
    Hang,
    /// Waits for a permit, then answers.
    Gated(Arc<Semaphore>, u32),
    /// Casts `Data(first)` to the asker, then answers `second`.
    Tell(ActorHandle<Asker>, u32, u32),
    /// Sleeps, then answers.
    Sleep(Duration, u32),
}

struct Callee {
    /// Requests whose handling began.
    handled: Arc<AtomicUsize>,
}

fn new_callee() -> (Callee, Arc<AtomicUsize>) {
    let handled = Arc::new(AtomicUsize::new(0));
    (
        Callee {
            handled: handled.clone(),
        },
        handled,
    )
}

impl Actor for Callee {
    type Message = Req;
    type Response = u32;

    async fn handle(&mut self, msg: Req, _: &mut ActorContext<Self>) -> ActorResult<u32> {
        self.handled.fetch_add(1, Ordering::SeqCst);
        match msg {
            Req::Echo(value) => Ok(value),
            Req::Fail => Err(ActorError::user("boom")),
            Req::Panic => panic!("boom"),
            Req::Hang => {
                std::future::pending::<()>().await;
                Ok(0)
            }
            Req::Gated(gate, value) => {
                gate.acquire().await.expect("the gate lives").forget();
                Ok(value)
            }
            Req::Tell(asker, first, second) => {
                let _ = asker.notify(Msg::Data(first)).await;
                Ok(second)
            }
            Req::Sleep(duration, value) => {
                sleep(duration).await;
                Ok(value)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The asker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Entry {
    Reply {
        id: RequestId,
        result: Result<u32, AskError>,
    },
    SelfReply(Option<u32>),
    Data(u32),
    Echo(u32),
    TurnEnd,
    /// A `Gone` notice was handled and the request it tried was accepted.
    Gone,
    /// A `Gone` notice was handled and the request it tried was refused.
    Refused(RequestError),
}

type Log = Arc<Mutex<Vec<Entry>>>;

enum Msg {
    /// Makes one request and answers its id.
    Ask {
        callee: ActorHandle<Callee>,
        req: Req,
        within: Option<Duration>,
    },
    /// Makes one request, parks at the hold, then answers the id, or cancels
    /// the request and answers what the cancel said.
    Hold {
        callee: ActorHandle<Callee>,
        req: Req,
        hold: Arc<Semaphore>,
        cancel: bool,
    },
    /// Makes gated requests until one is refused, or `count` are made.
    Fire {
        callee: ActorHandle<Callee>,
        gate: Arc<Semaphore>,
        count: usize,
        cap: Option<usize>,
    },
    Cancel(RequestId),
    Watch(ActorHandle<Callee>),
    /// Makes a request whose map panics on the answer 666.
    PanicOn(ActorHandle<Callee>),
    /// Asks itself.
    SelfAsk,
    Echo(u32),
    Reply(RequestReply<u32>),
    SelfReply(RequestReply<Resp>),
    Data(u32),
    Gone(Down),
    Probe,
}

#[derive(Debug)]
enum Resp {
    Id(Result<RequestId, RequestError>),
    Cancelled(bool),
    Fired {
        made: usize,
        ids: Vec<RequestId>,
        refused: Option<RequestError>,
    },
    Replies(usize),
    Echoed(u32),
    Unit,
}

struct Asker {
    log: Log,
}

impl Actor for Asker {
    type Message = Msg;
    type Response = Resp;

    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<Resp> {
        match msg {
            Msg::Ask {
                callee,
                req,
                within,
            } => Ok(Resp::Id(match within {
                Some(within) => ctx.request_within(&callee, req, within, Msg::Reply),
                None => ctx.request(&callee, req, Msg::Reply),
            })),
            Msg::Hold {
                callee,
                req,
                hold,
                cancel,
            } => {
                let id = ctx.request(&callee, req, Msg::Reply);
                hold.acquire().await.expect("the hold lives").forget();
                match (cancel, id) {
                    (true, Ok(id)) => Ok(Resp::Cancelled(ctx.cancel_request(id))),
                    (_, id) => Ok(Resp::Id(id)),
                }
            }
            Msg::Fire {
                callee,
                gate,
                count,
                cap,
            } => {
                if let Some(cap) = cap {
                    ctx.set_request_cap(cap);
                }
                let mut ids = Vec::new();
                let mut refused = None;
                while ids.len() < count {
                    let req = Req::Gated(gate.clone(), ids.len() as u32);
                    match ctx.request(&callee, req, Msg::Reply) {
                        Ok(id) => ids.push(id),
                        Err(err) => {
                            refused = Some(err);
                            break;
                        }
                    }
                }
                Ok(Resp::Fired {
                    made: ids.len(),
                    ids,
                    refused,
                })
            }
            Msg::Cancel(id) => Ok(Resp::Cancelled(ctx.cancel_request(id))),
            Msg::Watch(target) => {
                ctx.monitor(&target, Msg::Gone).expect("running");
                Ok(Resp::Unit)
            }
            Msg::PanicOn(callee) => {
                let id = ctx.request(&callee, Req::Echo(666), |reply| {
                    if matches!(reply.result, Ok(666)) {
                        panic!("boom");
                    }
                    Msg::Reply(reply)
                });
                Ok(Resp::Id(id))
            }
            Msg::SelfAsk => {
                let me = ctx.self_handle();
                ctx.request(&me, Msg::Echo(5), Msg::SelfReply)
                    .expect("a request to oneself is allowed");
                self.log.lock().unwrap().push(Entry::TurnEnd);
                Ok(Resp::Unit)
            }
            Msg::Echo(value) => {
                self.log.lock().unwrap().push(Entry::Echo(value));
                Ok(Resp::Echoed(value))
            }
            Msg::Reply(reply) => {
                self.log.lock().unwrap().push(Entry::Reply {
                    id: reply.id,
                    result: reply.result,
                });
                Ok(Resp::Unit)
            }
            Msg::SelfReply(reply) => {
                let value = match reply.result {
                    Ok(Resp::Echoed(value)) => Some(value),
                    _ => None,
                };
                self.log.lock().unwrap().push(Entry::SelfReply(value));
                Ok(Resp::Unit)
            }
            Msg::Data(value) => {
                self.log.lock().unwrap().push(Entry::Data(value));
                Ok(Resp::Unit)
            }
            Msg::Gone(_) => {
                let me = ctx.self_handle();
                let entry = match ctx.request(&me, Msg::Echo(0), Msg::SelfReply) {
                    Ok(_) => Entry::Gone,
                    Err(err) => Entry::Refused(err),
                };
                self.log.lock().unwrap().push(entry);
                Ok(Resp::Unit)
            }
            Msg::Probe => {
                let replies = self
                    .log
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|entry| matches!(entry, Entry::Reply { .. }))
                    .count();
                Ok(Resp::Replies(replies))
            }
        }
    }
}

struct Rig {
    asker: ActorHandle<Asker>,
    log: Log,
}

impl Rig {
    async fn start(config: ActorConfig) -> Rig {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let asker = Asker { log: log.clone() }
            .spawn()
            .with_config(config)
            .await
            .expect("the asker starts");
        Rig { asker, log }
    }

    fn entries(&self) -> Vec<Entry> {
        self.log.lock().unwrap().clone()
    }

    fn replies(&self) -> Vec<(RequestId, Result<u32, AskError>)> {
        self.entries()
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Reply { id, result } => Some((id, result)),
                _ => None,
            })
            .collect()
    }

    async fn id_of(&self, msg: Msg) -> RequestId {
        match within("the asker answers", self.asker.send(msg))
            .await
            .expect("the asker takes the message")
        {
            Resp::Id(Ok(id)) => id,
            other => panic!("expected a request id, got {other:?}"),
        }
    }

    async fn one_reply(&self) -> (RequestId, Result<u32, AskError>) {
        until("a reply reaches the asker", || !self.replies().is_empty()).await;
        self.replies().remove(0)
    }

    /// How many replies the asker's `handle` has seen once everything it was
    /// sent before this call has been handled.
    async fn replies_seen(&self) -> usize {
        match within("the probe", self.asker.send(Msg::Probe))
            .await
            .expect("the asker takes the probe")
        {
            Resp::Replies(count) => count,
            other => panic!("expected a count, got {other:?}"),
        }
    }

    async fn in_flight(&self) -> usize {
        within("the status", self.asker.get_status())
            .await
            .expect("the asker answers")
            .requests_in_flight
    }
}

fn capacity(capacity: usize) -> ActorConfig {
    ActorConfig::default().with_mailbox_capacity(capacity)
}

// ---------------------------------------------------------------------------
// Where the reply goes
// ---------------------------------------------------------------------------

/// A reply waits for room in the asker's mailbox like any other message: with
/// the mailbox full it queues behind the messages that took the slots, and the
/// mailbox never holds more than its capacity.
#[test]
fn reply_takes_an_ordinary_slot() {
    common::run(4, async {
        let rig = Rig::start(capacity(4)).await;
        let (callee, handled) = new_callee();
        let callee = callee.spawn().await.unwrap();
        let hold = Arc::new(Semaphore::new(0));
        let gate = Arc::new(Semaphore::new(0));
        let call = tokio::spawn({
            let asker = rig.asker.clone();
            let (callee, hold, gate) = (callee.clone(), hold.clone(), gate.clone());
            async move {
                asker
                    .send(Msg::Hold {
                        callee,
                        req: Req::Gated(gate, 9),
                        hold,
                        cancel: false,
                    })
                    .await
            }
        });
        // The asker parks in its handler; four messages fill its mailbox.
        until("the callee has the request", || {
            handled.load(Ordering::SeqCst) == 1
        })
        .await;
        for value in 1..=4 {
            within("a filler", rig.asker.notify(Msg::Data(value)))
                .await
                .expect("the mailbox takes the filler");
        }
        assert_eq!(rig.asker.mailbox_len(), 4);
        // The callee answers: the reply has no free slot and waits for one.
        gate.add_permits(1);
        sleep(WINDOW).await;
        assert_eq!(rig.asker.mailbox_len(), 4, "the reply took no hidden slot");
        assert!(
            rig.replies().is_empty(),
            "the reply cannot overtake a filler"
        );
        // The asker takes its messages one by one; the reply enters as slots
        // free, behind the four fillers, and the bound holds all the way.
        hold.add_permits(1);
        let mut peak = 0;
        until("the reply is handled", || {
            peak = peak.max(rig.asker.mailbox_len());
            !rig.replies().is_empty()
        })
        .await;
        assert!(peak <= 4, "the mailbox held {peak} of 4");
        let entries = rig.entries();
        let order: Vec<_> = entries
            .iter()
            .map(|entry| match entry {
                Entry::Data(value) => *value,
                Entry::Reply { .. } => 100,
                _ => 0,
            })
            .collect();
        assert_eq!(order, vec![1, 2, 3, 4, 100]);
        assert!(call.await.unwrap().is_ok());
    });
}

/// The reply enters after every message the callee sent the asker before it
/// answered, also when the asker has a priority lane.
#[test]
fn reply_follows_the_callees_earlier_messages() {
    common::run(4, async {
        for lane in [false, true] {
            let config = if lane {
                capacity(8).with_priority_lane(8)
            } else {
                capacity(8)
            };
            let rig = Rig::start(config).await;
            let (callee, _) = new_callee();
            let callee = callee.spawn().await.unwrap();
            let hold = Arc::new(Semaphore::new(0));
            let call = tokio::spawn({
                let asker = rig.asker.clone();
                let (callee, hold) = (callee.clone(), hold.clone());
                let target = rig.asker.clone();
                async move {
                    asker
                        .send(Msg::Hold {
                            callee,
                            req: Req::Tell(target, 1, 2),
                            hold,
                            cancel: false,
                        })
                        .await
                }
            });
            // Both the message and the reply are queued while the asker is
            // parked, so the order they are taken in is the queues' order.
            until("the message and the reply are queued", || {
                rig.asker.mailbox_len() + rig.asker.priority_len() == 2
            })
            .await;
            hold.add_permits(1);
            let (_, result) = rig.one_reply().await;
            assert_eq!(result.unwrap(), 2);
            let entries = rig.entries();
            assert!(
                matches!(entries.first(), Some(Entry::Data(1))),
                "lane {lane}: the callee's message comes first: {entries:?}"
            );
            assert!(call.await.unwrap().is_ok());
        }
    });
}

// ---------------------------------------------------------------------------
// The callee's end
// ---------------------------------------------------------------------------

/// Whatever ends the callee's side of a request, the asker gets exactly one
/// reply, with the same result `send` would have returned.
#[test]
fn callee_end_is_one_reply() {
    #[derive(Debug, Clone, Copy)]
    enum Row {
        ReturnsErr,
        Panics,
        KilledBeforeReplying,
        Stopped,
        ClosedMailbox,
    }
    common::run(4, async {
        for row in [
            Row::ReturnsErr,
            Row::Panics,
            Row::KilledBeforeReplying,
            Row::Stopped,
            Row::ClosedMailbox,
        ] {
            let rig = Rig::start(ActorConfig::default()).await;
            let (callee, handled) = new_callee();
            let callee = callee.spawn().await.unwrap();
            let gate = Arc::new(Semaphore::new(0));
            let id = match row {
                Row::ReturnsErr => rig.id_of(ask(&callee, Req::Fail)).await,
                Row::Panics => rig.id_of(ask(&callee, Req::Panic)).await,
                Row::KilledBeforeReplying => {
                    let id = rig.id_of(ask(&callee, Req::Hang)).await;
                    until("the callee is inside the request", || {
                        handled.load(Ordering::SeqCst) == 1
                    })
                    .await;
                    callee.stop(StopReason::Kill).await.unwrap();
                    id
                }
                Row::Stopped => {
                    // The first request holds the callee; ours waits behind
                    // it, and a graceful stop ends the callee before it is
                    // taken.
                    rig.id_of(ask(&callee, Req::Gated(gate.clone(), 1))).await;
                    until("the callee is inside the first request", || {
                        handled.load(Ordering::SeqCst) == 1
                    })
                    .await;
                    let id = rig.id_of(ask(&callee, Req::Echo(2))).await;
                    until("our request is queued", || callee.mailbox_len() == 1).await;
                    callee.stop(StopReason::Graceful).await.unwrap();
                    gate.add_permits(1);
                    id
                }
                Row::ClosedMailbox => {
                    callee.stop(StopReason::Graceful).await.unwrap();
                    within("the callee ends", callee.wait_stopped()).await;
                    rig.id_of(ask(&callee, Req::Echo(1))).await
                }
            };
            until("a reply for the request", || {
                rig.replies().iter().any(|(reply_id, _)| *reply_id == id)
            })
            .await;
            let replies = rig.replies();
            let (_, result) = replies
                .iter()
                .find(|(reply_id, _)| *reply_id == id)
                .expect("the reply");
            match (row, result) {
                (Row::ReturnsErr, Err(AskError::Actor(ActorError::User(_)))) => {}
                (Row::Panics, Err(AskError::Actor(ActorError::Panic(_)))) => {}
                (
                    Row::KilledBeforeReplying,
                    Err(AskError::ResponseDropped {
                        reason: StopReason::Kill,
                    }),
                ) => {}
                (
                    Row::Stopped,
                    Err(AskError::ResponseDropped {
                        reason: StopReason::Graceful,
                    }),
                ) => {}
                (Row::ClosedMailbox, Err(AskError::Closed)) => {}
                (row, result) => panic!("{row:?}: unexpected result {result:?}"),
            }
            // Delivered once: the probe is handled after anything the task
            // could still have queued for this request.
            rig.replies_seen().await;
            let ours = rig
                .replies()
                .iter()
                .filter(|(reply_id, _)| *reply_id == id)
                .count();
            assert_eq!(ours, 1, "{row:?}");
        }
    });

    fn ask(callee: &ActorHandle<Callee>, req: Req) -> Msg {
        Msg::Ask {
            callee: callee.clone(),
            req,
            within: None,
        }
    }
}

// ---------------------------------------------------------------------------
// The cap
// ---------------------------------------------------------------------------

/// In flight is capped at the mailbox capacity unless `set_request_cap` says
/// otherwise; a request over the cap is refused, and the count returns after a
/// reply and after a cancel.
#[test]
fn in_flight_is_capped_and_returns() {
    // (mailbox capacity, cap to set, requests that fit)
    let rows: [(usize, Option<usize>, usize); 4] = [
        (4, None, 4),
        (4, Some(2), 2),
        (4, Some(0), 1),
        (4, Some(8), 8),
    ];
    common::run(4, async {
        for (mailbox, cap, fits) in rows {
            let rig = Rig::start(capacity(mailbox)).await;
            let (callee, _) = new_callee();
            let callee = callee.spawn().await.unwrap();
            let gate = Arc::new(Semaphore::new(0));
            let fire = |count| Msg::Fire {
                callee: callee.clone(),
                gate: gate.clone(),
                count,
                cap,
            };
            let ids = match within("the first round", rig.asker.send(fire(fits + 3)))
                .await
                .unwrap()
            {
                Resp::Fired { made, ids, refused } => {
                    assert_eq!(made, fits, "mailbox {mailbox}, cap {cap:?}");
                    assert_eq!(refused, Some(RequestError::InFlightLimit));
                    ids
                }
                other => panic!("expected a round, got {other:?}"),
            };
            assert_eq!(rig.in_flight().await, fits);
            // A cancel returns its place to the count.
            match within("a cancel", rig.asker.send(Msg::Cancel(ids[0])))
                .await
                .unwrap()
            {
                Resp::Cancelled(true) => {}
                other => panic!("the cancel was in time: {other:?}"),
            }
            assert_eq!(rig.in_flight().await, fits - 1);
            match within("a round after the cancel", rig.asker.send(fire(5)))
                .await
                .unwrap()
            {
                Resp::Fired { made, refused, .. } => {
                    assert_eq!((made, refused), (1, Some(RequestError::InFlightLimit)));
                }
                other => panic!("expected a round, got {other:?}"),
            }
            // Replies return their places too.
            gate.add_permits(64);
            until("every reply is handled", || rig.replies().len() == fits).await;
            assert_eq!(rig.in_flight().await, 0);
            match within("a round after the replies", rig.asker.send(fire(fits + 3)))
                .await
                .unwrap()
            {
                Resp::Fired { made, .. } => {
                    assert_eq!(made, fits, "mailbox {mailbox}, cap {cap:?}")
                }
                other => panic!("expected a round, got {other:?}"),
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Cancel
// ---------------------------------------------------------------------------

/// A cancel in time says `true` and no reply arrives, though the callee still
/// handles the request; a cancel after the reply was queued says `false` and
/// the queued reply is dropped, never handled.
#[test]
fn cancel_drops_a_queued_reply_and_reports_it() {
    common::run(4, async {
        // Row 1: the callee has not answered.
        {
            let rig = Rig::start(ActorConfig::default()).await;
            let (callee, handled) = new_callee();
            let callee = callee.spawn().await.unwrap();
            let gate = Arc::new(Semaphore::new(0));
            let id = rig
                .id_of(Msg::Ask {
                    callee: callee.clone(),
                    req: Req::Gated(gate.clone(), 1),
                    within: None,
                })
                .await;
            until("the callee has the request", || {
                handled.load(Ordering::SeqCst) == 1
            })
            .await;
            match within("the cancel", rig.asker.send(Msg::Cancel(id)))
                .await
                .unwrap()
            {
                Resp::Cancelled(true) => {}
                other => panic!("the cancel was in time: {other:?}"),
            }
            gate.add_permits(1);
            until("the callee finishes the request", || {
                callee.mailbox_len() == 0 && gate.available_permits() == 0
            })
            .await;
            assert_eq!(rig.replies_seen().await, 0, "no reply after a cancel");
            assert_eq!(rig.in_flight().await, 0);
            assert_eq!(handled.load(Ordering::SeqCst), 1, "the callee handled it");
        }
        // Row 2: the reply is queued when the cancel comes. The asker is inside
        // the turn that made the request, so the one slot its mailbox counts
        // is the reply's. The reply's task takes that slot before it claims
        // the request, so a cancel made once the slot shows can still land in
        // time; it then answers true, and the round is run again with a new
        // asker and callee.
        let mut claimed = false;
        for _ in 0..ROUNDS {
            let rig = Rig::start(ActorConfig::default()).await;
            let (callee, handled) = new_callee();
            let callee = callee.spawn().await.unwrap();
            let hold = Arc::new(Semaphore::new(0));
            let call = tokio::spawn({
                let asker = rig.asker.clone();
                let (callee, hold) = (callee.clone(), hold.clone());
                async move {
                    asker
                        .send(Msg::Hold {
                            callee,
                            req: Req::Echo(1),
                            hold,
                            cancel: true,
                        })
                        .await
                }
            });
            until("the callee has the request", || {
                handled.load(Ordering::SeqCst) == 1
            })
            .await;
            until("the reply is queued", || rig.asker.mailbox_len() == 1).await;
            hold.add_permits(1);
            match within("the cancel", call).await.unwrap().unwrap() {
                // The cancel claimed the request before the reply's task did.
                Resp::Cancelled(true) => continue,
                Resp::Cancelled(false) => {}
                other => panic!("expected the cancel's answer, got {other:?}"),
            }
            assert_eq!(rig.replies_seen().await, 0, "the queued reply was dropped");
            assert_eq!(rig.in_flight().await, 0);
            claimed = true;
            break;
        }
        assert!(claimed, "no round claimed the reply before the cancel");
    });
}

// ---------------------------------------------------------------------------
// Deadlines
// ---------------------------------------------------------------------------

/// A deadline that passes first gives one `Timeout`, and the late answer is
/// discarded.
#[test]
fn deadline_gives_one_timeout() {
    common::run_paused(async {
        // Row 1: the callee has the request and is slow.
        {
            let rig = Rig::start(ActorConfig::default()).await;
            let (callee, _) = new_callee();
            let callee = callee.spawn().await.unwrap();
            rig.id_of(Msg::Ask {
                callee,
                req: Req::Sleep(Duration::from_secs(3600), 1),
                within: Some(Duration::from_millis(50)),
            })
            .await;
            let (_, result) = rig.one_reply().await;
            assert!(
                matches!(result, Err(AskError::Timeout { enqueued: true })),
                "{result:?}"
            );
            // The callee answers an hour in; nothing more reaches the asker.
            sleep(Duration::from_secs(7200)).await;
            assert_eq!(rig.replies_seen().await, 1);
        }
        // Row 2: the request cannot be enqueued before the deadline.
        {
            let rig = Rig::start(ActorConfig::default()).await;
            let (callee, handled) = new_callee();
            let callee = callee.spawn().with_config(capacity(1)).await.unwrap();
            let gate = Arc::new(Semaphore::new(0));
            rig.id_of(Msg::Ask {
                callee: callee.clone(),
                req: Req::Gated(gate.clone(), 1),
                within: None,
            })
            .await;
            until("the callee is inside the first request", || {
                handled.load(Ordering::SeqCst) == 1
            })
            .await;
            rig.id_of(Msg::Ask {
                callee: callee.clone(),
                req: Req::Echo(2),
                within: None,
            })
            .await;
            until("the second request fills the mailbox", || {
                callee.mailbox_len() == 1
            })
            .await;
            rig.id_of(Msg::Ask {
                callee: callee.clone(),
                req: Req::Echo(3),
                within: Some(Duration::from_millis(50)),
            })
            .await;
            let (_, result) = rig.one_reply().await;
            assert!(
                matches!(result, Err(AskError::Timeout { enqueued: false })),
                "{result:?}"
            );
            gate.add_permits(1);
        }
    });
}

// ---------------------------------------------------------------------------
// The stop
// ---------------------------------------------------------------------------

/// A reply pending when the asker stops is dropped under every stop reason,
/// queued or not, whether or not the stop-time stage runs; the callee still
/// handles the request. In the stage, the notice the asker is owed is
/// delivered while a reply beside it is not, and a request made from that
/// notice's turn is refused.
#[test]
fn pending_replies_drop_at_the_stop() {
    common::run(4, async {
        for reason in [StopReason::Graceful, StopReason::Kill] {
            for queued in [true, false] {
                for stage in [true, false] {
                    let rig = Rig::start(ActorConfig::default()).await;
                    let (callee, handled) = new_callee();
                    let callee = callee.spawn().await.unwrap();
                    let (victim, _) = new_callee();
                    let victim = victim.spawn().await.unwrap();
                    if stage {
                        // A monitor makes the stop-time stage run.
                        rig.asker.send(Msg::Watch(victim.clone())).await.unwrap();
                    }
                    let gate = Arc::new(Semaphore::new(0));
                    let hold = Arc::new(Semaphore::new(0));
                    let req = if queued {
                        Req::Echo(1)
                    } else {
                        Req::Gated(gate.clone(), 1)
                    };
                    let call = tokio::spawn({
                        let asker = rig.asker.clone();
                        let (callee, hold) = (callee.clone(), hold.clone());
                        async move {
                            asker
                                .send(Msg::Hold {
                                    callee,
                                    req,
                                    hold,
                                    cancel: false,
                                })
                                .await
                        }
                    });
                    // The asker is parked in its handler once the callee has the
                    // request, or once the reply is queued.
                    let owed = stage && matches!(reason, StopReason::Graceful);
                    if queued {
                        until("the reply is queued", || rig.asker.mailbox_len() == 1).await;
                    } else {
                        until("the callee has the request", || {
                            handled.load(Ordering::SeqCst) == 1
                        })
                        .await;
                    }
                    // The notice of a victim that ended is owed to the asker
                    // and waits in its mailbox beside the reply.
                    if owed {
                        victim.stop(StopReason::Graceful).await.unwrap();
                        within("the victim ends", victim.wait_stopped()).await;
                        until("the notice is queued", || {
                            rig.asker.mailbox_len() == 1 + usize::from(queued)
                        })
                        .await;
                    }
                    sleep(WINDOW).await;
                    rig.asker.stop(reason.clone()).await.unwrap();
                    hold.add_permits(1);
                    let ended = within("the asker ends", rig.asker.wait_stopped()).await;
                    assert_eq!(ended, reason);
                    let _ = within("the parked call returns", call).await;
                    gate.add_permits(1);
                    until("the callee handled the request", || {
                        handled.load(Ordering::SeqCst) == 1
                    })
                    .await;
                    sleep(WINDOW).await;
                    let row = format!("{reason:?} queued {queued} stage {stage}");
                    assert!(rig.replies().is_empty(), "{row}: a reply was delivered");
                    let refused = rig
                        .entries()
                        .iter()
                        .filter(|entry| matches!(entry, Entry::Refused(RequestError::Stopping)))
                        .count();
                    assert_eq!(refused, usize::from(owed), "{row}: {:?}", rig.entries());
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// A map that panics
// ---------------------------------------------------------------------------

/// A panic in the map function fails the asker with that panic.
#[test]
fn map_panic_fails_the_owner() {
    common::run(2, async {
        let rig = Rig::start(ActorConfig::default()).await;
        let (callee, _) = new_callee();
        let callee = callee.spawn().await.unwrap();
        rig.asker.send(Msg::PanicOn(callee)).await.unwrap();
        let ended = within("the asker fails", rig.asker.wait_stopped()).await;
        match ended {
            StopReason::Failure(ActorError::Panic(message)) => {
                assert!(message.contains("boom"), "{message}");
            }
            other => panic!("expected the map's panic, got {other:?}"),
        }
    });
}

// ---------------------------------------------------------------------------
// Oneself
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The snapshot
// ---------------------------------------------------------------------------

/// `requests_in_flight` counts the requests made and not yet answered or
/// cancelled.
#[test]
fn snapshot_requests_in_flight_agrees() {
    common::run(2, async {
        let rig = Rig::start(ActorConfig::default()).await;
        let (callee, _) = new_callee();
        let callee = callee.spawn().await.unwrap();
        assert_eq!(rig.in_flight().await, 0);
        let gate = Arc::new(Semaphore::new(0));
        let ids = match rig
            .asker
            .send(Msg::Fire {
                callee: callee.clone(),
                gate: gate.clone(),
                count: 3,
                cap: None,
            })
            .await
            .unwrap()
        {
            Resp::Fired { ids, .. } => ids,
            other => panic!("expected a round, got {other:?}"),
        };
        assert_eq!(rig.in_flight().await, 3);
        rig.asker.send(Msg::Cancel(ids[0])).await.unwrap();
        assert_eq!(rig.in_flight().await, 2);
        gate.add_permits(3);
        until("the replies are handled", || rig.replies().len() == 2).await;
        assert_eq!(rig.in_flight().await, 0);
    });
}

// ---------------------------------------------------------------------------
// A count row for the cost gate
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Requests with the idle spell, `continue_with` and a supervisor
// ---------------------------------------------------------------------------

enum XMsg {
    Reply(RequestReply<u32>),
    Work,
    Tick,
    Step,
    AskSup(ActorHandle<Sup>),
}

/// What the cross rows watch: each entry with the virtual time it happened at.
type XLog = Arc<Mutex<Vec<(String, Duration)>>>;

struct X {
    log: XLog,
    t0: Instant,
    /// A request made in `on_started`: the callee and the gate it answers at.
    ask: Option<(ActorHandle<Callee>, Arc<Semaphore>)>,
    /// A receive timeout set in `on_started`.
    receive: Option<Duration>,
    /// The reply handler calls `continue_with(Step)`.
    continue_on_reply: bool,
    /// The reply handler says it entered, then waits to be released.
    hold: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
}

impl X {
    fn new() -> Self {
        X {
            log: Arc::default(),
            t0: Instant::now(),
            ask: None,
            receive: None,
            continue_on_reply: false,
            hold: None,
        }
    }

    fn note(&self, what: impl Into<String>) {
        self.log
            .lock()
            .unwrap()
            .push((what.into(), self.t0.elapsed()));
    }
}

impl Actor for X {
    type Message = XMsg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if let Some((callee, gate)) = self.ask.take() {
            ctx.request(&callee, Req::Gated(gate, 7), XMsg::Reply)
                .expect("the asker is running");
        }
        if let Some(after) = self.receive {
            ctx.receive_timeout(after, || XMsg::Tick);
        }
        Ok(())
    }

    async fn handle(&mut self, msg: XMsg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            XMsg::Reply(reply) => {
                self.note(format!("reply:{:?}", reply.result));
                if self.continue_on_reply {
                    ctx.continue_with(XMsg::Step);
                }
                if let Some((entered, release)) = self.hold.take() {
                    let _ = entered.send(());
                    let _ = release.await;
                }
            }
            XMsg::Work => self.note("work"),
            XMsg::Tick => self.note("tick"),
            XMsg::Step => self.note("step"),
            XMsg::AskSup(sup) => {
                ctx.request(&sup, 21, XMsg::Reply)
                    .expect("the asker is running");
            }
        }
        Ok(())
    }
}

fn entries(log: &XLog) -> Vec<(String, Duration)> {
    log.lock().unwrap().clone()
}

fn labels(log: &XLog) -> Vec<String> {
    entries(log).into_iter().map(|(what, _)| what).collect()
}

/// Lets every ready task run.
async fn settle() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

/// Moves the paused clock in steps of 10 ms, letting every task run between
/// two, so an event is stamped with the instant of its deadline.
async fn advance(by: Duration) {
    let step = Duration::from_millis(10);
    let mut left = by;
    while !left.is_zero() {
        let now = left.min(step);
        tokio::time::advance(now).await;
        settle().await;
        left -= now;
    }
}

/// A supervisor.
struct Leaf;

impl Actor for Leaf {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// A supervisor that answers `2 * n + children`, starting its one child at
/// the first message.
struct Sup {
    spawned: bool,
}

impl Actor for Sup {
    type Message = u32;
    type Response = u32;

    async fn handle(&mut self, n: u32, ctx: &mut ActorContext<Self>) -> ActorResult<u32> {
        if !self.spawned {
            ctx.spawn_child(|| Leaf).named("leaf").await?;
            self.spawned = true;
        }
        Ok(2 * n + ctx.children().len() as u32)
    }
}

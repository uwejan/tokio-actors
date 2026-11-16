//! A registration made while the actor stops is refused (an owed notice) or
//! accepted and silent (a source of its own); a revoke answers `true` for a
//! pending item and the family's answer once the item is queued.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_actors::telemetry::{Filter, Scope};
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorHandle, ActorResult, ActorSystem, AliasError, AliasOptions, PgScope, StopReason,
    StreamEvent,
};
use tokio_stream::StreamExt;

type Row = (&'static str, &'static str, bool);
type Ctx = ActorContext<Probe>;
type Rev = Box<dyn Fn(&mut Ctx) -> bool + Send>;

/// (family, refused while stopping, the revoke's answer once an item is queued)
const TABLE: [(&str, bool, Option<bool>); 9] = [
    ("monitor", true, Some(false)),
    ("link", true, Some(false)),
    ("request", true, Some(false)),
    ("group", true, None),
    ("oneshot", false, Some(false)),
    ("recurring", false, Some(true)),
    ("stream", false, Some(true)),
    ("alias", false, Some(true)),
    ("sub", false, Some(true)),
];

#[derive(Clone, Copy)]
enum M {
    Go,
    Ev(&'static str),
}

impl From<StreamEvent<u8>> for M {
    fn from(_: StreamEvent<u8>) -> M {
        M::Ev("stream")
    }
}

/// Answers at once, or never when it holds the flag.
struct Target(bool);

impl Actor for Target {
    type Message = ();
    type Response = u8;
    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<u8> {
        if self.0 {
            std::future::pending::<()>().await;
        }
        Ok(7)
    }
}

fn rev<I: Send + 'static>(id: I, f: fn(&mut Ctx, &I) -> bool) -> Result<Rev, String> {
    Ok(Box::new(move |c| f(c, &id)))
}

fn st(e: impl std::fmt::Debug) -> String {
    format!("{e:?}")
}

/// Registers one item of `fam` and returns its revoke, or the registration's
/// error. A `due` item has produced, or is about to produce, its notice.
async fn item(
    fam: &'static str,
    c: &mut Ctx,
    t: &ActorHandle<Target>,
    due: bool,
    pg: &PgScope,
) -> Result<Rev, String> {
    let (ev, wait) = (
        M::Ev(fam),
        Duration::from_millis(if due { 1 } else { 60_000 }),
    );
    match fam {
        "monitor" => rev(c.monitor(t, move |_| ev).map_err(st)?, |c, i| {
            c.demonitor(*i)
        }),
        "request" => rev(c.request(t, (), move |_| ev).map_err(st)?, |c, i| {
            c.cancel_request(*i)
        }),
        "group" => rev(
            c.monitor_group(pg, "g", move |_| ev).map_err(st)?.0,
            |c, i| c.demonitor(*i),
        ),
        "link" => {
            c.trap_exit(move |_| ev);
            c.link(t).map_err(st)?;
            rev(t.clone(), |c, k| c.unlink(k))
        }
        "oneshot" => rev(c.schedule(ev).after(wait).await.map_err(st)?, |c, i| {
            c.cancel_timer(*i)
        }),
        "recurring" => rev(c.schedule(ev).every(wait).await.map_err(st)?, |c, i| {
            c.cancel_timer(*i)
        }),
        "stream" => {
            let id = if due {
                c.add_stream(tokio_stream::iter([1u8]).chain(tokio_stream::pending()))
            } else {
                c.add_stream(tokio_stream::pending::<u8>())
            };
            rev(id, |c, i| c.cancel_stream(*i))
        }
        "alias" => {
            let a = c.alias(AliasOptions::default());
            if due {
                a.try_notify(ev).unwrap();
            }
            rev(a, |c, a| c.unalias(a))
        }
        _ => rev(
            c.subscribe(Filter::new(Scope::System), move |_| ev),
            |c, i| c.unsubscribe(*i),
        ),
    }
}

struct Probe {
    tx: mpsc::UnboundedSender<Row>,
    pg: PgScope,
    t: [ActorHandle<Target>; 2],
    late: HashMap<&'static str, Rev>,
}

impl Actor for Probe {
    type Message = M;
    type Response = ();

    async fn handle(&mut self, msg: M, c: &mut Ctx) -> ActorResult<()> {
        let put = |f, cell, v| self.tx.send((f, cell, v)).expect("the test listens");
        let [t1, t2] = self.t.clone();
        match msg {
            M::Ev(fam) => {
                if let Some(rev) = self.late.remove(fam) {
                    put(fam, "queued", rev(c));
                }
            }
            M::Go => {
                for (fam, _, queued) in TABLE {
                    let rev = item(fam, c, &t1, false, &self.pg).await.unwrap();
                    put(fam, "pending", rev(c));
                    put(fam, "again", rev(c));
                    if queued.is_some() {
                        self.late
                            .insert(fam, item(fam, c, &t2, true, &self.pg).await.unwrap());
                    }
                }
                let a = c.alias(AliasOptions::default());
                let ended = c.unalias(&a);
                put(
                    "alias",
                    "exact",
                    ended && matches!(a.try_notify(M::Go), Err(AliasError::Inactive)),
                );
            }
        }
        Ok(())
    }

    async fn on_stopped(&mut self, _: &StopReason, c: &mut Ctx) -> ActorResult<()> {
        let (t, pg) = (self.t[0].clone(), self.pg.clone());
        for (fam, _, _) in TABLE {
            let refused = item(fam, c, &t, false, &pg).await.err();
            let refused = refused.is_some_and(|e| e.contains("Stopping"));
            self.tx
                .send((fam, "stopping", refused))
                .expect("the test listens");
        }
        Ok(())
    }
}

async fn collect(
    rx: &mut mpsc::UnboundedReceiver<Row>,
    rows: &mut BTreeMap<(&str, &str), bool>,
    n: usize,
) {
    while rows.len() < n {
        let row = tokio::time::timeout(Duration::from_secs(30), rx.recv()).await;
        let (fam, cell, v) = row.expect("a row arrives").expect("the probe lives");
        rows.insert((fam, cell), v);
    }
}

#[test]
fn signal_call_shape() {
    common::run(4, async {
        let sys = ActorSystem::create("signal-call-shape").expect("a fresh system");
        let t1 = Target(true).spawn().on_system(&sys).await.unwrap();
        let t2 = Target(false).spawn().on_system(&sys).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let probe = Probe {
            tx,
            pg: sys.pg(),
            t: [t1, t2.clone()],
            late: HashMap::new(),
        };
        let probe = probe.spawn().on_system(&sys).await.unwrap();
        let mut rows = BTreeMap::new();
        probe.send(M::Go).await.unwrap();
        collect(&mut rx, &mut rows, 19).await;
        t2.stop(StopReason::Graceful).await.unwrap();
        collect(&mut rx, &mut rows, 27).await;
        probe.stop(StopReason::Graceful).await.unwrap();
        collect(&mut rx, &mut rows, 36).await;
        for (fam, refused, queued) in TABLE {
            assert_eq!(rows[&(fam, "stopping")], refused, "{fam} stopping");
            assert!(rows[&(fam, "pending")], "{fam} pending revoke");
            assert!(!rows[&(fam, "again")], "{fam} second revoke");
            assert_eq!(rows.get(&(fam, "queued")).copied(), queued, "{fam} queued");
        }
        assert!(rows[&("alias", "exact")], "no send enters after unalias");
    });
}

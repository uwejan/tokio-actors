//! Scoped `tracing` subscribers for unit tests.
//!
//! The unit tests of one binary share the process-wide interest cache of every
//! `tracing` callsite. A callsite caches what the registered subscribers
//! answered when it was first hit or when a subscriber was registered: a
//! scoped subscriber that takes everything leaves "always" behind after its
//! scope, so a thread with no subscriber then builds spans and identities; a
//! callsite first hit by a thread with no subscriber while a scoped subscriber
//! is the only one registered is cached as "never", so that subscriber's own
//! thread then records nothing.
//!
//! A permanent registered subscriber that answers "sometimes" for every
//! callsite keeps every cached answer dynamic: each hit asks the subscriber of
//! the thread that makes it, so what one test installs never changes what
//! another thread sees. Every unit test that installs a scoped subscriber goes
//! through [`install`].

use std::sync::OnceLock;

use tracing::subscriber::DefaultGuard;
use tracing::subscriber::Interest;
use tracing::{span, Dispatch, Event, Metadata, Subscriber};

/// Answers "sometimes" for every callsite and takes nothing itself.
struct Dynamic;

impl Subscriber for Dynamic {
    fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        false
    }
    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }
    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn event(&self, _: &Event<'_>) {}
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}
}

/// Makes `subscriber` the subscriber of the calling thread until the guard
/// drops, with every callsite's cached answer dynamic from the first call on.
pub(crate) fn install<S>(subscriber: S) -> DefaultGuard
where
    S: Subscriber + Send + Sync + 'static,
{
    static DYNAMIC: OnceLock<Dispatch> = OnceLock::new();
    DYNAMIC.get_or_init(|| Dispatch::new(Dynamic));
    tracing::subscriber::set_default(subscriber)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};

    use super::*;
    use crate::actor::context::ActorContext;
    use crate::actor::{Actor, ActorExt};
    use crate::error::ActorResult;
    use crate::system::ActorSystem;
    use crate::types::StopReason;

    struct Idle;

    impl Actor for Idle {
        type Message = ();
        type Response = ();

        async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
            Ok(())
        }
    }

    /// Takes every callsite and every event, and counts the events.
    #[derive(Clone, Default)]
    struct TakesAll {
        events: Arc<AtomicUsize>,
    }

    impl Subscriber for TakesAll {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn event(&self, _: &Event<'_>) {
            self.events.fetch_add(1, Ordering::SeqCst);
        }
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }

    /// One row: its name and the check it runs.
    type Row = (&'static str, fn() -> Result<(), String>);

    /// Spawns one actor on a runtime of the calling thread and returns how
    /// many identities its system holds while it runs.
    fn identities_while_an_actor_runs() -> usize {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let name = format!(
                "isolation-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let sys = ActorSystem::create(name).unwrap();
            let handle = Idle.spawn().on_system(&sys).await.unwrap();
            let identities = sys.telemetry_mode().live_len();
            handle.stop(StopReason::Graceful).await.unwrap();
            handle.wait_stopped().await;
            identities
        })
    }

    /// The one callsite both threads of the second row hit.
    fn log_one_line() {
        tracing::warn!("a line");
    }

    /// A subscriber installed for one thread leaves nothing behind for the
    /// others, first in what a thread with no subscriber records and then in
    /// what a thread installing its subscriber later sees.
    fn a_thread_without_a_subscriber_records_no_identity() -> Result<(), String> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (end_tx, end_rx) = mpsc::channel::<()>();
        let installer = std::thread::spawn(move || {
            let guard = install(TakesAll::default());
            ready_tx.send(identities_while_an_actor_runs()).unwrap();
            end_rx.recv().unwrap();
            drop(guard);
        });
        let installed = ready_rx.recv().unwrap();
        let during = std::thread::spawn(identities_while_an_actor_runs)
            .join()
            .unwrap();
        end_tx.send(()).unwrap();
        installer.join().unwrap();
        let after = std::thread::spawn(identities_while_an_actor_runs)
            .join()
            .unwrap();
        if installed == 0 {
            return Err("the installed subscriber recorded no identity".to_string());
        }
        if (during, after) != (0, 0) {
            return Err(format!(
                "identities recorded with no subscriber: {during} while the scope was open, {after} after it"
            ));
        }
        Ok(())
    }

    fn a_callsite_first_hit_by_a_thread_without_a_subscriber_still_logs() -> Result<(), String> {
        let subscriber = TakesAll::default();
        let events = subscriber.events.clone();
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let installer = std::thread::spawn(move || {
            let _installed = install(subscriber);
            ready_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            log_one_line();
        });
        ready_rx.recv().unwrap();
        std::thread::spawn(log_one_line).join().unwrap();
        go_tx.send(()).unwrap();
        installer.join().unwrap();
        match events.load(Ordering::SeqCst) {
            1 => Ok(()),
            n => Err(format!(
                "the installer's own hit reached its subscriber {n} times, not once"
            )),
        }
    }

    /// What one thread's subscriber answers never decides what another
    /// thread's callsite hits record. Each row orders its threads by
    /// handshakes and names the property it pins.
    #[test]
    fn a_scoped_subscriber_changes_nothing_for_another_thread() {
        let rows: [Row; 2] = [
            (
                "a thread without a subscriber records no identity",
                a_thread_without_a_subscriber_records_no_identity,
            ),
            (
                "a callsite first hit without a subscriber still logs for a later installer",
                a_callsite_first_hit_by_a_thread_without_a_subscriber_still_logs,
            ),
        ];
        let failed: Vec<String> = rows
            .iter()
            .filter_map(|(name, row)| row().err().map(|why| format!("{name}: {why}")))
            .collect();
        assert!(failed.is_empty(), "{}", failed.join("; "));
    }
}

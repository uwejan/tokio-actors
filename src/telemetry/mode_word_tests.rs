//! The mode word: control bits, class bits, the generation and the hot
//! handler cache.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::actor_ref::ActorRef;
use super::event::{EventKind, HookKind, KindSet, TelemetryEvent, TurnOutcome};
use super::mode::{Mode, CTL_DEADLOCK, CTL_TRACK_TURNS, CTL_WATCHDOG};
use super::subscription::{self, Filter, Scope, SubEntry, SubQueue, SubscriptionId};
use crate::types::ActorId;

const CTL_ALL: u64 = CTL_WATCHDOG | CTL_TRACK_TURNS | CTL_DEADLOCK;

fn noop() -> Arc<dyn Fn(&TelemetryEvent<'_>) + Send + Sync> {
    Arc::new(|_| {})
}

fn actor() -> ActorRef {
    ActorRef::new_root(ActorId::from("mode-word"), 1)
}

fn turn_event() -> TelemetryEvent<'static> {
    TelemetryEvent::TurnFinished {
        actor: Cow::Owned(actor()),
        hook: HookKind::Handle,
        took: Duration::ZERO,
        outcome: TurnOutcome::Returned,
    }
}

/// Lets the helper threads of a test go when the test ends, however it ends.
struct Release(Arc<AtomicBool>, Arc<AtomicBool>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
        self.1.store(true, Ordering::Release);
    }
}

fn sub_entry(id: u64, filter: Filter) -> SubEntry {
    let (queue, _rx) = SubQueue::new(4);
    SubEntry {
        id: SubscriptionId(id),
        watcher: actor(),
        filter,
        queue,
    }
}

#[test]
fn a_filter_that_names_a_hot_kind_keeps_no_hot_kind() {
    let mode = Mode::new();
    let all = Filter::new(Scope::System).kinds(KindSet::all());
    subscription::register(&mode, sub_entry(1, all));
    for kind in [
        EventKind::TurnFinished,
        EventKind::RunFinished,
        EventKind::TimerTick,
    ] {
        assert!(!mode.armed(kind), "{kind:?} armed by a subscription");
    }
    assert!(mode.armed(EventKind::ActorSpawned), "the cold kinds stay");
    let kept = mode
        .sinks
        .tables
        .lock()
        .unwrap()
        .subs
        .first()
        .unwrap()
        .filter
        .kinds;
    assert_eq!(kept, KindSet::cold());
    assert_eq!(Filter::new(Scope::System).kinds, KindSet::cold());
}

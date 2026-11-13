//! Typed events about the actors of a system: their ends, stop requests,
//! supervision decisions and helper tasks, with each actor's ancestry.
//!
//! # Fields of the `tracing` output
//!
//! With the `tracing` feature, a log statement about one actor carries these
//! fields, except the statements named below:
//!
//! | Field | Value |
//! |---|---|
//! | `actor_id` | the actor's id |
//! | `actor_name` | its registered name, empty for an anonymous actor |
//! | `incarnation` | its incarnation number |
//! | `system` | the name of its system |
//!
//! A statement that names a supervisor and a child carries `supervisor`,
//! `child` and `incarnation` instead. A statement written where only the
//! actor's id is known (the panic of a waker the crate wakes, a helper task's
//! panic, a panic discarded while the actor's end drops its mail, a drop that
//! panics at the abort or the unwind of the actor's task) carries `actor_id`
//! alone. The other statements carry the fields of an actor that was spawned
//! while a sink took events or a subscriber wanted its span or any of its
//! statements.
//!
//! Each actor's task runs inside a span named `actor` at debug level, with no
//! parent, carrying `actor.id`, `actor.name`, `actor.incarnation` and
//! `system`. The span is created at the spawn only when a subscriber is
//! interested in it, and is entered on every poll of the task. The report of
//! a child that ended unexpectedly while its supervisor stopped it is an
//! error on the `tokio_actors::supervisor` target, with the `report` field
//! `shutdown_error`.
//!
//! Crash, supervisor and progress reports are statements of their own: they
//! raise no event and run no handler, and nothing is formatted for them unless
//! a subscriber takes their target and level. The `format_status` call a crash
//! report makes is a turn of kind `Sys` like any other.
//!
//! * An actor that ends with a failure, or with the death of its supervisor,
//!   writes one error on the `tokio_actors::crash` target with its id, name,
//!   incarnation, system, reason, the text its `format_status` returns and
//!   the entries of its log, as an Erlang/OTP process that crashes writes its
//!   crash report. An end by `Graceful`, `ParentRequest` or `Cancelled`, and a
//!   `Kill`, writes none, and `format_status` is called for the report only
//!   when a subscriber takes the target.
//! * A supervisor writes, on the `tokio_actors::supervisor` target with a
//!   `report` field, `child_terminated` for every exit of a permanent child
//!   and for every exit of a transient or temporary one that is not clean
//!   ([`StopReason::is_normal`](crate::StopReason::is_normal)), the reading
//!   a transient child's restart decision makes; `start_error` for a child that fails to
//!   start or whose `spawn_child` is refused; and
//!   `reached_max_restart_intensity` once, at the charge that exhausts its
//!   budget; all at error, as the reports of the Erlang/OTP supervisor are.
//!   It writes `progress` at info for a restart it decided and at debug for a
//!   child started or restarted by a call.
//! * The statements that carry an actor's identity fields print the word
//!   `error` or `panic`, and not the text of the failure, for a sensitive
//!   actor.

#[allow(dead_code)]
mod actor_ref;
#[allow(dead_code)]
mod event;
#[allow(dead_code)]
pub(crate) mod handlers;
#[allow(dead_code)]
mod hot;
#[allow(dead_code)]
mod identity;
#[allow(dead_code)]
mod mode;
#[allow(dead_code)]
pub(crate) mod monitor;
#[allow(dead_code)]
mod pump;
#[allow(dead_code)]
mod record;
#[allow(dead_code)]
pub(crate) mod reports;
#[allow(dead_code)]
mod sinks;
#[allow(dead_code)]
mod span;
#[allow(dead_code)]
pub(crate) mod subscription;
#[allow(dead_code)]
pub(crate) mod token;
#[allow(dead_code)]
pub(crate) mod wait_graph;

#[cfg(test)]
mod events_tests;
#[cfg(test)]
mod gate_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod mode_word_tests;
#[cfg(test)]
mod pump_tests;
#[cfg(test)]
mod quiescence_tests;
#[cfg(test)]
mod sender_wait_tests;
#[cfg(test)]
mod snapshot_tests;
#[cfg(test)]
mod subscription_life_tests;
#[cfg(test)]
mod timer_tests;
#[cfg(test)]
mod turn_state_tests;
#[cfg(test)]
mod turn_tests;
#[cfg(test)]
mod wait_cycle_tests;

pub use actor_ref::ActorRef;
#[allow(unused_imports)]
pub(crate) use actor_ref::UNKNOWN_INCARNATION;
pub use event::{
    AttemptOutcome, EventKind, HelperEnd, HelperKind, HookKind, KillStepKind, KindSet,
    RaiseOutcome, StopOrigin, TelemetryEvent, TurnOutcome, WaitOutcome,
};
pub use monitor::{LongQueue, MonitorConfig, MonitorError, MonitorSettings};
pub use record::ActorRecord;
#[allow(unused_imports)]
pub(crate) use record::CouplingInfo;
pub use subscription::{
    Delivery, Filter, Scope, SubscriptionId, SubscriptionNotice, MAX_SUBSCRIPTION_QUEUE,
};

#[allow(unused_imports)]
pub(crate) use hot::HotCache;
#[allow(unused_imports)]
pub(crate) use identity::{
    armed_for, death_claim, death_decided, death_landed, emit_hot, engaged_for, group_pending,
    identity_for, push_about, push_for, set_label, stage_about, supervisor_of, supervisor_ref,
    with_mode, Birth, Identity, PublishTo, Staged,
};
#[cfg(feature = "tracing")]
pub(crate) use identity::{log_fields, LogFields};
#[allow(unused_imports)]
pub(crate) use mode::{
    Mode, ModeOwner, CLASS_MASK, CTL_DEADLOCK, CTL_TRACK_TURNS, CTL_WATCHDOG, RESOLVING,
};
#[allow(unused_imports)]
pub(crate) use span::{ActorSpan, SpawnTraced};

#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use sinks::test_sink::TestSinkGuard;

/// The identity recorded for the incarnation behind `record` in `system`.
#[cfg(test)]
pub(crate) fn identity_of(
    system: &crate::system::ActorSystem,
    record: &crate::actor::subtree::AbortRecord,
) -> Option<std::sync::Arc<Identity>> {
    system.telemetry_mode().find(record.key())
}

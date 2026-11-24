//! Forwarder tasks of timers and streams: their slots, their end and the panic they park.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use super::build_for_attempt_of;
use super::ActorContext;
#[cfg(feature = "telemetry")]
use crate::actor::handle::ActorHandle;
use crate::actor::incarnation::{StatusTx, SubtreeHold};
use crate::actor::panic::{catch_sync, payload_into_string, PanicPayload};
#[cfg(test)]
use crate::actor::supervision::{RestartOutcome, SlotIo};
use crate::actor::{runtime, Actor};
use crate::error::ActorError;
use crate::telemetry::SubscriptionId;
#[cfg(feature = "telemetry")]
use crate::telemetry::{Delivery, SubscriptionNotice, TelemetryEvent};
use crate::time_probe::require_runtime_timers;
use crate::types::{ActorId, MonitorId, RecurringId, StreamId};

impl<A: Actor> ActorContext<A> {
    /// Whether no entry of the forwarder plane can be ready and the plane
    /// holds the task's waker wherever the select's poll would leave one:
    /// every forwarder end counted so far has had its entry taken, and the
    /// select has polled the plane since the last spawn and the last reap.
    pub(crate) fn forwarders_idle(&self) -> bool {
        let Some(extras) = self.extras.as_deref() else {
            return true;
        };
        if extras.forwarders_unpolled {
            return false;
        }
        match &extras.forwarders_done {
            None => true,
            Some(done) => {
                let done = done.load(Ordering::Acquire);
                debug_assert!(
                    extras.forwarders_taken <= done,
                    "a forwarder entry was taken before its end was counted"
                );
                done == extras.forwarders_taken
            }
        }
    }

    /// The run loop's select is polling the forwarder plane.
    pub(crate) fn note_forwarders_polled(&mut self) {
        if let Some(extras) = self.extras.as_deref_mut() {
            extras.forwarders_unpolled = false;
        }
    }

    /// Reaps one forwarder task's completion: removes its registration from
    /// `timers`/`streams` (a fired one-shot or a finished stream leaves zero
    /// registrations behind) and, if it ended in a panic, records it as
    /// [`last_forwarder_error`](Self::last_forwarder_error) and returns its
    /// payload. With the `tracing` feature every payload it records is
    /// logged once, naming the actor, whoever reaps it (the message loop
    /// during the run, or the end). A forwarder that panicked both before and
    /// after it saw the mailbox closed keeps the later panic; the earlier one
    /// is logged as discarded. A completion whose registration was already
    /// removed (an explicit `cancel_timer`/`cancel_stream` beat it here) is a
    /// harmless no-op.
    pub(crate) fn reap_forwarder(
        &mut self,
        outcome: Result<(tokio::task::Id, ForwarderEnd), JoinError>,
    ) -> Option<String> {
        {
            let extras = self.ex_mut();
            extras.forwarders_taken += 1;
            extras.forwarders_unpolled = true;
        }
        let (task_id, panic) = match outcome {
            Ok((id, ForwarderEnd::Done)) => (id, None),
            Ok((id, ForwarderEnd::Panicked(payload))) => (id, Some(payload)),
            // Read as the run's wherever it is reaped here; the end
            // classifies an end drop itself before reaping.
            Ok((id, ForwarderEnd::Released { run, end })) => match (end, run) {
                (Some(end), Some(run)) => {
                    #[cfg(feature = "tracing")]
                    let fields = self.log_fields();
                    #[cfg(feature = "tracing")]
                    caught_log!(warn, target: "tokio_actors::actor::context",
                        actor_id = %self.actor_id(),
                        actor_name = fields.name(),
                        incarnation = fields.incarnation(),
                        system = fields.system(),
                        panic = %fields.panic_text(&run),
                        "a timer or stream of the actor panicked during the run and again at the close; the earlier panic is discarded"
                    );
                    #[cfg(not(feature = "tracing"))]
                    let _ = run;
                    (id, Some(end))
                }
                (end, run) => (id, end.or(run)),
            },
            Err(err) => {
                let id = err.id();
                let panic = if err.is_panic() {
                    match err.try_into_panic() {
                        Ok(payload) => Some(payload_into_string(payload)),
                        Err(_) => Some("panic payload unavailable".to_string()),
                    }
                } else {
                    None
                };
                (id, panic)
            }
        };
        if let Some(kind) = self.ex_mut().forwarder_kinds.remove(&task_id) {
            match kind {
                ForwarderKind::Timer(id) => {
                    self.ex_mut().timers.remove(&id);
                }
                ForwarderKind::Stream(id) => {
                    self.ex_mut().streams.remove(&id);
                }
                // A monitor stays registered after its task ends, until its
                // notice is dispatched or the monitor is revoked, so that a
                // revoke can still reach a notice that is queued. Only a
                // task that ended by a panic has no notice left to wait for.
                ForwarderKind::Monitor(id) => {
                    if panic.is_some() {
                        if let Some(state) = self.monitors.as_deref_mut() {
                            state.remove(id);
                        }
                    }
                }
                // A link stays in the table after its task ends only while
                // its exit is queued or the stop-time stage owes it.
                ForwarderKind::Link(id) => {
                    if let Some(state) = self.monitors.as_deref_mut() {
                        state.links().reaped(id, panic.is_some());
                    }
                }
                // A group monitor stays registered after its task ends, until
                // it is revoked or the actor ends; only a task that ended by
                // a panic leaves nothing to deliver.
                ForwarderKind::Group(id) => {
                    if panic.is_some() {
                        if let Some(state) = self.monitors.as_deref_mut() {
                            state.groups.remove(id);
                        }
                    }
                }
                // A request stays in its table after its task ends, until
                // its reply is dispatched or it is cancelled, so that a
                // cancel can still reach a reply that is queued. Only a task
                // that ended by a panic has no reply left to wait for.
                ForwarderKind::Request(id) => {
                    if panic.is_some() {
                        if let Some(state) = self.monitors.as_deref_mut() {
                            state.requests.remove(id);
                        }
                    }
                }
                ForwarderKind::Subscription(id) => {
                    self.ex_mut().subscriptions.remove(&id);
                }
            }
        }
        if let Some(payload) = &panic {
            #[cfg(feature = "tracing")]
            let fields = self.log_fields();
            #[cfg(feature = "tracing")]
            caught_log!(warn, target: "tokio_actors::actor::context",
                actor_id = %self.actor_id(),
                actor_name = fields.name(),
                incarnation = fields.incarnation(),
                system = fields.system(),
                panic = %fields.panic_text(payload),
                "a timer or stream of the actor panicked"
            );
            self.ex_mut().last_forwarder_error = Some(ActorError::Panic(payload.clone()));
        }
        panic
    }

    /// Whether the timer or stream a forwarder belongs to is still
    /// registered, that is, neither cancelled nor reaped.
    fn is_registered(&self, kind: &ForwarderKind) -> bool {
        match kind {
            ForwarderKind::Timer(id) => self.ex().timers.contains_key(id),
            ForwarderKind::Stream(id) => self.ex().streams.contains_key(id),
            ForwarderKind::Monitor(id) => self
                .monitors
                .as_deref()
                .is_some_and(|state| state.contains(*id)),
            ForwarderKind::Link(id) => self
                .monitors
                .as_deref()
                .is_some_and(|state| state.holds_link(*id)),
            ForwarderKind::Group(id) => self
                .monitors
                .as_deref()
                .is_some_and(|state| state.groups.contains(*id)),
            ForwarderKind::Request(id) => self
                .monitors
                .as_deref()
                .is_some_and(|state| state.requests.contains(*id)),
            ForwarderKind::Subscription(id) => self.ex().subscriptions.contains_key(id),
        }
    }

    /// Spawns a forwarder task onto the forwarder plane, remembering which
    /// registration it belongs to so a later completion - success, or a
    /// caught-by-tokio panic - can find its way back to the right entry in
    /// `timers`/`streams`.
    pub(super) fn spawn_forwarder<F>(&mut self, kind: ForwarderKind, fut: F, slot: PanicSlot)
    where
        F: std::future::Future<Output = Released> + Send + 'static,
    {
        let done = self
            .ex_mut()
            .forwarders_done
            .get_or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone();
        let forwarder = Forwarder {
            done,
            counted: false,
            future: Some(Box::pin(fut)),
            slot,
            owner: self.owner(),
            _status: self.status_tx.clone(),
            _subtree: self.subtree.clone(),
        };
        let runtime = self.runtime.clone();
        let extras = self.ex_mut();
        let abort = extras.forwarders.spawn_on(forwarder, &runtime);
        extras.forwarders_unpolled = true;
        extras.forwarder_kinds.insert(abort.id(), kind);
    }

    /// Ends every timer and stream forwarder and waits until each one's
    /// future, with the stream, message or factory it owns, is dropped.
    /// A forwarder whose timer or stream was cancelled during the run
    /// (`on_stopped` included) belongs to the run, and so does a panic
    /// raised by any forwarder's poll: an aborted forwarder is never polled
    /// again, so every poll began before the end. Both are reaped as the
    /// message loop reaps them (the last forwarder error, never the actor's
    /// end), so the end reads the same whichever task runs first. From the
    /// mailbox's close on, every forwarder releases what it owns when it
    /// next runs, in its own poll, and this abort drops what one that has
    /// not run still owns. Either way a drop panic is classified by
    /// registration: part of the end for a timer or stream still registered
    /// when the end began, the run's for one cancelled during the run. The
    /// first end panic is returned; a later one is logged, naming the actor,
    /// and discarded. A panic of the run reaped here is logged by
    /// `reap_forwarder`, as one the message loop reaps is.
    pub(crate) async fn end_forwarders(&mut self) -> Option<String> {
        self.extras.as_ref()?;
        let registered: HashSet<tokio::task::Id> = self
            .ex()
            .forwarder_kinds
            .iter()
            .filter(|(_, kind)| self.is_registered(kind))
            .map(|(id, _)| *id)
            .collect();
        self.ex_mut().forwarders.abort_all();
        let owner = self.owner();
        let mut first = None;
        while let Some(joined) = self.ex_mut().forwarders.join_next_with_id().await {
            match joined {
                Err(err) if err.is_panic() && registered.contains(&err.id()) => {
                    let payload = match err.try_into_panic() {
                        Ok(payload) => payload_into_string(payload),
                        Err(_) => "panic payload unavailable".to_string(),
                    };
                    runtime::keep_first_of(&mut first, Some(payload), &owner);
                }
                // A drop the forwarder made once it had seen the mailbox
                // closed, for a timer or stream still registered: the end's.
                // Its drop made before the close stays the run's.
                Ok((
                    id,
                    ForwarderEnd::Released {
                        run,
                        end: Some(end),
                    },
                )) if registered.contains(&id) => {
                    runtime::keep_first_of(&mut first, Some(end), &owner);
                    self.reap_forwarder(Ok((id, ForwarderEnd::Released { run, end: None })));
                }
                other => {
                    self.reap_forwarder(other);
                }
            }
        }
        first
    }
}

// ---------------------------------------------------------------------------
// Internal types and helpers
// ---------------------------------------------------------------------------

pub(super) struct TimerRegistration {
    pub(super) token: CancellationToken,
    /// For a one-shot timer, set once by whichever comes first: its firing,
    /// which then puts its message in the mailbox, or its cancel. `None` for
    /// a recurring timer.
    pub(super) claimed: Option<Arc<AtomicBool>>,
}

impl TimerRegistration {
    /// Claims the timer for a cancel: always for a recurring timer, and for
    /// a one-shot timer only when its firing has not claimed it first.
    pub(super) fn claim(&self) -> bool {
        self.claimed
            .as_ref()
            .is_none_or(|claimed| !claimed.swap(true, Ordering::SeqCst))
    }
}

/// A timer or stream forwarder's future, together with the slot its
/// [`ReleaseGuard`] parks a panic in and the end signals of the actor it
/// belongs to: a sender of its status plane and its hold on its subtree.
/// Its drop drops the future first, inside a catch, so however the forwarder
/// ends (its future completes, it is aborted before its first poll or while
/// suspended) the actor's status plane and its subtree read as ended only
/// once the stream, message or factory the future owns is gone.
///
/// A panic parked in the slot is the task's panic when the future itself did
/// not panic: a poll turns it into [`ForwarderEnd::Panicked`], and a drop
/// raises it again for whoever drops the task, which tokio always does
/// inside a catch. When the future panicked too, its own panic is the
/// task's, and the parked one is logged, naming the actor.
pub(super) struct Forwarder<F> {
    /// The owner's count of forwarder ends (see `Extras::forwarders_done`).
    pub(super) done: Arc<AtomicU64>,
    /// Set once this forwarder's end is counted.
    pub(super) counted: bool,
    pub(super) future: Option<Pin<Box<F>>>,
    pub(super) slot: PanicSlot,
    pub(super) owner: runtime::Owner,
    pub(super) _status: StatusTx,
    pub(super) _subtree: Arc<SubtreeHold>,
}

impl<F> Forwarder<F> {
    /// Counts this forwarder's end once: at its ready poll, or at its drop
    /// when it ends without one (an abort, a runtime shutdown, an unwind out
    /// of its poll), which tokio runs before it completes the task.
    fn count_end(&mut self) {
        if !self.counted {
            self.counted = true;
            self.done.fetch_add(1, Ordering::Release);
        }
    }
}

impl<F> Drop for Forwarder<F> {
    fn drop(&mut self) {
        self.count_end();
        let future = self.future.take();
        let own = catch_sync(move || drop(future)).err();
        let parked = take_parked(&self.slot);
        match (own, parked) {
            (Some(own), parked) => {
                if let Some(parked) = parked {
                    runtime::log_discarded(&payload_into_string(parked), &self.owner);
                }
                std::panic::resume_unwind(own);
            }
            (None, Some(parked)) => std::panic::resume_unwind(parked),
            (None, None) => {}
        }
    }
}

/// How a forwarder task ended.
pub(crate) enum ForwarderEnd {
    /// Its future completed, releasing nothing whose drop panicked.
    Done,
    /// A poll of its future panicked, during the run: an aborted forwarder
    /// is never polled again.
    Panicked(String),
    /// Its future completed after catching panics in the drops it performed
    /// itself. `run` is a drop made before it saw the mailbox closed; `end`
    /// is a drop made once it had seen the mailbox closed, which the end
    /// classifies by registration as it classifies an aborted forwarder's
    /// drop.
    Released {
        run: Option<String>,
        end: Option<String>,
    },
}

/// The drops a forwarder performed itself and whose panics it caught.
#[derive(Default)]
pub(crate) struct Released {
    /// A drop before the forwarder saw the mailbox closed.
    pub(crate) run: Option<String>,
    /// A drop once the forwarder had seen the mailbox closed.
    pub(crate) end: Option<String>,
}

impl Released {
    /// Drops what a forwarder owns once it has seen the mailbox closed.
    pub(super) fn at_the_close<T>(held: T) -> Self {
        Released {
            run: None,
            end: release(held),
        }
    }

    /// As [`at_the_close`](Self::at_the_close) for two items, each dropped
    /// in its own catch; the first panic is kept, and a second is logged,
    /// naming the actor `owner`, and discarded.
    pub(super) fn both_at_the_close<T, U>(first: T, second: U, owner: &runtime::Owner) -> Self {
        let mut end = release(first);
        runtime::keep_first_of(&mut end, release(second), owner);
        Released { run: None, end }
    }
}

/// The build of a restart attempt: the actor its factory makes, or the
/// outcome the attempt ends with.
///
/// The factory is reached through the spec's weak reference; the upgrade runs
/// no user code. A spec already removed leaves nothing to build from: the
/// attempt is cancelled, and the cancel is recorded in the cell for the wait
/// on it. The build runs in its own catch and only borrows the factory, so no
/// user drop runs inside its unwind; a panicking factory surfaces as
/// `FactoryFailed` and charges the budget when the event re-enters strategy
/// evaluation.
///
/// The spec's box holds the factory unless the teardown or a `Kill` removed
/// the spec during the build. The attempt's reference is then the last one,
/// and the attempt starts no incarnation, whether or not the factory's drop
/// panics: the factory drops here, in its own catch, and a panic of that
/// drop is logged once and carried out on the cancelled outcome, the
/// supervisor's as if the spec's own removal had raised it. Whatever was
/// built drops in its own catch, a panic of the build or of that drop is
/// logged once, and the cancel is recorded in the cell. An attempt whose
/// build finished before the removal starts its incarnation as before: the
/// teardown stops it, or under `Kill` its link guard raises `Kill` once the
/// aborted attempt reaches the first await that returns `Pending`.
#[cfg(test)]
pub(super) fn build_for_attempt<F, A>(
    factory: &std::sync::Weak<F>,
    io: &SlotIo,
    supervisor: &ActorId,
    child_id: &ActorId,
    incarnation: u64,
) -> Result<A, Box<RestartOutcome>>
where
    F: Fn() -> A,
{
    build_for_attempt_of(factory, io, supervisor, child_id, incarnation, false)
}

/// The panic a forwarder's [`ReleaseGuard`] parks for its [`Forwarder`],
/// which reports it as the task's panic.
pub(super) type PanicSlot = Arc<std::sync::Mutex<Option<PanicPayload>>>;

/// Takes the panic parked in `slot`, if any.
pub(super) fn take_parked(slot: &PanicSlot) -> Option<PanicPayload> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

/// Holds a value a timer or stream forwarder owns for its whole life: a
/// recurring timer's factory or a stream. Every path that ends the
/// forwarder takes the value out and releases it itself; this guard drops it
/// only when the forwarder's future is dropped with it still held (an
/// abort, or a return that leaves it). Its drop runs inside a catch and never
/// unwinds: a panic there is parked in the forwarder's slot, where the
/// [`Forwarder`] reads it as the task's panic, unless a panic is parked
/// already, in which case it is logged, naming the actor, and discarded. The
/// guard is the first local of its forwarder's body, so it drops after the
/// tick, item or send in flight: a panic of theirs is the task's panic, and
/// a panic of the guard's value beside it is the one logged.
pub(super) struct ReleaseGuard<T> {
    value: Option<T>,
    owner: runtime::Owner,
    slot: PanicSlot,
}

impl<T> ReleaseGuard<T> {
    pub(super) fn new(value: T, owner: runtime::Owner, slot: PanicSlot) -> Self {
        Self {
            value: Some(value),
            owner,
            slot,
        }
    }

    pub(super) fn get_mut(&mut self) -> &mut T {
        self.value
            .as_mut()
            .expect("the value is held until a path takes it")
    }

    /// Takes the value out, for a path that releases or returns it.
    pub(super) fn take(&mut self) -> T {
        self.value
            .take()
            .expect("the value is held until a path takes it")
    }
}

/// Holds a value the caller of a public call handed in while the call checks
/// that it can run. If the check panics, the value is dropped here inside a
/// catch, so a drop that panics adds no second panic to the unwind (which
/// would end the process); that drop's panic is logged and the check's stands.
struct EntryHold<T> {
    value: Option<T>,
    owner: runtime::Owner,
}

impl<T> Drop for EntryHold<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            if let Some(panic) = runtime::end_value(value) {
                runtime::log_discarded(&panic, &self.owner);
            }
        }
    }
}

/// Panics, naming `op`, when `rt` was built without the time driver, and
/// otherwise gives `value` back. The value is held in the meantime so that
/// its drop, if the check panics, is contained.
pub(super) fn require_timers_holding<T>(
    op: &'static str,
    rt: &Handle,
    value: T,
    owner: runtime::Owner,
) -> T {
    let mut held = EntryHold {
        value: Some(value),
        owner,
    };
    require_runtime_timers(op, rt);
    held.value.take().expect("held until the check passed")
}

impl<T> Drop for ReleaseGuard<T> {
    fn drop(&mut self) {
        let Some(value) = self.value.take() else {
            return;
        };
        let Err(payload) = catch_sync(move || drop(value)) else {
            return;
        };
        let second = {
            let mut slot = self
                .slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.is_none() {
                *slot = Some(payload);
                None
            } else {
                Some(payload)
            }
        };
        if let Some(payload) = second {
            runtime::log_discarded(&payload_into_string(payload), &self.owner);
        }
    }
}

/// Holds the panic of a stream that ended during the run, from the
/// stream's drop until the forwarder reports it with its `Finished`. A
/// forwarder that ends before reporting it (a panicking `From` conversion,
/// or an abort while the `Finished` waits for room) logs it here, naming
/// the actor, so it is never lost unrecorded. Like every log line of the
/// crate it is written inside a catch, which matters here: this drop can run
/// while another panic unwinds.
pub(super) struct RunPanic {
    pub(super) payload: Option<String>,
    pub(super) owner: ActorId,
    /// The incarnation's record, for the identity fields of the log line;
    /// held only in a build that writes the line.
    #[cfg(feature = "tracing")]
    pub(super) record: crate::actor::incarnation::IncarnationRef,
}

impl RunPanic {
    /// Takes the panic out, for the forwarder's own report.
    pub(super) fn take(&mut self) -> Option<String> {
        self.payload.take()
    }
}

impl Drop for RunPanic {
    fn drop(&mut self) {
        #[cfg(feature = "tracing")]
        if let Some(payload) = self.payload.take() {
            let fields = crate::telemetry::log_fields(&self.record, None);
            caught_log!(warn, target: "tokio_actors::actor::context",
                actor_id = %self.owner,
                actor_name = fields.name(),
                incarnation = fields.incarnation(),
                system = fields.system(),
                panic = %fields.panic_text(&payload),
                "a stream that ended during the run panicked when dropped, and its forwarder ended before reporting that panic"
            );
        }
        #[cfg(not(feature = "tracing"))]
        let _ = &self.owner;
    }
}

/// Drops `held` now, catching a panic of its drop.
pub(super) fn release<T>(held: T) -> Option<String> {
    catch_sync(move || drop(held))
        .err()
        .map(payload_into_string)
}

impl<F: Future<Output = Released>> Future for Forwarder<F> {
    type Output = ForwarderEnd;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<ForwarderEnd> {
        let this = &mut *self;
        let polled = catch_sync(|| {
            this.future
                .as_mut()
                .expect("a forwarder is not polled after it ended")
                .as_mut()
                .poll(cx)
        });
        // Taken at once, before any conversion or log: a later unwind in
        // this poll finds the slot empty.
        let parked = take_parked(&this.slot);
        let out = match (polled, parked) {
            (Err(own), parked) => {
                if let Some(parked) = parked {
                    runtime::log_discarded(&payload_into_string(parked), &this.owner);
                }
                std::task::Poll::Ready(ForwarderEnd::Panicked(payload_into_string(own)))
            }
            (Ok(std::task::Poll::Ready(_)), Some(parked)) => {
                std::task::Poll::Ready(ForwarderEnd::Panicked(payload_into_string(parked)))
            }
            (Ok(std::task::Poll::Pending), Some(parked)) => {
                *this
                    .slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(parked);
                std::task::Poll::Pending
            }
            (
                Ok(std::task::Poll::Ready(Released {
                    run: None,
                    end: None,
                })),
                None,
            ) => std::task::Poll::Ready(ForwarderEnd::Done),
            (Ok(std::task::Poll::Ready(Released { run, end })), None) => {
                std::task::Poll::Ready(ForwarderEnd::Released { run, end })
            }
            (Ok(std::task::Poll::Pending), None) => std::task::Poll::Pending,
        };
        if out.is_ready() {
            this.count_end();
        }
        out
    }
}

pub(super) struct StreamRegistration {
    pub(super) token: CancellationToken,
}

/// Identifies which registration a forwarder task belongs to, so its
/// completion (however it completes) can be traced back to the right
/// `timers`/`streams` entry.
pub(super) enum ForwarderKind {
    Timer(RecurringId),
    Stream(StreamId),
    Monitor(MonitorId),
    Link(MonitorId),
    Group(MonitorId),
    Request(MonitorId),
    #[cfg_attr(not(feature = "telemetry"), allow(dead_code))]
    Subscription(SubscriptionId),
}

pub(crate) type MessageFactory<A> = dyn FnMut() -> <A as Actor>::Message + Send + 'static;

/// Maps `delivery` and puts the message in the mailbox. A map that panics
/// gets one more call, with the notice that it panicked, and ends the
/// subscription. `Err` is the end of the forwarder, with what it released.
#[cfg(feature = "telemetry")]
pub(super) async fn map_and_send<A, F>(
    map: &mut ReleaseGuard<F>,
    delivery: Delivery,
    handle: &ActorHandle<A>,
    token: &CancellationToken,
) -> Result<(), Released>
where
    A: Actor,
    F: FnMut(Delivery) -> A::Message + Send + 'static,
{
    let msg = match catch_sync(|| (map.get_mut())(delivery)) {
        Ok(msg) => msg,
        Err(payload) => {
            let panic = payload_into_string(payload);
            #[cfg(feature = "tracing")]
            caught_log!(warn, target: "tokio_actors::actor::context",
                actor_id = %handle.id(),
                panic = %handle.log_fields().panic_text(&panic),
                "the map of an event subscription panicked; the subscription ends"
            );
            #[cfg(not(feature = "tracing"))]
            let _ = panic;
            let notice = Delivery {
                event: Arc::new(TelemetryEvent::Notice(SubscriptionNotice::MapPanicked)),
                missed_before: 0,
            };
            // A second panic ends the subscription without a notice.
            if let Ok(msg) = catch_sync(|| (map.get_mut())(notice)) {
                forward_message(msg, map, handle, token).await?;
            }
            return Err(Released {
                run: release(map.take()),
                end: None,
            });
        }
    };
    forward_message(msg, map, handle, token).await
}

/// Waits for room in the mailbox and sends `msg`; a cancel that comes first,
/// or a closed mailbox, ends the forwarder after releasing `msg` and the map.
#[cfg(feature = "telemetry")]
async fn forward_message<A, F>(
    msg: A::Message,
    map: &mut ReleaseGuard<F>,
    handle: &ActorHandle<A>,
    token: &CancellationToken,
) -> Result<(), Released>
where
    A: Actor,
{
    // The message waits for a slot in the mailbox and is never moved into the
    // wait: a cancel that comes first ends the subscription, and the message
    // and the map drop each in their own catch, in this poll, as the run's.
    let permit = tokio::select! {
        biased;
        _ = token.cancelled() => {
            let mut run = release(msg);
            runtime::keep_first_of(&mut run, release(map.take()), &handle.owner());
            return Err(Released { run, end: None });
        }
        permit = handle.reserve_forward() => permit,
    };
    let Some(permit) = permit else {
        return Err(Released::both_at_the_close(
            msg,
            map.take(),
            &handle.owner(),
        ));
    };
    permit.send(msg);
    Ok(())
}

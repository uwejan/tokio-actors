//! Timers: `schedule`, `every` and their builders, the miss policy and cancellation.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{
    release, require_timers_holding, ActorContext, ForwarderKind, MessageFactory, OneShotSchedule,
    PanicSlot, RecurringScheduleBuilder, ReleaseGuard, Released, ScheduleBuilder,
    TimerRegistration,
};
use crate::actor::handle::ActorHandle;
use crate::actor::{runtime, Actor};
use crate::error::TimerError;
use crate::telemetry::{EventKind, TelemetryEvent};
use crate::types::{MissPolicy, RecurringId};

impl<A: Actor> ActorContext<A> {
    // - Timers -------------------------------------------------------------

    /// Creates a [`ScheduleBuilder`] for scheduling a message.
    ///
    /// Kind: a registration of a source of the actor's own; accepted while the actor is stopping, and it then produces nothing. See [`signals`](crate::signals).
    ///
    /// Chain `.at(instant)` or `.after(delay)` for one-shot, or `.every(interval)`
    /// for recurring timers, then `.await` to register. Registration happens
    /// entirely inside that final `.await`: a builder that is constructed and
    /// dropped without being awaited registers nothing and never fires. Every
    /// builder returned from this chain is `#[must_use]`, so that mistake is a
    /// compiler warning rather than a silent no-op.
    ///
    /// Use [`every_with`](Self::every_with) instead of `.every()` when the
    /// message type does not implement `Clone`.
    ///
    /// A registered schedule holds an internal self-reference (see
    /// [`self_handle`](Self::self_handle)) that never extends the actor's
    /// lifetime: the timer is torn down when the actor stops, not the other
    /// way around.
    ///
    /// # Panics
    /// Panics, before it registers anything, when this actor's runtime was
    /// built without the Tokio time driver.
    ///
    /// # Examples
    /// ```rust,no_run
    /// use tokio::time::Duration;
    /// use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult, MissPolicy};
    ///
    /// #[derive(Default)]
    /// struct MyActor;
    ///
    /// #[derive(Clone)]
    /// enum Msg { Ping, Tick }
    ///
    /// impl Actor for MyActor {
    ///     type Message = Msg;
    ///     type Response = ();
    ///     async fn handle(&mut self, _: Msg, _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         // One-shot after 5 seconds
    ///         ctx.schedule(Msg::Ping).after(Duration::from_secs(5)).await?;
    ///
    ///         // Recurring every 100ms (default MissPolicy::Skip)
    ///         ctx.schedule(Msg::Tick).every(Duration::from_millis(100)).await?;
    ///
    ///         // Recurring with explicit MissPolicy
    ///         ctx.schedule(Msg::Tick).every(Duration::from_millis(100))
    ///             .on_miss(MissPolicy::CatchUp).await?;
    ///         Ok(())
    ///     }
    /// }
    /// ```
    ///
    /// The time left on a timer is the actor's to keep: it records the
    /// deadline it scheduled and subtracts the current instant.
    ///
    /// ```rust
    /// use std::time::Duration;
    /// use tokio::time::Instant;
    /// use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult};
    ///
    /// #[derive(Clone)]
    /// enum Msg { Fire, Remaining }
    ///
    /// struct Alarm { deadline: Option<Instant> }
    ///
    /// impl Actor for Alarm {
    ///     type Message = Msg;
    ///     type Response = Duration;
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         let after = Duration::from_secs(5);
    ///         self.deadline = Some(Instant::now() + after);
    ///         ctx.schedule(Msg::Fire).after(after).await?;
    ///         Ok(())
    ///     }
    ///
    ///     async fn handle(&mut self, msg: Msg, _: &mut ActorContext<Self>) -> ActorResult<Duration> {
    ///         Ok(match (msg, self.deadline) {
    ///             (Msg::Remaining, Some(deadline)) => {
    ///                 deadline.saturating_duration_since(Instant::now())
    ///             }
    ///             _ => Duration::ZERO,
    ///         })
    ///     }
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread", start_paused = true)]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let alarm = Alarm { deadline: None }.spawn().await?;
    /// assert_eq!(alarm.send(Msg::Remaining).await?, Duration::from_secs(5));
    /// tokio::time::advance(Duration::from_secs(2)).await;
    /// assert_eq!(alarm.send(Msg::Remaining).await?, Duration::from_secs(3));
    /// # Ok(())
    /// # }
    /// ```
    pub fn schedule(&mut self, message: A::Message) -> ScheduleBuilder<'_, A> {
        let message = require_timers_holding(
            "ActorContext::schedule",
            &self.runtime,
            message,
            self.owner(),
        );
        ScheduleBuilder { ctx: self, message }
    }

    /// Creates a [`RecurringScheduleBuilder`] whose message is produced by
    /// `factory` on every tick, instead of being cloned from a single
    /// template. Use this when `Message` does not implement `Clone`; when it
    /// does, `.schedule(msg).every(interval)` is the shorter spelling.
    ///
    /// Kind: a registration of a source of the actor's own; accepted while the actor is stopping, and it then produces nothing. See [`signals`](crate::signals).
    ///
    /// Chain `.on_miss(policy)` before `.await` to override the default
    /// [`MissPolicy::Skip`]. As with every schedule builder, nothing is
    /// registered until the builder is awaited. A zero `interval` is
    /// rejected with [`TimerError::ZeroInterval`] at that point, before
    /// anything is spawned. An `interval` beyond 30 years is treated as 30
    /// years, which in practice never fires. The first tick is due one
    /// interval after the call that registers the timer (the final `.await`);
    /// the miss policy governs the ticks after it.
    ///
    /// # Panics
    /// Panics, before it registers anything, when this actor's runtime was
    /// built without the Tokio time driver.
    ///
    /// # Examples
    /// ```rust,no_run
    /// use tokio::time::Duration;
    /// use tokio_actors::{actor::{Actor, ActorExt, context::ActorContext}, ActorResult};
    ///
    /// struct Payload(Vec<u8>); // not Clone
    /// enum Msg { Tick(Payload) }
    ///
    /// #[derive(Default)]
    /// struct MyActor;
    ///
    /// impl Actor for MyActor {
    ///     type Message = Msg;
    ///     type Response = ();
    ///     async fn handle(&mut self, _: Msg, _: &mut ActorContext<Self>) -> ActorResult<()> { Ok(()) }
    ///
    ///     async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         ctx.every_with(Duration::from_millis(100), || Msg::Tick(Payload(Vec::new())))
    ///             .await?;
    ///         Ok(())
    ///     }
    /// }
    /// ```
    pub fn every_with<F>(
        &mut self,
        interval: Duration,
        factory: F,
    ) -> RecurringScheduleBuilder<'_, A>
    where
        F: FnMut() -> A::Message + Send + 'static,
    {
        let factory = require_timers_holding(
            "ActorContext::every_with",
            &self.runtime,
            factory,
            self.owner(),
        );
        RecurringScheduleBuilder {
            ctx: self,
            factory: Box::new(factory),
            interval,
            miss_policy: MissPolicy::Skip,
        }
    }

    /// Internal: register a one-shot timer.
    fn register_oneshot(
        &mut self,
        message: A::Message,
        when: Instant,
    ) -> Result<RecurringId, TimerError> {
        let id = self.ex_mut().id_gen.next();
        let token = CancellationToken::new();
        let cancel_clone = token.clone();
        let handle = self.self_handle.clone();
        let claimed = Arc::new(AtomicBool::new(false));
        let fut = oneshot_forward(handle, message, when, cancel_clone, claimed.clone());
        self.spawn_forwarder(ForwarderKind::Timer(id), fut, PanicSlot::default());
        self.ex_mut().timers.insert(
            id,
            TimerRegistration {
                token,
                claimed: Some(claimed),
            },
        );
        Ok(id)
    }

    /// Cancels a specific timer by its ID. Kind: a revoke, see
    /// [`signals`](crate::signals).
    ///
    /// For a one-shot timer, `true` means its message was not put in the
    /// mailbox and never will be. `false` means there was no such timer
    /// (already cancelled or never made) or the timer fired, so its message
    /// is in the mailbox or is being put there at that instant and still
    /// arrives (a message queued right after the answer may arrive first, and
    /// an actor that stops first drops it with its other queued messages).
    /// OTP's `cancel_timer` likewise returns `false` for an expired timer
    /// without saying whether its message has arrived.
    ///
    /// For a recurring timer, `true` means the timer was still held and no
    /// later tick is sent; a tick already queued, or being put there at that
    /// instant, arrives. `false` means there was no such timer.
    pub fn cancel_timer(&mut self, id: RecurringId) -> bool {
        match self.ex_mut().timers.remove(&id) {
            Some(entry) if entry.claim() => {
                entry.token.cancel();
                true
            }
            _ => false,
        }
    }

    /// Cancels all active timers: no later message is sent by any of them.
    /// A one-shot timer that fired keeps its message in the mailbox, or puts
    /// it there at that instant, as [`cancel_timer`](Self::cancel_timer)
    /// reads; a recurring tick being sent at that instant still arrives.
    pub fn cancel_all_timers(&mut self) {
        if let Some(extras) = self.extras.as_deref_mut() {
            for entry in extras.timers.values() {
                if entry.claim() {
                    entry.token.cancel();
                }
            }
            extras.timers.clear();
        }
    }

    /// Returns the number of active timers.
    pub fn active_timer_count(&self) -> usize {
        self.ex().timers.len()
    }
}

impl<'ctx, A: Actor> std::fmt::Debug for ScheduleBuilder<'ctx, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduleBuilder").finish_non_exhaustive()
    }
}

impl<'ctx, A: Actor> ScheduleBuilder<'ctx, A> {
    /// Schedule a one-shot message at a specific [`Instant`]. An instant in
    /// the past fires at once; one more than 30 years away is treated as 30
    /// years away, which in practice never fires. To read the time left of a
    /// pending timer, as Erlang/OTP's `erlang:read_timer/1` does, keep the
    /// `Instant` passed here and subtract `Instant::now()`; the result reads
    /// zero once the instant has passed and does not tell a cancelled timer
    /// from a pending one, where `erlang:read_timer/1` returns `false` for both
    /// an expired and a cancelled timer.
    pub fn at(self, when: Instant) -> OneShotSchedule<'ctx, A> {
        let now = Instant::now();
        let when = if when > now {
            runtime::saturating_deadline(now, when - now)
        } else {
            when
        };
        OneShotSchedule {
            ctx: self.ctx,
            message: self.message,
            when,
        }
    }

    /// Schedule a one-shot message after a [`Duration`]. A delay beyond 30
    /// years is treated as 30 years, which in practice never fires.
    pub fn after(self, delay: Duration) -> OneShotSchedule<'ctx, A> {
        let when = runtime::saturating_deadline(Instant::now(), delay);
        OneShotSchedule {
            ctx: self.ctx,
            message: self.message,
            when,
        }
    }

    /// Schedule a recurring timer at `interval`. Default `MissPolicy::Skip`.
    ///
    /// Kind: a registration of a source of the actor's own; accepted while the actor is stopping, and it then produces nothing. See [`signals`](crate::signals).
    ///
    /// Chain `.on_miss(policy)` before `.await` to override. Requires
    /// `Message: Clone` (the template is cloned for every tick) - use
    /// [`ActorContext::every_with`] instead for a message type that is not
    /// `Clone`. A zero `interval` is rejected with
    /// [`TimerError::ZeroInterval`] when the schedule is awaited, before
    /// anything is spawned. An `interval` beyond 30 years is treated as 30
    /// years, which in practice never fires. The first tick is due one
    /// interval after the call that registers the timer (the final
    /// `.await`); the miss policy governs the ticks after it.
    pub fn every(self, interval: Duration) -> RecurringScheduleBuilder<'ctx, A>
    where
        A::Message: Clone,
    {
        let msg = self.message;
        RecurringScheduleBuilder {
            ctx: self.ctx,
            factory: Box::new(move || msg.clone()),
            interval,
            miss_policy: MissPolicy::Skip,
        }
    }
}

impl<'ctx, A: Actor> std::fmt::Debug for OneShotSchedule<'ctx, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneShotSchedule")
            .field("when", &self.when)
            .finish_non_exhaustive()
    }
}

impl<'ctx, A: Actor> std::future::IntoFuture for OneShotSchedule<'ctx, A> {
    type Output = Result<RecurringId, TimerError>;
    type IntoFuture = std::future::Ready<Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        std::future::ready(self.ctx.register_oneshot(self.message, self.when))
    }
}

impl<'ctx, A: Actor> std::fmt::Debug for RecurringScheduleBuilder<'ctx, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecurringScheduleBuilder")
            .field("interval", &self.interval)
            .field("miss_policy", &self.miss_policy)
            .finish_non_exhaustive()
    }
}

impl<'ctx, A: Actor> RecurringScheduleBuilder<'ctx, A> {
    /// Sets the [`MissPolicy`] for this recurring timer.
    /// Default is [`MissPolicy::Skip`].
    pub fn on_miss(mut self, policy: MissPolicy) -> Self {
        self.miss_policy = policy;
        self
    }
}

impl<'ctx, A: Actor> std::future::IntoFuture for RecurringScheduleBuilder<'ctx, A> {
    type Output = Result<RecurringId, TimerError>;
    type IntoFuture = std::future::Ready<Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        // A zero interval would fire again on the very same clock reading
        // forever, with no `.await` in that path to ever yield or observe
        // cancellation, so it is rejected here, before an id is minted or
        // anything is spawned.
        if self.interval.is_zero() {
            return std::future::ready(Err(TimerError::ZeroInterval));
        }
        let id = self.ctx.ex_mut().id_gen.next();
        let token = CancellationToken::new();
        let cancel_clone = token.clone();
        let handle = self.ctx.self_handle.clone();
        let slot = PanicSlot::default();
        let first = runtime::saturating_deadline(Instant::now(), self.interval);
        let fut = recurring_loop(
            handle,
            id,
            self.factory,
            first,
            self.interval,
            self.miss_policy,
            cancel_clone,
            slot.clone(),
        );
        self.ctx
            .spawn_forwarder(ForwarderKind::Timer(id), fut, slot);
        self.ctx.ex_mut().timers.insert(
            id,
            TimerRegistration {
                token,
                claimed: None,
            },
        );
        std::future::ready(Ok(id))
    }
}

pub(super) async fn oneshot_forward<A: Actor>(
    handle: ActorHandle<A>,
    message: A::Message,
    when: Instant,
    token: CancellationToken,
    claimed: Arc<AtomicBool>,
) -> Released {
    tokio::select! {
        biased;
        _ = token.cancelled() => return Released::default(),
        // The actor stopped before the timer fired; once the mailbox is
        // closed the firing is never taken.
        () = handle.mailbox_close() => return Released::at_the_close(message),
        _ = time::sleep_until(when) => {}
    }
    // The deadline passed: the firing waits for a slot in the mailbox, and a
    // cancel that comes first ends it.
    let permit = tokio::select! {
        biased;
        _ = token.cancelled() => return Released::default(),
        permit = handle.reserve_forward() => permit,
    };
    let Some(permit) = permit else {
        return Released::at_the_close(message);
    };
    // With the slot held, nothing awaits until the message is sent or
    // dropped: the timer is claimed here, by the firing, or it was claimed
    // by a cancel first, which then read `Ok`. A message the cancel claimed
    // drops in this poll, as one cancelled before the deadline does.
    if !claimed.swap(true, Ordering::SeqCst) {
        permit.send(message);
    } else {
        drop(permit);
        drop(message);
    }
    Released::default()
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn recurring_loop<A: Actor>(
    handle: ActorHandle<A>,
    timer: RecurringId,
    factory: Box<MessageFactory<A>>,
    mut next: Instant,
    interval: Duration,
    miss_policy: MissPolicy,
    token: CancellationToken,
    slot: PanicSlot,
) -> Released {
    // The first local of the body: when the future is dropped (the end's
    // abort), the tick and the send in flight, which live in inner scopes,
    // drop before it, so the factory's drop comes last. Keep it first.
    let mut factory = ReleaseGuard::new(factory, handle.owner(), slot);
    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => return Released::default(),
            // The actor stopped; once the mailbox is closed the factory never
            // runs again.
            () = handle.mailbox_close() => return Released::at_the_close(factory.take()),
            _ = time::sleep_until(next) => {
                let tick = (factory.get_mut().as_mut())();
                // The tick waits for a slot in the mailbox and is never moved
                // into the wait: a cancel that comes first ends the timer, and
                // the tick and the factory drop each in its own catch, in this
                // poll, as the run's.
                let permit = tokio::select! {
                    biased;
                    _ = token.cancelled() => {
                        let mut run = release(tick);
                        runtime::keep_first_of(&mut run, release(factory.take()), &handle.owner());
                        return Released { run, end: None };
                    }
                    permit = handle.reserve_forward() => permit,
                };
                let Some(permit) = permit else {
                    return Released::both_at_the_close(tick, factory.take(), &handle.owner());
                };
                permit.send(tick);
                let ticks = TickEvents { timer };
                match adjust_next(&mut next, interval, miss_policy, &token, &handle, factory.get_mut(), ticks) {
                    Adjusted::Next => {}
                    Adjusted::Refused(tick) => {
                        return Released::both_at_the_close(tick, factory.take(), &handle.owner());
                    }
                    Adjusted::Panicked(payload) => {
                        let mut run = Some(payload);
                        runtime::keep_first_of(&mut run, release(factory.take()), &handle.owner());
                        return Released { run, end: None };
                    }
                }
            }
        }
    }
}

/// What [`adjust_next`] ended with.
pub(crate) enum Adjusted<M> {
    /// The timer goes on to its next tick.
    Next,
    /// The mailbox is closed: the tick that could not be sent is handed back.
    Refused(M),
    /// A `CatchUp` tick the full mailbox had no room for panicked when
    /// dropped, in its own catch: the timer ends with that panic.
    Panicked(String),
}

/// What a timer needs to report its ticks: its id.
pub(crate) struct TickEvents {
    pub(crate) timer: RecurringId,
}

/// Moves `next` past the tick just delivered and reports the tick to the
/// `TimerTick` sinks through the pump. Under `CatchUp` the missed ticks are
/// sent at once, each reported with its own lateness; a tick the full mailbox
/// has no room for is dropped, in its own catch, and the burst ends with one
/// report that says it paused.
pub(crate) fn adjust_next<A: Actor>(
    next: &mut Instant,
    interval: Duration,
    miss_policy: MissPolicy,
    token: &CancellationToken,
    handle: &ActorHandle<A>,
    factory: &mut Box<MessageFactory<A>>,
    ticks: TickEvents,
) -> Adjusted<A::Message> {
    let now = Instant::now();
    let TickEvents { timer } = ticks;
    // Whether a sink takes the kind, read once for the call: with none, the
    // off path of every report below is one branch on this value. A sink
    // attached during a catch-up burst reads from the next call.
    let taken = handle
        .stop_lane_ref()
        .abort_record()
        .mode()
        .is_some_and(|mode| mode.armed(EventKind::TimerTick));
    // Reports one tick, due at `due`, through the pump: this task is a timer
    // forwarder, which the actor's end joins, so it runs no handler. Nothing
    // is computed or built unless a sink takes the kind.
    let report = |due: Instant, missed: u128, paused: bool| {
        if !taken {
            return;
        }
        crate::telemetry::push_about(
            handle.stop_lane_ref().abort_record(),
            None,
            EventKind::TimerTick,
            |actor| TelemetryEvent::TimerTick {
                actor: Cow::Owned(actor.clone()),
                timer,
                policy: miss_policy,
                lateness: now.saturating_duration_since(due),
                missed: u64::try_from(missed).unwrap_or(u64::MAX),
                paused,
            },
        );
    };
    match miss_policy {
        MissPolicy::Skip => {
            // The first grid point after `now`, computed in constant time
            // instead of a loop that visits every missed tick: `periods` is
            // the smallest `k` with `*next + k * interval > now`, derived
            // directly from how far behind `next` has fallen. A loop here
            // alone could never terminate for a zero interval, which is why
            // a zero interval is rejected before a timer is ever registered
            // (see `RecurringScheduleBuilder::into_future`), making
            // `interval` non-zero on every call.
            let behind = now.saturating_duration_since(*next);
            let missed = behind.as_nanos() / interval.as_nanos();
            let periods = missed + 1;
            report(*next, missed, false);
            *next = match u32::try_from(periods)
                .ok()
                .and_then(|periods| interval.checked_mul(periods))
            {
                Some(advance) => runtime::saturating_deadline(*next, advance),
                None => runtime::saturating_deadline(now, interval),
            };
        }
        MissPolicy::Delay => {
            // The schedule drifts by the lateness: the next deadline is one
            // interval from now, and no tick is counted as missed.
            report(*next, 0, false);
            *next = runtime::saturating_deadline(now, interval);
        }
        MissPolicy::CatchUp => {
            // The tick just delivered was due at `*next`.
            report(*next, 0, false);
            *next = runtime::saturating_deadline(*next, interval);
            while *next <= now {
                if token.is_cancelled() {
                    return Adjusted::Next;
                }
                let msg = (factory.as_mut())();
                match handle.try_forward(msg) {
                    Ok(()) => report(*next, 0, false),
                    // The mailbox has no room: the message made for this tick is
                    // dropped and the burst pauses here. The timer loop serves
                    // the same deadline again, with a fresh message, once there
                    // is room.
                    Err(mpsc::error::TrySendError::Full(tick)) => {
                        report(*next, 0, true);
                        if let Some(payload) = release(tick) {
                            return Adjusted::Panicked(payload);
                        }
                        break;
                    }
                    Err(mpsc::error::TrySendError::Closed(msg)) => return Adjusted::Refused(msg),
                }
                *next = runtime::saturating_deadline(*next, interval);
            }
        }
    }
    Adjusted::Next
}

//! Actor execution context providing timers and runtime hooks.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;

use crate::actor::{handle::ActorHandle, Actor};
use crate::error::{ActorError, TimerError};
use crate::types::{ActorId, ActorStatus, MissPolicy, RecurringId, RecurringIdGenerator};

/// Actor execution context providing timers and runtime hooks.
///
/// The context is passed to every actor handler and provides access to:
/// - The actor's unique ID
/// - A handle to the actor itself (for sending messages to self)
/// - Timer scheduling (one-shot and recurring)
/// - Lifecycle status
pub struct ActorContext<A: Actor> {
    actor_id: ActorId,
    self_handle: ActorHandle<A>,
    runtime: Handle,
    timers: HashMap<RecurringId, TimerRegistration>,
    timer_ids: RecurringIdGenerator,
    last_error: Option<ActorError>,
    status: ActorStatus,
}

impl<A: Actor> ActorContext<A> {
    pub(crate) fn new(actor_id: ActorId, handle: ActorHandle<A>, runtime: Handle) -> Self {
        Self {
            actor_id,
            self_handle: handle,
            runtime,
            timers: HashMap::new(),
            timer_ids: RecurringIdGenerator::default(),
            last_error: None,
            status: ActorStatus::Initializing,
        }
    }

    /// Returns the unique identifier of this actor.
    pub fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    /// Returns a handle to this actor.
    ///
    /// This is useful for passing the actor's handle to other actors or for
    /// sending messages to self (e.g., for delayed processing).
    pub fn self_handle(&self) -> ActorHandle<A> {
        self.self_handle.clone()
    }

    /// Records a failure that occurred during message processing.
    ///
    /// This is typically called automatically by the runtime when a handler returns an error,
    /// but can be called manually if needed.
    pub fn record_failure(&mut self, error: ActorError) {
        self.last_error = Some(error);
    }

    /// Returns the last error recorded by this actor, if any.
    pub fn last_error(&self) -> Option<&ActorError> {
        self.last_error.as_ref()
    }

    /// Returns the current lifecycle status of the actor.
    pub fn status(&self) -> ActorStatus {
        self.status
    }

    /// Schedules a message to be sent to this actor at a specific instant.
    ///
    /// If `when` is in the past or equal to the current time, the message is sent immediately.
    /// Returns a timer ID that can be used to cancel the timer via [`cancel_timer`](Self::cancel_timer).
    ///
    /// # Example
    /// ```no_run
    /// use tokio::time::{Instant, Duration};
    /// # use tokio_actors::*;
    /// # use tokio_actors::actor::*;
    /// # async fn example(ctx: &mut context::ActorContext<impl Actor<Message=(), Response=()>>) {
    /// let when = Instant::now() + Duration::from_secs(5);
    /// let timer_id = ctx.schedule_once((), when).unwrap();
    /// # }
    /// ```
    pub fn schedule_once(
        &mut self,
        message: A::Message,
        when: Instant,
    ) -> Result<RecurringId, TimerError>
    where
        A::Message: Send + 'static,
    {
        let id = self.timer_ids.next();
        let token = CancellationToken::new();
        let cancel_clone = token.clone();
        let handle = self.self_handle.clone();
        let fut = async move {
            tokio::select! {
                _ = cancel_clone.cancelled() => {}
                _ = time::sleep_until(when) => {
                    let _ = handle.notify(message).await;
                }
            }
        };
        self.runtime.spawn(fut);
        self.timers.insert(id, TimerRegistration { token });
        Ok(id)
    }

    /// Schedules a message to be sent to this actor after a duration.
    ///
    /// This is a convenience method that calls [`schedule_once`](Self::schedule_once)
    /// with `Instant::now() + delay`.
    ///
    /// # Example
    /// ```no_run
    /// use tokio::time::Duration;
    /// # use tokio_actors::*;
    /// # use tokio_actors::actor::*;
    /// # async fn example(ctx: &mut context::ActorContext<impl Actor<Message=(), Response=()>>) {
    /// let timer_id = ctx.schedule_after((), Duration::from_secs(5)).unwrap();
    /// # }
    /// ```
    pub fn schedule_after(
        &mut self,
        message: A::Message,
        delay: Duration,
    ) -> Result<RecurringId, TimerError>
    where
        A::Message: Send + 'static,
    {
        self.schedule_once(message, Instant::now() + delay)
    }

    /// Schedules a message to be sent repeatedly using a factory closure.
    ///
    /// This is useful when the message type is not `Clone` or when the message content
    /// needs to be dynamic (e.g., containing a timestamp).
    ///
    /// The factory closure must be `Send + Sync` because it is shared across the timer task.
    pub fn schedule_recurring_with<F>(
        &mut self,
        factory: F,
        interval: Duration,
        miss_policy: MissPolicy,
    ) -> Result<RecurringId, TimerError>
    where
        F: Fn() -> A::Message + Send + Sync + 'static,
    {
        let id = self.timer_ids.next();
        let token = CancellationToken::new();
        let cancel_clone = token.clone();
        let handle = self.self_handle.clone();
        let factory: Arc<MessageFactory<A>> = Arc::new(factory);
        let fut = recurring_loop(handle, factory, interval, miss_policy, cancel_clone);
        self.runtime.spawn(fut);
        self.timers.insert(id, TimerRegistration { token });
        Ok(id)
    }

    /// Schedules a message to be sent repeatedly at a fixed interval.
    ///
    /// Returns a timer ID that can be used to cancel the timer.
    ///
    /// # Example
    /// ```no_run
    /// use tokio::time::Duration;
    /// # use tokio_actors::*;
    /// # use tokio_actors::actor::*;
    /// # async fn example(ctx: &mut context::ActorContext<impl Actor<Message=(), Response=()>>) {
    /// ctx.schedule_recurring(
    ///     (),
    ///     Duration::from_secs(1),
    ///     MissPolicy::Skip
    /// ).unwrap();
    /// # }
    /// ```
    pub fn schedule_recurring(
        &mut self,
        message: A::Message,
        interval: Duration,
        miss_policy: MissPolicy,
    ) -> Result<RecurringId, TimerError>
    where
        A::Message: Clone + Sync + Send + 'static,
    {
        self.schedule_recurring_with(move || message.clone(), interval, miss_policy)
    }

    /// Cancels a specific timer by its ID.
    ///
    /// Returns an error if the timer ID is not found (it may have already fired or been cancelled).
    pub fn cancel_timer(&mut self, id: RecurringId) -> Result<(), TimerError> {
        match self.timers.remove(&id) {
            Some(entry) => {
                entry.token.cancel();
                Ok(())
            }
            None => Err(TimerError::NotFound),
        }
    }

    /// Cancels all active timers.
    pub fn cancel_all_timers(&mut self) {
        for entry in self.timers.values() {
            entry.token.cancel();
        }
        self.timers.clear();
    }

    /// Returns the number of active timers.
    pub fn active_timer_count(&self) -> usize {
        self.timers.len()
    }

    pub(crate) fn set_status(&mut self, status: ActorStatus) {
        self.status = status;
    }
}

impl<A: Actor> Drop for ActorContext<A> {
    fn drop(&mut self) {
        // Cancel all active timers to prevent them from firing after the actor stops.
        // This cleanup ensures timer tasks don't attempt to send messages to a dead actor.
        for entry in self.timers.values() {
            entry.token.cancel();
        }
    }
}

struct TimerRegistration {
    token: CancellationToken,
}

type MessageFactory<A> = dyn Fn() -> <A as Actor>::Message + Send + Sync + 'static;

async fn recurring_loop<A: Actor>(
    handle: ActorHandle<A>,
    factory: Arc<MessageFactory<A>>,
    interval: Duration,
    miss_policy: MissPolicy,
    token: CancellationToken,
) {
    let mut next = Instant::now() + interval;
    loop {
        tokio::select! {
            _ = token.cancelled() => break,
            _ = time::sleep_until(next) => {
                let msg = (factory.as_ref())();
                let _ = handle.notify(msg).await;
                adjust_next(&mut next, interval, miss_policy, &token, &handle, &factory).await;
            }
        }
    }
}

async fn adjust_next<A: Actor>(
    next: &mut Instant,
    interval: Duration,
    miss_policy: MissPolicy,
    token: &CancellationToken,
    handle: &ActorHandle<A>,
    factory: &Arc<MessageFactory<A>>,
) {
    let now = Instant::now();
    match miss_policy {
        MissPolicy::Skip => {
            *next += interval;
            while *next <= now {
                *next += interval;
            }
        }
        MissPolicy::Delay => {
            *next = now + interval;
        }
        MissPolicy::CatchUp => {
            *next += interval;
            while *next <= now {
                if token.is_cancelled() {
                    return;
                }
                let msg = (factory.as_ref())();
                // Use try_notify to avoid blocking on full mailboxes during catch-up
                if handle.try_notify(msg).is_err() {
                    break; // Stop catch-up if mailbox is full or closed
                }
                *next += interval;
            }
        }
    }
}

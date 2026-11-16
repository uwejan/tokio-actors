//! Event managers: the `gen_event` behaviour.
//!
//! An [`EventManager`] is an actor that owns a list of handlers. An event goes to
//! every handler, newest first. A handler that fails or asks to leave is removed
//! alone; the manager and the other handlers go on.

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::time::{timeout_at, Instant};

use crate::actor::context::ActorContext;
use crate::actor::handle::ActorHandle;
use crate::actor::incarnation::status_cell::StatusTx;
use crate::actor::incarnation::StatusRx;
use crate::actor::monitor::{peek_final, signal_plane, PlaneTarget};
use crate::actor::panic::{catch_sync, payload_into_string, send_contained};
use crate::actor::runtime::{end_value, saturating_deadline};
use crate::actor::Actor;
use crate::error::{ActorError, ActorResult, SendError, TrySendError};
use crate::time_probe::runtime_has_timers;
use crate::types::{ActorId, DownReason, Incarnation, MonitorId, StopReason};

/// One handler of an [`EventManager`]. Its functions cannot await.
pub trait EventHandler: Send + 'static {
    /// The event type of the manager.
    type Event: Send + 'static;
    /// The request of a call to one handler.
    type Call: Send + 'static;
    /// The reply to such a call.
    type Reply: Send + 'static;

    /// Handles one event.
    fn handle_event(&mut self, event: &Self::Event) -> Handled;

    /// Answers a call addressed to this handler. The default gives no reply,
    /// and the caller reads [`EventError::NoReply`].
    fn handle_call(&mut self, _call: Self::Call) -> (Handled, Option<Self::Reply>) {
        (Handled::Keep, None)
    }

    /// Runs once when the handler leaves the manager, except when the manager
    /// is killed: a kill drops its handlers without running it. What it returns
    /// goes to the client that removed it: `delete_handler` returns it and
    /// `swap_handler` hands it to the swap function, so a handler can pass its
    /// state on. Any other removal drops it.
    fn terminate(&mut self, _why: &Removed) -> Option<Self::Reply> {
        None
    }
}

/// A handler as the manager stores it.
pub type BoxedHandler<E, C, R> = Box<dyn EventHandler<Event = E, Call = C, Reply = R>>;

/// Whether a handler stays after an event or a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// The handler stays.
    Keep,
    /// The handler is removed after [`EventHandler::terminate`].
    Remove,
}

/// Names one handler of one manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HandlerId(u64);

/// Why a handler left its manager.
#[derive(Debug, Clone, PartialEq)]
pub enum Removed {
    /// A client deleted it.
    Deleted,
    /// It returned [`Handled::Remove`].
    Returned,
    /// It panicked, or the swap function that was to replace it did; the text
    /// is the panic's message.
    Panicked(String),
    /// Passed to `terminate` when a client replaced the handler with
    /// `swap_handler`; the slot stays, and no [`HandlerExit`] carries it.
    Swapped,
    /// The actor that added it with `add_sup_handler` ended with this reason.
    OwnerEnded(StopReason),
    /// The manager stopped.
    Stop(StopReason),
}

/// Says that a handler left its manager: sent to the owner of a supervised
/// handler, and returned by `sync_notify` for each handler that left while
/// handling the event.
#[derive(Debug, Clone, PartialEq)]
pub struct HandlerExit {
    /// The handler.
    pub id: HandlerId,
    /// Why it left.
    pub reason: Removed,
}

/// Why a call to an event manager did not succeed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EventError {
    /// The manager has no handler with that id.
    #[error("no such handler")]
    NoHandler,
    /// The caller's bound passed. A request queued before it is still served,
    /// so an add that timed out may still have added its handler.
    #[error("the call timed out")]
    Timeout,
    /// The call is bounded, by [`EventManagerRef::timeout`] or by the default of
    /// [`EventManagerRef::call`], on a runtime without a time driver;
    /// [`EventManagerRef::unbounded`] lifts the bound.
    #[error("no time driver")]
    NoTimeDriver,
    /// The manager's mailbox is closed and its final status is not yet
    /// published, so the request was not queued; a manager still stopping, with
    /// its mailbox open, queues requests, and a request still queued when the
    /// mailbox closes is dropped. An add of a supervised handler also answers it
    /// when the manager cannot watch the owner because the manager is stopping.
    #[error("the event manager's mailbox is closed")]
    Closed,
    /// The handler took the call and gave no reply.
    #[error("the handler gave no reply")]
    NoReply,
    /// The handler panicked in the call, or the swap function panicked; the
    /// handler is removed. The text is the panic's message.
    #[error("the handler panicked: {0}")]
    HandlerPanicked(String),
    /// The manager has ended with this reason.
    #[error("the event manager has ended: {0:?}")]
    Ended(StopReason),
    /// The call was made from the manager's own task.
    #[error("called from the manager's own task")]
    CallingSelf,
    /// The manager's mailbox is full; only [`EventManagerRef::try_notify`] answers this.
    #[error("the event manager's mailbox is full")]
    Full,
    /// The calling actor is stopping, so it adds no supervised handler.
    #[error("the actor is stopping, so it adds no handler")]
    Stopping,
}

/// The message type of an [`EventManager`]. Clients use [`EventManagerRef`].
pub struct EventMessage<E, C, R> {
    kind: Kind<E, C, R>,
}

impl<E, C, R> fmt::Debug for EventMessage<E, C, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventMessage").finish_non_exhaustive()
    }
}

/// An actor that owns a list of [`EventHandler`]s for events of type `E`.
pub struct EventManager<E, C = (), R = ()> {
    core: Core<E, C, R>,
}

impl<E, C, R> fmt::Debug for EventManager<E, C, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventManager").finish_non_exhaustive()
    }
}

impl<E, C, R> Default for EventManager<E, C, R> {
    fn default() -> Self {
        Self {
            core: Core::default(),
        }
    }
}

impl<E, C, R> EventManager<E, C, R> {
    /// A manager with no handler. Starting it runs no user code.
    pub fn new() -> Self {
        Self::default()
    }
}

impl<E, C, R> Actor for EventManager<E, C, R>
where
    E: Send + 'static,
    C: Send + 'static,
    R: Send + 'static,
{
    type Message = EventMessage<E, C, R>;
    type Response = ();

    async fn handle(
        &mut self,
        msg: EventMessage<E, C, R>,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let (cmd, reply) = match msg.kind {
            Kind::Notify(event) => (Cmd::Event(event), None),
            Kind::Sync(event, reply) => (Cmd::Event(event), Some(reply)),
            Kind::Call(id, call, reply) => (Cmd::Call(id, call), Some(reply)),
            Kind::Delete(id, reply) => (Cmd::Delete(id), Some(reply)),
            Kind::Swap(id, swap, reply) => (Cmd::Swap(id, swap), Some(reply)),
            Kind::Which(reply) => (Cmd::Which, Some(reply)),
            Kind::OwnerEnded(id, reason) => (Cmd::OwnerEnded(id, reason), None),
            Kind::Add {
                id,
                handler,
                owner,
                reply,
            } => {
                let sup = match owner {
                    None => None,
                    Some(owner) => match owner.watch(ctx, id) {
                        Ok(sup) => Some(sup),
                        Err(notice) => {
                            // The manager is stopping: the notice drops with
                            // the handler list, once the reason is final.
                            self.core.unwatched.push(notice);
                            let _ = end_value(handler);
                            answer(reply, Err(EventError::Closed));
                            return Ok(());
                        }
                    },
                };
                (Cmd::Add(Slot { id, handler, sup }), Some(reply))
            }
        };
        let (result, left) = self.core.step(cmd);
        for left in left {
            left.perform(Some(ctx));
        }
        if let Some(reply) = reply {
            answer(reply, result);
        }
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let (_, left) = self.core.step(Cmd::Stop(reason.clone()));
        for left in left {
            left.perform::<E, C, R>(None);
        }
        Ok(())
    }
}

/// A reference to an event manager, for the calls a client makes.
///
/// Waits are unbounded by default, except [`Self::call`], which waits 5 seconds;
/// [`Self::timeout`] puts one bound on every call made through the reference
/// and [`Self::unbounded`] lifts every bound. A bound ends the caller's wait
/// only: a request already queued is still served.
pub struct EventManagerRef<E, C = (), R = ()>
where
    E: Send + 'static,
    C: Send + 'static,
    R: Send + 'static,
{
    manager: ActorHandle<EventManager<E, C, R>>,
    timeout: Option<Duration>,
    unbounded: bool,
}

impl<E, C, R> Clone for EventManagerRef<E, C, R>
where
    E: Send + 'static,
    C: Send + 'static,
    R: Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            manager: self.manager.clone(),
            timeout: self.timeout,
            unbounded: self.unbounded,
        }
    }
}

impl<E, C, R> fmt::Debug for EventManagerRef<E, C, R>
where
    E: Send + 'static,
    C: Send + 'static,
    R: Send + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventManagerRef")
            .field("timeout", &self.timeout)
            .field("unbounded", &self.unbounded)
            .finish_non_exhaustive()
    }
}

impl<E, C, R> EventManagerRef<E, C, R>
where
    E: Send + 'static,
    C: Send + 'static,
    R: Send + 'static,
{
    /// A reference to the manager behind `handle`.
    pub fn of(handle: &ActorHandle<EventManager<E, C, R>>) -> Self {
        Self {
            manager: handle.clone(),
            timeout: None,
            unbounded: false,
        }
    }

    /// Bounds every call made through this reference.
    #[must_use]
    pub fn timeout(mut self, bound: Duration) -> Self {
        self.timeout = Some(bound);
        self.unbounded = false;
        self
    }

    /// Lifts every bound of this reference, the default of [`Self::call`]
    /// included, so no call needs a time driver.
    #[must_use]
    pub fn unbounded(mut self) -> Self {
        self.timeout = None;
        self.unbounded = true;
        self
    }

    /// Queues an event for every handler without waiting: a full mailbox answers
    /// [`EventError::Full`], a manager whose mailbox is closed and whose final
    /// status is not yet published [`EventError::Closed`] and an ended manager
    /// [`EventError::Ended`] with its reason. A manager that is stopping with its
    /// mailbox open takes the event, which is dropped with the mailbox.
    pub fn try_notify(&self, event: E) -> Result<(), EventError> {
        self.manager
            .try_notify(EventMessage {
                kind: Kind::Notify(event),
            })
            .map_err(|err| match err {
                TrySendError::Full => EventError::Full,
                TrySendError::Closed => self.closed(),
            })
    }

    /// Queues an event for every handler, waiting for room in the manager's mailbox.
    pub async fn notify(&self, event: E) -> Result<(), EventError> {
        self.refuse_self_call()?;
        let deadline = self.deadline(None)?;
        self.queue(
            EventMessage {
                kind: Kind::Notify(event),
            },
            deadline,
        )
        .await
    }

    /// Sends an event and returns after every handler has handled it, with the
    /// handlers that left the manager while handling it.
    pub async fn sync_notify(&self, event: E) -> Result<Vec<HandlerExit>, EventError> {
        let answer = self
            .ask(None, |reply| Kind::Sync(event, reply))
            .await
            .map_err(|failed| failed.error)?;
        answer.pick(|answer| match answer {
            Answer::Exits(exits) => Some(exits),
            _ => None,
        })
    }

    /// Calls one handler and returns its reply.
    pub async fn call(&self, id: HandlerId, call: C) -> Result<R, EventError> {
        let answer = self
            .ask(Some(CALL_BOUND), |reply| Kind::Call(id, call, reply))
            .await
            .map_err(|failed| failed.error)?;
        answer.pick(|answer| match answer {
            Answer::Reply(reply) => Some(reply),
            _ => None,
        })
    }

    /// Adds a handler at the head of the list.
    pub async fn add_handler<H>(&self, handler: H) -> Result<HandlerId, EventError>
    where
        H: EventHandler<Event = E, Call = C, Reply = R>,
    {
        let id = HandlerId::next();
        let handler: BoxedHandler<E, C, R> = Box::new(handler);
        self.add(id, handler, None, &AtomicBool::new(false))
            .await
            .map_err(|failed| failed.error)
    }

    /// Removes a handler and returns what its `terminate` returned.
    pub async fn delete_handler(&self, id: HandlerId) -> Result<Option<R>, EventError> {
        let answer = self
            .ask(None, |reply| Kind::Delete(id, reply))
            .await
            .map_err(|failed| failed.error)?;
        answer.pick(|answer| match answer {
            Answer::Terminated(reply) => Some(reply),
            _ => None,
        })
    }

    /// Replaces a handler in its place in the list: `swap` gets what the old
    /// handler's `terminate` returned and returns the new handler. It runs
    /// inside the manager.
    ///
    /// A supervised handler stays supervised, and its owner gets no notice of
    /// the swap. When `swap` panics the handler is removed with
    /// [`Removed::Panicked`], the caller reads [`EventError::HandlerPanicked`]
    /// and a supervising owner gets its notice. A missing id answers
    /// [`EventError::NoHandler`] and adds nothing.
    pub async fn swap_handler<F>(&self, id: HandlerId, swap: F) -> Result<(), EventError>
    where
        F: FnOnce(Option<R>) -> BoxedHandler<E, C, R> + Send + 'static,
    {
        let swap: SwapFn<E, C, R> = Box::new(swap);
        let answer = self
            .ask(None, |reply| Kind::Swap(id, swap, reply))
            .await
            .map_err(|failed| failed.error)?;
        answer.pick(|answer| match answer {
            Answer::Unit => Some(()),
            _ => None,
        })
    }

    /// The ids of the handlers, newest first.
    pub async fn which_handlers(&self) -> Result<Vec<HandlerId>, EventError> {
        let answer = self
            .ask(None, Kind::Which)
            .await
            .map_err(|failed| failed.error)?;
        answer.pick(|answer| match answer {
            Answer::Ids(ids) => Some(ids),
            _ => None,
        })
    }

    /// Refuses a call made from the manager's own task: nothing could answer it.
    fn refuse_self_call(&self) -> Result<(), EventError> {
        if self.manager.stop_lane_ref().abort_record().is_self_call() {
            return Err(EventError::CallingSelf);
        }
        Ok(())
    }

    /// The bound of a call: the reference's, else `default`, and none once
    /// the reference is unbounded. A bound needs a runtime with a time driver.
    fn deadline(&self, default: Option<Duration>) -> Result<Option<Instant>, EventError> {
        let bound = if self.unbounded {
            None
        } else {
            self.timeout.or(default)
        };
        let Some(bound) = bound else {
            return Ok(None);
        };
        if !Handle::try_current().is_ok_and(|runtime| runtime_has_timers(&runtime)) {
            return Err(EventError::NoTimeDriver);
        }
        Ok(Some(saturating_deadline(Instant::now(), bound)))
    }

    /// What a manager that cannot take a message answers: its end, or that its
    /// mailbox is closed before its end is published.
    fn closed(&self) -> EventError {
        peek_final(&self.manager.status_rx()).map_or(EventError::Closed, EventError::Ended)
    }

    /// Queues `msg`, waiting for room until `deadline`. `Ok` means it is in the
    /// mailbox.
    async fn queue(
        &self,
        msg: EventMessage<E, C, R>,
        deadline: Option<Instant>,
    ) -> Result<(), EventError> {
        match within(deadline, self.manager.notify(msg)).await {
            None => Err(EventError::Timeout),
            Some(Ok(())) => Ok(()),
            Some(Err(SendError::Full)) => Err(EventError::Full),
            Some(Err(SendError::Closed | SendError::NotFound)) => Err(self.closed()),
        }
    }

    /// Queues the request `kind` builds and waits for the manager's answer,
    /// under the reference's bound or `default`.
    async fn ask(
        &self,
        default: Option<Duration>,
        kind: impl FnOnce(Reply<R>) -> Kind<E, C, R>,
    ) -> Result<Answer<R>, Failed> {
        self.ask_marked(default, kind, &AtomicBool::new(false))
            .await
    }

    /// As [`Self::ask`]; sets `queued` once the mailbox accepted the request.
    async fn ask_marked(
        &self,
        default: Option<Duration>,
        kind: impl FnOnce(Reply<R>) -> Kind<E, C, R>,
        queued: &AtomicBool,
    ) -> Result<Answer<R>, Failed> {
        self.refuse_self_call().map_err(Failed::unqueued)?;
        let deadline = self.deadline(default).map_err(Failed::unqueued)?;
        let (reply, answer) = oneshot::channel();
        let msg = EventMessage { kind: kind(reply) };
        self.queue(msg, deadline).await.map_err(Failed::unqueued)?;
        queued.store(true, Ordering::Relaxed);
        match within(deadline, answer).await {
            None => Err(Failed::queued(EventError::Timeout)),
            Some(Ok(answer)) => answer.map_err(Failed::queued),
            Some(Err(_)) => {
                // The manager dropped the request unanswered: it ended.
                match within(deadline, self.manager.wait_stopped()).await {
                    None => Err(Failed::queued(EventError::Timeout)),
                    Some(reason) => Err(Failed::ended(EventError::Ended(reason))),
                }
            }
        }
    }

    /// Queues an add of `handler` under `id` and returns the id the manager
    /// took it under.
    async fn add(
        &self,
        id: HandlerId,
        handler: BoxedHandler<E, C, R>,
        owner: Option<Supervised>,
        queued: &AtomicBool,
    ) -> Result<HandlerId, Failed> {
        let add = |reply| Kind::Add {
            id,
            handler,
            owner,
            reply,
        };
        let answer = self.ask_marked(None, add, queued).await?;
        answer
            .pick(|answer| match answer {
                Answer::Id(id) => Some(id),
                _ => None,
            })
            .map_err(Failed::queued)
    }
}

impl<A: Actor> ActorContext<A> {
    /// Adds a handler that this actor supervises: when the handler leaves the
    /// manager, for any reason, or the manager ends, this actor receives
    /// `map(HandlerExit)` once; when this actor ends, the manager removes the
    /// handler with [`Removed::OwnerEnded`]. A swap keeps the supervision and
    /// sends no notice.
    ///
    /// Waits for the manager to take the handler, with the bound of the
    /// reference, and returns the handler's id. Refused with
    /// [`EventError::Stopping`] once this actor is stopping. A notice still owed
    /// when this actor stops is delivered once through `handle`, as a `Down` is.
    /// The handler is removed again with [`EventManagerRef::delete_handler`]. A
    /// bound that passes after the add was queued leaves the handler added and
    /// supervised under an id this call did not return; its `HandlerExit` names it.
    pub async fn add_sup_handler<E, C, R, H, F>(
        &mut self,
        manager: &EventManagerRef<E, C, R>,
        handler: H,
        map: F,
    ) -> Result<HandlerId, EventError>
    where
        E: Send + 'static,
        C: Send + 'static,
        R: Send + 'static,
        H: EventHandler<Event = E, Call = C, Reply = R>,
        F: Fn(HandlerExit) -> A::Message + Send + Sync + 'static,
    {
        if matches!(
            self.status(),
            crate::types::ActorStatus::Stopping | crate::types::ActorStatus::Stopped
        ) {
            return Err(EventError::Stopping);
        }
        let id = HandlerId::next();
        // The slot's plane closes when the manager drops the slot's sender; the
        // owner's monitor on it then makes the notice, which reads why from the
        // cell the manager wrote first.
        let (sender, plane) = signal_plane(self.resolved_system().telemetry_mode().clone());
        let cell = Arc::new(OnceLock::new());
        let told = cell.clone();
        let slot = PlaneTarget {
            actor_id: manager.manager.id().clone(),
            incarnation: Some(manager.manager.incarnation()),
            plane,
        };
        let monitor = self
            .monitor(&slot, move |_| {
                map(HandlerExit {
                    id,
                    reason: told
                        .get()
                        .cloned()
                        .unwrap_or(Removed::Stop(StopReason::Kill)),
                })
            })
            .map_err(|_| EventError::Stopping)?;
        let own = self.self_handle_ref();
        let owner = Supervised {
            actor_id: own.id().clone(),
            incarnation: own.incarnation(),
            plane: own.status_rx(),
            notice: Notice {
                cell,
                _plane: sender,
                manager: manager.manager.status_rx(),
            },
        };
        let handler: BoxedHandler<E, C, R> = Box::new(handler);
        let queued = AtomicBool::new(false);
        let unaccepted = Unaccepted {
            ctx: self,
            monitor,
            queued: &queued,
        };
        let added = manager.add(id, handler, Some(owner), &queued).await;
        queued.store(true, Ordering::Relaxed);
        match added {
            Ok(id) => Ok(id),
            Err(failed) => {
                // A bound that passed after the add was queued leaves the
                // handler added, and its notice names it. Any other failure
                // means no handler exists, so no notice is owed.
                if !failed.may_exist {
                    unaccepted.ctx.demonitor_flush(monitor);
                }
                Err(failed.error)
            }
        }
    }
}

/// The owner's monitor while a supervised add runs: an add dropped before the
/// manager's mailbox accepted it leaves no handler, so the monitor is removed
/// and flushed, before or after the notice drops; an accepted one keeps it.
/// `queued` is also set when the add returns, which hands the monitor back.
struct Unaccepted<'a, A: Actor> {
    ctx: &'a mut ActorContext<A>,
    monitor: MonitorId,
    queued: &'a AtomicBool,
}

impl<A: Actor> Drop for Unaccepted<'_, A> {
    fn drop(&mut self) {
        if !self.queued.load(Ordering::Relaxed) {
            self.ctx.demonitor_flush(self.monitor);
        }
    }
}

/// The bound of [`EventManagerRef::call`] on a reference that sets no timeout
/// and is not unbounded.
const CALL_BOUND: Duration = Duration::from_secs(5);

/// The next id, from one counter for the whole process.
static NEXT_HANDLER: AtomicU64 = AtomicU64::new(1);

impl HandlerId {
    fn next() -> Self {
        Self(NEXT_HANDLER.fetch_add(1, Ordering::Relaxed))
    }
}

/// Awaits `fut` until `deadline`; `None` when the deadline passes first.
async fn within<T>(deadline: Option<Instant>, fut: impl Future<Output = T>) -> Option<T> {
    match deadline {
        None => Some(fut.await),
        Some(deadline) => timeout_at(deadline, fut).await.ok(),
    }
}

/// What the manager answers to a request.
enum Answer<R> {
    Unit,
    Id(HandlerId),
    Exits(Vec<HandlerExit>),
    Reply(R),
    Terminated(Option<R>),
    Ids(Vec<HandlerId>),
}

impl<R> Answer<R> {
    /// The payload `pick` selects. A request is answered with the answer of its
    /// own kind, so the other arm is never taken.
    fn pick<T>(self, pick: impl FnOnce(Self) -> Option<T>) -> Result<T, EventError> {
        pick(self).ok_or(EventError::NoReply)
    }
}

type Reply<R> = oneshot::Sender<Result<Answer<R>, EventError>>;

/// Sends an answer. The receiver's waker belongs to the caller, so the send is
/// contained; an answer nobody receives is dropped inside a catch, since the
/// reply is the user's value.
fn answer<R>(reply: Reply<R>, result: Result<Answer<R>, EventError>) {
    if let Err(unclaimed) = send_contained(reply, result) {
        let _ = end_value(unclaimed);
    }
}

/// Why a request did not succeed, and whether its handler may exist anyway.
struct Failed {
    error: EventError,
    may_exist: bool,
}

impl Failed {
    /// The request was not queued.
    fn unqueued(error: EventError) -> Self {
        Self {
            error,
            may_exist: false,
        }
    }

    /// The request was queued: only a bound that passed leaves it served.
    fn queued(error: EventError) -> Self {
        Self {
            may_exist: error == EventError::Timeout,
            error,
        }
    }

    /// The manager ended with the request queued: it was never served.
    fn ended(error: EventError) -> Self {
        Self::unqueued(error)
    }
}

type SwapFn<E, C, R> = Box<dyn FnOnce(Option<R>) -> BoxedHandler<E, C, R> + Send>;

/// The requests a manager takes.
enum Kind<E, C, R> {
    Notify(E),
    Sync(E, Reply<R>),
    Call(HandlerId, C, Reply<R>),
    Add {
        id: HandlerId,
        handler: BoxedHandler<E, C, R>,
        owner: Option<Supervised>,
        reply: Reply<R>,
    },
    Delete(HandlerId, Reply<R>),
    Swap(HandlerId, SwapFn<E, C, R>, Reply<R>),
    Which(Reply<R>),
    OwnerEnded(HandlerId, StopReason),
}

/// The owner's side of a supervised slot: the cell holding why the slot left,
/// written before the plane closes, and the manager's status plane.
struct Notice {
    cell: Arc<OnceLock<Removed>>,
    _plane: StatusTx,
    manager: StatusRx,
}

impl Notice {
    fn tell(self, why: Removed) {
        let _ = self.cell.set(why);
    }
}

impl Drop for Notice {
    /// A notice dropped unwritten reads the reason the manager ended with. The
    /// manager drops one only once that reason is final (with its queued
    /// requests or its handler list, after the reason is written), so `Kill`
    /// is read only for a manager killed, aborted or ended with its runtime.
    fn drop(&mut self) {
        if self.cell.get().is_none() {
            let reason = peek_final(&self.manager).unwrap_or(StopReason::Kill);
            let _ = self.cell.set(Removed::Stop(reason));
        }
    }
}

/// What an add by an actor carries: the owner's plane, which the manager
/// watches, and the notice the manager owes it.
struct Supervised {
    actor_id: ActorId,
    incarnation: Incarnation,
    plane: StatusRx,
    notice: Notice,
}

impl Supervised {
    /// Watches the owner from the manager.
    fn watch<E, C, R>(
        self,
        ctx: &mut ActorContext<EventManager<E, C, R>>,
        id: HandlerId,
    ) -> Result<Supervision, Notice>
    where
        E: Send + 'static,
        C: Send + 'static,
        R: Send + 'static,
    {
        let Self {
            actor_id,
            incarnation,
            plane,
            notice,
        } = self;
        let target = PlaneTarget {
            actor_id,
            incarnation: Some(incarnation),
            plane,
        };
        let watched = ctx.monitor(&target, move |down| {
            let reason = match down.reason {
                DownReason::Stopped(reason) => reason,
                DownReason::NoProc | DownReason::Unknown => {
                    StopReason::Failure(ActorError::NoProc(down.actor_id))
                }
            };
            EventMessage {
                kind: Kind::OwnerEnded(id, reason),
            }
        });
        let Ok(watch) = watched else {
            return Err(notice);
        };
        Ok(Supervision {
            notice,
            watch: Some(watch),
        })
    }
}

/// The supervision of one slot, as the manager keeps it.
struct Supervision {
    notice: Notice,
    watch: Option<MonitorId>,
}

struct Slot<E, C, R> {
    id: HandlerId,
    handler: BoxedHandler<E, C, R>,
    sup: Option<Supervision>,
}

/// A slot that left: the owner is owed one notice.
struct Left {
    id: HandlerId,
    why: Removed,
    sup: Option<Supervision>,
}

impl Left {
    /// Tells the owner and stops watching it. `ctx` is `None` once the manager
    /// is past its last message.
    fn perform<E, C, R>(self, ctx: Option<&mut ActorContext<EventManager<E, C, R>>>)
    where
        E: Send + 'static,
        C: Send + 'static,
        R: Send + 'static,
    {
        let Some(sup) = self.sup else { return };
        if let (Some(ctx), Some(watch)) = (ctx, sup.watch) {
            ctx.demonitor(watch);
        }
        sup.notice.tell(self.why);
    }
}

/// One request to the handler list.
enum Cmd<E, C, R> {
    Event(E),
    Call(HandlerId, C),
    Add(Slot<E, C, R>),
    Delete(HandlerId),
    Swap(HandlerId, SwapFn<E, C, R>),
    Which,
    OwnerEnded(HandlerId, StopReason),
    Stop(StopReason),
}

/// The handler list. A step takes one request and returns the answer and the
/// slots that left; it holds no channel, task or clock.
struct Core<E, C, R> {
    /// Newest first.
    slots: Vec<Slot<E, C, R>>,
    /// The notices of adds refused because the manager is stopping: each
    /// drops with the list, after the manager's reason is final.
    unwatched: Vec<Notice>,
}

impl<E, C, R> Default for Core<E, C, R> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            unwatched: Vec::new(),
        }
    }
}

impl<E, C, R> Drop for Core<E, C, R> {
    /// A manager that ends without `on_stopped` (a kill) drops its handlers
    /// one at a time, each drop caught.
    fn drop(&mut self) {
        for slot in std::mem::take(&mut self.slots) {
            let _ = end_value(slot.handler);
        }
    }
}

type Step<R> = (Result<Answer<R>, EventError>, Vec<Left>);

impl<E, C, R> Core<E, C, R>
where
    E: Send + 'static,
    C: Send + 'static,
    R: Send + 'static,
{
    fn position(&self, id: HandlerId) -> Option<usize> {
        self.slots.iter().position(|slot| slot.id == id)
    }

    /// Runs `terminate` once, then drops the handler, both caught. Returns what
    /// `terminate` returned and the notice the owner is owed.
    fn retire(slot: Slot<E, C, R>, why: Removed) -> (Option<R>, Left) {
        let Slot {
            id,
            mut handler,
            sup,
        } = slot;
        let returned = terminate(&mut handler, &why);
        let _ = end_value(handler);
        (returned, Left { id, why, sup })
    }

    /// Retires a slot whose `terminate` value nobody receives: the value is
    /// dropped inside a catch, so a `Drop` that panics fails no step.
    fn retire_unclaimed(slot: Slot<E, C, R>, why: Removed) -> Left {
        let (returned, left) = Self::retire(slot, why);
        let _ = end_value(returned);
        left
    }

    fn step(&mut self, cmd: Cmd<E, C, R>) -> Step<R> {
        match cmd {
            Cmd::Event(event) => {
                let step = self.event(&event);
                let _ = end_value(event);
                step
            }
            Cmd::Call(id, call) => self.call(id, call),
            Cmd::Add(slot) => {
                let id = slot.id;
                self.slots.insert(0, slot);
                (Ok(Answer::Id(id)), Vec::new())
            }
            Cmd::Delete(id) => match self.position(id) {
                None => (Err(EventError::NoHandler), Vec::new()),
                Some(at) => {
                    let (returned, left) = Self::retire(self.slots.remove(at), Removed::Deleted);
                    (Ok(Answer::Terminated(returned)), vec![left])
                }
            },
            Cmd::Swap(id, make) => self.swap(id, make),
            Cmd::Which => (
                Ok(Answer::Ids(self.slots.iter().map(|slot| slot.id).collect())),
                Vec::new(),
            ),
            Cmd::OwnerEnded(id, reason) => {
                let left = self.position(id).map(|at| {
                    Self::retire_unclaimed(self.slots.remove(at), Removed::OwnerEnded(reason))
                });
                (Ok(Answer::Unit), left.into_iter().collect())
            }
            Cmd::Stop(reason) => {
                let left = std::mem::take(&mut self.slots)
                    .into_iter()
                    .map(|slot| Self::retire_unclaimed(slot, Removed::Stop(reason.clone())))
                    .collect();
                (Ok(Answer::Unit), left)
            }
        }
    }

    fn event(&mut self, event: &E) -> Step<R> {
        let mut exits = Vec::new();
        let mut left = Vec::new();
        let mut at = 0;
        while at < self.slots.len() {
            let why = match catch_sync(|| self.slots[at].handler.handle_event(event)) {
                Ok(Handled::Keep) => {
                    at += 1;
                    continue;
                }
                Ok(Handled::Remove) => Removed::Returned,
                Err(payload) => Removed::Panicked(payload_into_string(payload)),
            };
            let gone = Self::retire_unclaimed(self.slots.remove(at), why);
            exits.push(HandlerExit {
                id: gone.id,
                reason: gone.why.clone(),
            });
            left.push(gone);
        }
        (Ok(Answer::Exits(exits)), left)
    }

    fn call(&mut self, id: HandlerId, call: C) -> Step<R> {
        let Some(at) = self.position(id) else {
            let _ = end_value(call);
            return (Err(EventError::NoHandler), Vec::new());
        };
        let (result, why) = match catch_sync(|| self.slots[at].handler.handle_call(call)) {
            Ok((handled, reply)) => (
                reply.map(Answer::Reply).ok_or(EventError::NoReply),
                (handled == Handled::Remove).then_some(Removed::Returned),
            ),
            Err(payload) => {
                let text = payload_into_string(payload);
                (
                    Err(EventError::HandlerPanicked(text.clone())),
                    Some(Removed::Panicked(text)),
                )
            }
        };
        let left = why
            .map(|why| Self::retire_unclaimed(self.slots.remove(at), why))
            .into_iter()
            .collect();
        (result, left)
    }

    fn swap(&mut self, id: HandlerId, make: SwapFn<E, C, R>) -> Step<R> {
        let Some(at) = self.position(id) else {
            let _ = end_value(make);
            return (Err(EventError::NoHandler), Vec::new());
        };
        let Slot {
            id,
            mut handler,
            sup,
        } = self.slots.remove(at);
        let returned = terminate(&mut handler, &Removed::Swapped);
        let _ = end_value(handler);
        match catch_sync(move || make(returned)) {
            Ok(handler) => {
                self.slots.insert(at, Slot { id, handler, sup });
                (Ok(Answer::Unit), Vec::new())
            }
            Err(payload) => {
                let text = payload_into_string(payload);
                let why = Removed::Panicked(text.clone());
                (
                    Err(EventError::HandlerPanicked(text)),
                    vec![Left { id, why, sup }],
                )
            }
        }
    }
}

/// Runs a handler's `terminate`, caught; a panic leaves no reply.
fn terminate<E, C, R>(handler: &mut BoxedHandler<E, C, R>, why: &Removed) -> Option<R>
where
    E: Send + 'static,
    C: Send + 'static,
    R: Send + 'static,
{
    match catch_sync(|| handler.terminate(why)) {
        Ok(reply) => reply,
        Err(payload) => {
            let _ = payload_into_string(payload);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    type Log = Arc<Mutex<Vec<String>>>;

    /// A reply whose drop panics when it carries `BAD`.
    struct Rep(u32);
    const BAD: u32 = 0;

    impl fmt::Debug for Rep {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl Drop for Rep {
        fn drop(&mut self) {
            assert!(self.0 != BAD, "reply drop");
        }
    }

    struct Probe {
        tag: char,
        mode: String,
        log: Log,
    }

    impl EventHandler for Probe {
        type Event = u8;
        type Call = u8;
        type Reply = Rep;

        fn handle_event(&mut self, event: &u8) -> Handled {
            self.log
                .lock()
                .unwrap()
                .push(format!("{}{event}", self.tag));
            match self.mode.as_str() {
                "remove" | "dropr" => Handled::Remove,
                "panic" => panic!("boom"),
                _ => Handled::Keep,
            }
        }

        fn handle_call(&mut self, call: u8) -> (Handled, Option<Rep>) {
            match self.mode.as_str() {
                "panic" => panic!("callboom"),
                "mute" => (Handled::Keep, None),
                _ => (Handled::Keep, Some(Rep(u32::from(call) * 10))),
            }
        }

        fn terminate(&mut self, why: &Removed) -> Option<Rep> {
            let why = format!("{why:?}").replace('"', "");
            self.log.lock().unwrap().push(format!("{}!{why}", self.tag));
            let bad = self.mode == "dropr";
            Some(Rep(if bad { BAD } else { self.tag as u32 }))
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            assert!(self.mode != "dropp", "drop");
        }
    }

    fn probe(tag: char, mode: &str, log: &Log) -> BoxedHandler<u8, u8, Rep> {
        Box::new(Probe {
            tag,
            mode: mode.into(),
            log: log.clone(),
        })
    }

    #[test]
    fn event_manager_table() {
        // Ops: +tag:mode e -tag ctag s<old><new> ptag(swap panics) otag(owner ended) x w
        let rows = [
            (
                "newest first; Remove and a panic remove alone",
                "+a:keep +b:remove +c:panic e w",
                "c1 c!Panicked(boom) b1 b!Returned a1 \
                 exits[c:Panicked(boom) b:Returned] ids[a]",
            ),
            (
                "delete returns the reply; a missing id is a value",
                "+a:keep -a -a",
                "a!Deleted reply[97] no such handler",
            ),
            (
                "swap keeps the slot and hands on terminate's reply",
                "+a:keep +b:keep sac w -a",
                "a!Swapped gotSome(97) ok ids[ba] c!Deleted reply[99]",
            ),
            (
                "a swap function that panics removes the slot",
                "+a:keep pa w",
                "a!Swapped the handler panicked: swapboom ids[]",
            ),
            (
                "calls: a reply, no reply, a panic removes",
                "+a:keep +b:mute +c:panic ca cb cc w",
                "reply[10] the handler gave no reply c!Panicked(callboom) \
                 the handler panicked: callboom ids[ba]",
            ),
            (
                "a handler whose Drop panics fails nothing",
                "+a:keep +d:dropp -d e",
                "d!Deleted reply[100] a1",
            ),
            (
                "a terminate value whose Drop panics fails nothing",
                "+a:keep +r:dropr +b:keep e e",
                "b1 r1 r!Returned a1 exits[r:Returned] b1 a1",
            ),
            (
                "the owner ended, then the manager stopped",
                "+a:keep +b:keep +c:keep ob x w",
                "b!OwnerEnded(Graceful) ok c!Stop(Graceful) a!Stop(Graceful) ok ids[]",
            ),
        ];
        for (row, ops, expected) in rows {
            let log = Log::default();
            let mut core = Core::<u8, u8, Rep>::default();
            let mut ids: Vec<(char, HandlerId)> = Vec::new();
            let id_of = |ids: &[(char, HandlerId)], t: char| {
                ids.iter().find(|p| p.0 == t).map_or(HandlerId(0), |p| p.1)
            };
            for op in ops.split(' ') {
                let mut chars = op.chars();
                let (kind, t) = (chars.next().unwrap(), chars.next().unwrap_or(' '));
                let id = id_of(&ids, t);
                let cmd = match kind {
                    '+' => {
                        let id = HandlerId::next();
                        ids.push((t, id));
                        let handler = probe(t, &op[3..], &log);
                        Cmd::Add(Slot {
                            id,
                            handler,
                            sup: None,
                        })
                    }
                    'e' => Cmd::Event(1),
                    '-' => Cmd::Delete(id),
                    'c' => Cmd::Call(id, 1),
                    's' => {
                        let (new, l) = (chars.next().unwrap(), log.clone());
                        Cmd::Swap(
                            id,
                            Box::new(move |got| {
                                l.lock().unwrap().push(format!("got{got:?}"));
                                probe(new, "keep", &l)
                            }),
                        )
                    }
                    'p' => Cmd::Swap(id, Box::new(|_| panic!("swapboom"))),
                    'o' => Cmd::OwnerEnded(id, StopReason::Graceful),
                    'x' => Cmd::Stop(StopReason::Graceful),
                    _ => Cmd::Which,
                };
                let (result, _) = core.step(cmd);
                let tag = |id| ids.iter().find(|p| p.1 == id).map_or('?', |p| p.0);
                let said = match result {
                    Ok(Answer::Ids(v)) => {
                        format!("ids[{}]", v.into_iter().map(tag).collect::<String>())
                    }
                    Ok(Answer::Exits(v)) if !v.is_empty() => {
                        let each = v.iter().map(|x| format!("{}:{:?}", tag(x.id), x.reason));
                        format!("exits[{}]", each.collect::<Vec<_>>().join(" ")).replace('"', "")
                    }
                    Ok(Answer::Reply(r) | Answer::Terminated(Some(r))) => format!("reply[{}]", r.0),
                    Ok(Answer::Unit) => "ok".to_string(),
                    Err(err) => err.to_string(),
                    _ => continue,
                };
                log.lock().unwrap().push(said);
            }
            assert_eq!(log.lock().unwrap().join(" "), expected, "{row}");
        }
        // The default bound of `call` is the 5 seconds the docs name.
        assert_eq!(CALL_BOUND, Duration::from_secs(5));
    }
}

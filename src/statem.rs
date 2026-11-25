//! State machines: the `gen_statem` behaviour as a typed adapter over [`Actor`].
//!
//! A [`StateMachine`] is a plain type with one event function that cannot await.
//! What the function asks for (a reply, a postponed event, a time-out) is recorded
//! and performed after it returns, so the same machine runs as an actor under
//! [`StatemActor`] and, with no runtime at all, under [`Harness`] in a test.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::{timeout_at, Instant};

use crate::actor::context::ActorContext;
use crate::actor::handle::ActorHandle;
use crate::actor::incarnation::{StatusRx, StopLane};
use crate::actor::panic::{catch_sync, discard, send_contained, AnswerHold};
use crate::actor::Actor;
use crate::error::{ActorError, ActorResult, AskError, SendError, StopError, TrySendError};
use crate::system::{read_status, wait_final_reason};
use crate::types::{ActorStatus, RecurringId, StopReason, StreamEvent};

/// A state machine.
///
/// The machine's data is `self`; the state name is a value of [`Self::State`]. A
/// transition is a state change when the new state differs from the old one, so
/// data that changes without a change of state belongs in `self`.
pub trait StateMachine: Sized + Send + 'static {
    /// The state name.
    type State: Clone + PartialEq + Send + 'static;
    /// Casts, inserted events and time-out events.
    type Event: Send + 'static;
    /// The request of a call.
    type Call: Send + 'static;
    /// The reply to a call.
    type Reply: Send + 'static;
    /// The name of a generic time-out.
    type TimerName: Clone + Eq + Hash + Send + 'static;

    /// Runs once before the first event, as the actor's start gate: an `Err` means
    /// the machine never starts. The state is the one given to
    /// [`StatemActor::new`]; the first [`Self::on_enter`] runs after `init`.
    ///
    /// The registrations that await (`schedule`, `every`, `add_sup_handler`) are
    /// made here, since the event function cannot await. A [`Harness`] does not
    /// run `init`.
    fn init(
        &mut self,
        _ctx: &mut ActorContext<StatemActor<Self>>,
    ) -> impl Future<Output = ActorResult<()>> + Send {
        async { Ok(()) }
    }

    /// Handles one event in `state` and says what the machine does next.
    fn handle_event(
        &mut self,
        state: &Self::State,
        event: StatemEvent<Self>,
        cx: &mut StatemContext<'_, Self>,
    ) -> Transition<Self::State>;

    /// Runs when a state is entered: once before the first event with
    /// `old == new`, after every state change, and again after
    /// [`Transition::Repeat`].
    fn on_enter(
        &mut self,
        _old: &Self::State,
        _new: &Self::State,
        _cx: &mut EnterContext<'_, Self>,
    ) {
    }

    /// Runs once when the machine stops, unless it is killed or its start
    /// failed. It has no context and cannot reply: a call the machine still
    /// holds ends with the machine.
    fn terminate(&mut self, _reason: &StopReason, _state: &Self::State) {}
}

/// One event handed to [`StateMachine::handle_event`].
pub enum StatemEvent<M: StateMachine> {
    /// An event sent with [`ActorHandle::cast`], or a notice mapped to an event.
    Cast(M::Event),
    /// A call; answer it through its [`ReplyTo`], now or after postponing it.
    Call(M::Call, ReplyTo<M::Reply>),
    /// An event inserted with [`StatemContext::next_event`].
    Internal(M::Event),
    /// The event time-out fired: no other event was handled in time.
    EventTimeout(M::Event),
    /// The state time-out fired: the state did not change in time.
    StateTimeout(M::Event),
    /// A generic time-out fired.
    Timeout(M::TimerName, M::Event),
}

impl<M: StateMachine> fmt::Debug for StatemEvent<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cast(_) => "Cast",
            Self::Call(..) => "Call",
            Self::Internal(_) => "Internal",
            Self::EventTimeout(_) => "EventTimeout",
            Self::StateTimeout(_) => "StateTimeout",
            Self::Timeout(..) => "Timeout",
        })
    }
}

/// What a machine does after an event.
#[derive(Debug)]
pub enum Transition<S> {
    /// Go to this state; a different state is a state change.
    Next(S),
    /// Stay in the state; postponed events are not retried.
    Keep,
    /// Stay in the state and run [`StateMachine::on_enter`] again.
    Repeat,
    /// Stop the machine with this reason, as
    /// [`Next::Stop`](crate::behavior::Next::Stop) stops a behavior: no further
    /// event is handled, the replies recorded in this call are sent first, and
    /// `StopReason::Kill` is refused and fails the machine with an error.
    Stop(StopReason),
}

/// The return address of one call. It is used once, through the `reply` of a
/// callback's context; it has no method of its own, so a reply is always made
/// from a callback of the machine.
///
/// While the machine runs, a `ReplyTo` dropped without a reply fails the
/// caller's call with [`AskError::Actor`] carrying [`ActorError::NoReply`],
/// wherever it is dropped: a panic of another task that holds it included. One
/// dropped inside a callback of this machine is answered when the callback's
/// step ends, so one dropped in a panic the callback itself catches gives
/// `NoReply` then. A callback of this machine that fails gives
/// [`AskError::ResponseDropped`] with the failure for every `ReplyTo` dropped in
/// its step, the call it was handling included; so does a failure of the
/// engine's own work on the state the callback returned. Once the machine is
/// stopping, killed or ended, a dropped `ReplyTo` says nothing of its own and
/// the caller reads [`AskError::ResponseDropped`] with the machine's final
/// reason.
pub struct ReplyTo<R> {
    sink: Option<Sink<R>>,
    /// Boxes the sink for the list of a step's mark, with its reply type
    /// erased; set where the reply type is known to be `'static`.
    erase: fn(Sink<R>) -> Box<dyn Held>,
}

/// The answer a call's caller reads.
type Answer<R> = Result<R, ActorError>;

/// Where a [`ReplyTo`] delivers: a caller's one-shot under an actor, or the log
/// of a [`Harness`].
enum Sink<R> {
    Call {
        hold: AnswerHold<Answer<R>>,
        plane: StatusRx,
        lane: StopLane,
    },
    Log {
        log: mpsc::Sender<(usize, Option<R>)>,
        number: usize,
        alive: Arc<AtomicBool>,
    },
}

impl<R> Sink<R> {
    /// The key of the machine this sink answers for: the key its steps are
    /// marked with (see `Core::key`).
    fn key(&self) -> usize {
        match self {
            Sink::Call { plane, .. } => plane.record().key(),
            Sink::Log { alive, .. } => Arc::as_ptr(alive) as usize,
        }
    }

    /// Answers `NoReply` while the machine runs. Otherwise nothing is sent:
    /// the sink drops, and the caller reads the machine's end.
    fn no_reply(self) {
        match self {
            Sink::Call {
                mut hold,
                plane,
                lane,
            } => {
                if machine_running(&plane, &lane) {
                    if let Some(tx) = hold.take() {
                        let _ = send_contained(tx, Err(ActorError::NoReply));
                    }
                }
            }
            Sink::Log { log, number, alive } => {
                if alive.load(Ordering::Acquire) {
                    let _ = log.send((number, None));
                }
            }
        }
    }
}

/// Whether the machine of an actor runs: its status plane reads `Running`, no
/// final reason is written on the plane, its stop lane holds no `Kill`, and no
/// abort of its task is recorded. Under a `Kill`, and in a task aborted after
/// its kill grace, the status still reads `Running` while the end drops the
/// queued calls and the machine's own state; the last three facts make those
/// drops answer nothing.
fn machine_running(plane: &StatusRx, lane: &StopLane) -> bool {
    read_status(plane) == ActorStatus::Running
        && !plane.record().has_final_reason()
        && !matches!(lane.reason(), Some(StopReason::Kill))
        && plane.record().view().0.is_none()
}

/// A reply sink held by a step's mark, with its reply type erased.
trait Held {
    /// Answers as a drop outside the step does: `NoReply` while the machine
    /// runs, nothing otherwise.
    fn answer(self: Box<Self>);
}

impl<R> Held for Sink<R> {
    fn answer(self: Box<Self>) {
        (*self).no_reply();
    }
}

fn erase<R: 'static>(sink: Sink<R>) -> Box<dyn Held> {
    Box::new(sink)
}

/// How many step keys a thread keeps inline; a deeper nesting of machines
/// keeps the rest in a list that grows once per thread.
const INLINE_STEPS: usize = 8;

thread_local! {
    /// The steps of machines this thread runs now, with the sinks they hold.
    /// Only [`StepMark`] pushes and pops a step.
    static STEPS: RefCell<Steps> = const { RefCell::new(Steps::new()) };
}

/// The stack of a thread's steps, outermost first, each named by its
/// machine's key, and the sinks dropped inside them, each with the place of
/// the step that holds it. A key is the address of a block the machine and
/// each of its return addresses hold alive, so two live keys are equal only
/// for one machine, and no key is 0. A step that drops no return address
/// allocates nothing.
struct Steps {
    inline: [usize; INLINE_STEPS],
    deeper: Vec<usize>,
    depth: usize,
    held: Vec<(usize, Box<dyn Held>)>,
}

impl Steps {
    const fn new() -> Self {
        Self {
            inline: [0; INLINE_STEPS],
            deeper: Vec::new(),
            depth: 0,
            held: Vec::new(),
        }
    }

    fn key_at(&self, at: usize) -> usize {
        match at.checked_sub(INLINE_STEPS) {
            None => self.inline[at],
            Some(deep) => self.deeper[deep],
        }
    }

    /// Pushes a step of the machine named by `key` and returns its place.
    fn push(&mut self, key: usize) -> usize {
        let at = self.depth;
        match at.checked_sub(INLINE_STEPS) {
            None => self.inline[at] = key,
            Some(_) => self.deeper.push(key),
        }
        self.depth += 1;
        at
    }

    /// The place of the innermost step of the machine named by `key`.
    fn innermost(&self, key: usize) -> Option<usize> {
        (0..self.depth).rev().find(|&at| self.key_at(at) == key)
    }

    /// Pops the innermost step, at `at`, and takes the sinks it holds. The
    /// sinks of the steps around it stay.
    fn pop(&mut self, at: usize) -> Vec<Box<dyn Held>> {
        debug_assert_eq!(at + 1, self.depth, "steps end innermost first");
        self.depth = at;
        if let Some(deep) = at.checked_sub(INLINE_STEPS) {
            self.deeper.truncate(deep);
        }
        if self.held.iter().all(|(owner, _)| *owner != at) {
            return Vec::new();
        }
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|(owner, _)| *owner == at);
        self.held = rest;
        mine.into_iter().map(|(_, sink)| sink).collect()
    }
}

/// Moves `sink` to the list of the innermost step of its machine on this
/// thread, found anywhere on the stack of steps, and gives it back when no
/// step of its machine is on the stack.
fn hold_in_step<R>(sink: Sink<R>, erase: fn(Sink<R>) -> Box<dyn Held>) -> Option<Sink<R>> {
    let key = sink.key();
    let mut sink = Some(sink);
    let _ = STEPS.try_with(|steps| {
        if let Ok(mut steps) = steps.try_borrow_mut() {
            if let Some(at) = steps.innermost(key) {
                if let Some(sink) = sink.take() {
                    steps.held.push((at, erase(sink)));
                }
            }
        }
    });
    sink
}

/// Pops the step at `at` and takes the sinks it holds; nothing once the
/// thread's stack is gone.
fn pop_step(at: usize) -> Vec<Box<dyn Held>> {
    STEPS
        .try_with(|steps| steps.borrow_mut().pop(at))
        .unwrap_or_default()
}

/// Marks one step of a machine on this thread for the guard's life: a return
/// address of that machine dropped meanwhile is held by the mark, and the
/// step's end decides its answer. The engine ends a step that returns with
/// [`close`](Self::close), which answers `NoReply` for each held address
/// while the machine runs; a step that unwinds drops the guard, which drops
/// them with nothing sent, so their callers read the machine's failure. Both
/// put back the outer mark before they answer or drop, so a drop made while
/// they do is decided by the steps around it.
///
/// The guard belongs to the thread that made it, so it is not `Send`.
struct StepMark {
    /// The step's place, or `None` once the thread's stack is gone: the mark
    /// then holds nothing and each drop under it is answered as one outside
    /// any step of its machine.
    at: Option<usize>,
    open: bool,
    _thread: PhantomData<*const ()>,
}

impl StepMark {
    fn enter(key: usize) -> Self {
        let at = STEPS
            .try_with(|steps| steps.try_borrow_mut().ok().map(|mut s| s.push(key)))
            .ok()
            .flatten();
        Self {
            at,
            open: true,
            _thread: PhantomData,
        }
    }

    /// Ends a step that returns: answers each held address.
    fn close(mut self) {
        self.open = false;
        let Some(at) = self.at else { return };
        for sink in pop_step(at) {
            sink.answer();
        }
    }
}

impl Drop for StepMark {
    /// Ends a step that unwinds: the held addresses drop inside a catch with
    /// nothing sent.
    fn drop(&mut self) {
        if let (true, Some(at)) = (self.open, self.at) {
            drop_contained(pop_step(at));
        }
    }
}

/// Drops a value inside a catch: the drop of a user's value must not unwind
/// into the engine.
fn drop_contained<T>(value: T) {
    if let Err(payload) = catch_sync(move || drop(value)) {
        discard(payload);
    }
}

impl<R: 'static> ReplyTo<R> {
    fn call(tx: oneshot::Sender<Answer<R>>, plane: StatusRx, lane: StopLane) -> Self {
        Self {
            sink: Some(Sink::Call {
                hold: AnswerHold::new(Some(tx)),
                plane,
                lane,
            }),
            erase,
        }
    }
}

impl<R> ReplyTo<R> {
    /// Sends the reply and spends the return address.
    fn deliver(mut self, reply: R) {
        match self.sink.take() {
            Some(Sink::Call { mut hold, .. }) => match hold.take() {
                Some(tx) => {
                    if let Err(unsent) = send_contained(tx, Ok(reply)) {
                        drop_contained(unsent);
                    }
                }
                None => drop_contained(reply),
            },
            Some(Sink::Log { log, number, .. }) => {
                if let Err(unsent) = log.send((number, Some(reply))) {
                    drop_contained(unsent);
                }
            }
            None => drop_contained(reply),
        }
    }

    /// Closes the return address without an answer, whatever the machine's
    /// status, when it belongs to the machine named by `key`, which holds it
    /// and has ended or failed. An address of another machine stays armed:
    /// it is answered when it drops, by the rule of its own machine.
    fn disarm(&mut self, key: usize) {
        if self.sink.as_ref().is_some_and(|sink| sink.key() == key) {
            drop(self.sink.take());
        }
    }
}

impl<R> Drop for ReplyTo<R> {
    /// Inside a step of its machine on this thread the step's mark holds the
    /// sink; with no such step it answers now.
    fn drop(&mut self) {
        if let Some(sink) = self.sink.take() {
            if let Some(sink) = hold_in_step(sink, self.erase) {
                sink.no_reply();
            }
        }
    }
}

impl<R> fmt::Debug for ReplyTo<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReplyTo").finish_non_exhaustive()
    }
}

/// The calls an event function made, recorded to be performed after it returns.
struct Recorder<M: StateMachine> {
    replies: Replies<M::Reply>,
    postponed: Vec<StatemEvent<M>>,
    inserted: Vec<M::Event>,
    ops: Vec<Op<M>>,
    /// How many more events the engine may hold.
    room: usize,
    /// The running time-outs, lent by the engine for the call.
    running: Timeouts<M::TimerName>,
}

/// A request for a time-out, in call order.
enum Op<M: StateMachine> {
    Set(Fired<M::TimerName>, Duration, M::Event),
    Cancel(Fired<M::TimerName>),
}

impl<M: StateMachine> Recorder<M> {
    fn new(key: usize, room: usize, running: Timeouts<M::TimerName>) -> Self {
        Self {
            replies: Replies {
                key,
                sent: Vec::new(),
            },
            postponed: Vec::new(),
            inserted: Vec::new(),
            ops: Vec::new(),
            room,
            running,
        }
    }

    fn is_running(&self, kind: &Fired<M::TimerName>) -> bool {
        for op in self.ops.iter().rev() {
            match op {
                Op::Set(k, ..) if k == kind => return true,
                Op::Cancel(k) if k == kind => return false,
                _ => {}
            }
        }
        self.running.get(kind).is_some()
    }

    fn cancel(&mut self, kind: Fired<M::TimerName>) -> bool {
        let was = self.is_running(&kind);
        self.ops.push(Op::Cancel(kind));
        was
    }

    fn postpone(&mut self, event: StatemEvent<M>) -> Result<(), StatemEvent<M>> {
        if self.room == 0 {
            return Err(event);
        }
        self.room -= 1;
        self.postponed.push(event);
        Ok(())
    }

    fn next_event(&mut self, event: M::Event) -> Result<(), M::Event> {
        if self.room == 0 {
            return Err(event);
        }
        self.room -= 1;
        self.inserted.push(event);
        Ok(())
    }
}

/// Replies recorded by a callback of the machine named by `key`: each is a
/// return address with its value. A value dropped unsent is dropped inside a
/// catch.
struct Replies<R> {
    key: usize,
    sent: Vec<(ReplyTo<R>, R)>,
}

impl<R> Replies<R> {
    /// Sends every reply, in call order.
    fn send_all(&mut self) {
        let mut all = std::mem::take(&mut self.sent);
        all.reverse();
        while let Some((to, reply)) = all.pop() {
            to.deliver(reply);
        }
    }
}

impl<R> Drop for Replies<R> {
    /// An address of this machine closes with nothing sent; an address of
    /// another machine drops as any `ReplyTo` does.
    fn drop(&mut self) {
        for (mut to, reply) in self.sent.drain(..) {
            to.disarm(self.key);
            drop_contained(reply);
            drop(to);
        }
    }
}

/// The running time-outs of a machine, each by the generation it was set with.
struct Timeouts<N> {
    event: Option<u64>,
    state: Option<u64>,
    named: HashMap<N, u64>,
}

impl<N> Default for Timeouts<N> {
    fn default() -> Self {
        Self {
            event: None,
            state: None,
            named: HashMap::new(),
        }
    }
}

impl<N: Clone + Eq + Hash> Timeouts<N> {
    fn get(&self, kind: &Fired<N>) -> Option<u64> {
        match kind {
            Fired::Event => self.event,
            Fired::State => self.state,
            Fired::Named(name) => self.named.get(name).copied(),
        }
    }

    fn set(&mut self, kind: &Fired<N>, generation: u64) {
        match kind {
            Fired::Event => self.event = Some(generation),
            Fired::State => self.state = Some(generation),
            Fired::Named(name) => {
                self.named.insert(name.clone(), generation);
            }
        }
    }

    fn take(&mut self, kind: &Fired<N>) -> Option<u64> {
        match kind {
            Fired::Event => self.event.take(),
            Fired::State => self.state.take(),
            Fired::Named(name) => self.named.remove(name),
        }
    }
}

/// An event waiting in the engine's queue, with the time-out it stands for.
struct Queued<M: StateMachine> {
    event: StatemEvent<M>,
    due: Option<(Fired<M::TimerName>, u64)>,
}

impl<M: StateMachine> Queued<M> {
    fn plain(event: StatemEvent<M>) -> Self {
        Self { event, due: None }
    }

    fn firing(kind: Fired<M::TimerName>, generation: u64, event: M::Event) -> Self {
        let event = match kind.clone() {
            Fired::Event => StatemEvent::EventTimeout(event),
            Fired::State => StatemEvent::StateTimeout(event),
            Fired::Named(name) => StatemEvent::Timeout(name, event),
        };
        Self {
            event,
            due: Some((kind, generation)),
        }
    }
}

/// What the engine asks its shell to do with a time-out.
enum Effect<M: StateMachine> {
    /// Start a time-out; `event` is `None` for a zero duration, whose event the
    /// engine queued itself.
    Set {
        kind: Fired<M::TimerName>,
        generation: u64,
        after: Duration,
        event: Option<M::Event>,
    },
    Cancel {
        kind: Fired<M::TimerName>,
        generation: u64,
    },
}

/// What the event function and the transition did, for the shell to act on.
struct Handled<M: StateMachine> {
    replies: Replies<M::Reply>,
    /// The state left, when an enter call follows.
    enter: Option<M::State>,
    stop: Option<StopReason>,
    /// How many events the call inserted at the front of the queue.
    inserted: usize,
}

/// The engine: the machine, its state and its queues. It takes one event at a
/// time and returns what to do; it cannot await, sleep, spawn or read a clock.
struct Core<M: StateMachine> {
    machine: M,
    state: M::State,
    postponed: VecDeque<StatemEvent<M>>,
    queue: VecDeque<Queued<M>>,
    timeouts: Timeouts<M::TimerName>,
    /// The one counter of time-out generations; it is never reset.
    next_generation: u64,
    capacity: usize,
    effects: Vec<Effect<M>>,
    ended: bool,
    /// The key the machine's steps are marked with: the address of its
    /// actor's status block, or of its harness's liveness flag.
    key: usize,
}

/// The mailbox capacity a [`Harness`] bounds its held events by.
const DEFAULT_CAPACITY: usize = 64;

impl<M: StateMachine> Core<M> {
    fn new(machine: M, state: M::State, capacity: usize) -> Self {
        Self {
            machine,
            state,
            postponed: VecDeque::new(),
            queue: VecDeque::new(),
            timeouts: Timeouts::default(),
            next_generation: 0,
            capacity,
            effects: Vec::new(),
            ended: false,
            key: 0,
        }
    }

    fn room(&self) -> usize {
        self.capacity
            .saturating_sub(self.postponed.len() + self.queue.len())
    }

    fn cancel_running(&mut self, kind: Fired<M::TimerName>) {
        if let Some(generation) = self.timeouts.take(&kind) {
            self.effects.push(Effect::Cancel { kind, generation });
        }
    }

    /// Takes an event out of the queue or the mailbox: a stale firing is
    /// dropped, and an event other than the event time-out cancels it.
    fn admit(&mut self, queued: Queued<M>) -> Option<StatemEvent<M>> {
        let Queued { event, due } = queued;
        let own = matches!(due, Some((Fired::Event, _)));
        if let Some((kind, generation)) = due {
            if self.timeouts.get(&kind) == Some(generation) {
                self.timeouts.take(&kind);
            } else {
                drop_contained(event);
                return None;
            }
        }
        if !own {
            self.cancel_running(Fired::Event);
        }
        Some(event)
    }

    /// Runs the event function and applies what it recorded. The replies it
    /// recorded come back first: the shell sends them before the enter call.
    fn handle(
        &mut self,
        event: StatemEvent<M>,
        actx: Option<&mut ActorContext<StatemActor<M>>>,
    ) -> Handled<M> {
        // The mark covers the whole step, the engine's own work on the state
        // and the time-outs included, and it outlives every value of the step
        // on an unwind, so what the step drops is dropped as part of its
        // unwind.
        let mark = StepMark::enter(self.key);
        let mut rec = Recorder::new(self.key, self.room(), std::mem::take(&mut self.timeouts));
        let transition = {
            let mut cx = StatemContext {
                ctx: actx,
                rec: &mut rec,
            };
            self.machine.handle_event(&self.state, event, &mut cx)
        };
        let Recorder {
            replies,
            mut postponed,
            inserted,
            ops,
            running,
            ..
        } = rec;
        self.timeouts = running;
        let mut handled = Handled {
            replies,
            enter: None,
            stop: None,
            inserted: inserted.len(),
        };
        let mut changed = false;
        match transition {
            Transition::Next(next) => {
                if next != self.state {
                    handled.enter = Some(std::mem::replace(&mut self.state, next));
                    changed = true;
                }
            }
            Transition::Keep => {}
            Transition::Repeat => handled.enter = Some(self.state.clone()),
            Transition::Stop(reason) => {
                disarm_events(&mut postponed, self.key);
                drop_contained((postponed, inserted, ops));
                handled.stop = Some(reason);
                self.end();
                StepMark::close(mark);
                return handled;
            }
        }
        if changed {
            self.cancel_running(Fired::State);
        }
        self.apply(ops);
        self.postponed.extend(postponed);
        if changed {
            while let Some(event) = self.postponed.pop_back() {
                self.queue.push_front(Queued::plain(event));
            }
        }
        for event in inserted.into_iter().rev() {
            self.queue
                .push_front(Queued::plain(StatemEvent::Internal(event)));
        }
        StepMark::close(mark);
        handled
    }

    /// Runs the enter call for the state now held, entered from `old`.
    fn enter(
        &mut self,
        old: &M::State,
        actx: Option<&mut ActorContext<StatemActor<M>>>,
    ) -> Replies<M::Reply> {
        let mark = StepMark::enter(self.key);
        let mut rec = Recorder::new(self.key, 0, std::mem::take(&mut self.timeouts));
        {
            let mut cx = EnterContext {
                ctx: actx,
                rec: &mut rec,
            };
            self.machine.on_enter(old, &self.state, &mut cx);
        }
        let Recorder {
            replies,
            ops,
            running,
            ..
        } = rec;
        self.timeouts = running;
        self.apply(ops);
        StepMark::close(mark);
        replies
    }

    /// Performs the recorded time-out requests, in call order.
    fn apply(&mut self, ops: Vec<Op<M>>) {
        for op in ops {
            match op {
                Op::Cancel(kind) => self.cancel_running(kind),
                Op::Set(kind, after, event) => {
                    self.cancel_running(kind.clone());
                    let generation = self.next_generation;
                    self.next_generation += 1;
                    self.timeouts.set(&kind, generation);
                    if after.is_zero() {
                        self.effects.push(Effect::Set {
                            kind: kind.clone(),
                            generation,
                            after,
                            event: None,
                        });
                        self.queue
                            .push_back(Queued::firing(kind, generation, event));
                    } else {
                        self.effects.push(Effect::Set {
                            kind,
                            generation,
                            after,
                            event: Some(event),
                        });
                    }
                }
            }
        }
    }

    /// Ends the machine: nothing queued or postponed is handled again, and the
    /// calls it holds end with it. An address of another machine among them
    /// is answered by that machine's rule.
    fn end(&mut self) {
        self.ended = true;
        let mut postponed: Vec<_> = self.postponed.drain(..).collect();
        disarm_events(&mut postponed, self.key);
        let mut queued: Vec<_> = self.queue.drain(..).map(|q| q.event).collect();
        disarm_events(&mut queued, self.key);
        drop_contained((postponed, queued));
    }
}

impl<M: StateMachine> Drop for Core<M> {
    fn drop(&mut self) {
        self.end();
    }
}

/// Closes the return address of every call among `events` that belongs to
/// the machine named by `key`; the others drop with the events.
fn disarm_events<M: StateMachine>(events: &mut [StatemEvent<M>], key: usize) {
    for event in events {
        if let StatemEvent::Call(_, to) = event {
            to.disarm(key);
        }
    }
}

/// What [`StateMachine::handle_event`] may ask for besides its transition.
///
/// Each request is recorded and performed after the function returns: the
/// transition, then the function's time-out requests in call order, then the
/// replies in call order, then, when the transition is a state change or
/// `Repeat`, the enter call with its own replies and time-out requests; the
/// timers start last. A later time-out request of one kind (the event
/// time-out, the state time-out, or one name) replaces an earlier one; replies,
/// postponed events and inserted events add up. A `Transition::Stop` sends the
/// replies and performs nothing else.
pub struct StatemContext<'a, M: StateMachine> {
    ctx: Option<&'a mut ActorContext<StatemActor<M>>>,
    rec: &'a mut Recorder<M>,
}

impl<M: StateMachine> fmt::Debug for StatemContext<'_, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatemContext").finish_non_exhaustive()
    }
}

/// Panics, as `ActorContext::schedule` does, when the actor's runtime has no
/// time driver; a harness has no actor and no check.
fn require_time_driver<M: StateMachine>(ctx: &mut Option<&mut ActorContext<StatemActor<M>>>) {
    if let Some(ctx) = ctx.as_deref_mut() {
        drop(ctx.schedule(StatemMessage::step()));
    }
}

impl<M: StateMachine> StatemContext<'_, M> {
    /// The machine's actor context, or `None` when a [`Harness`] drives the
    /// machine. The registrations that do not await are made here: `monitor`,
    /// `request`, `subscribe` and `trap_exit` take a map that builds its message
    /// through [`StatemMessage::cast`], `add_stream` needs an event type that
    /// converts from `StreamEvent`, and `link` and `alias` take no map; the
    /// ones that await are made in [`StateMachine::init`]. A message given to
    /// `continue_with` here is handled after the inserted events of this call.
    pub fn actor(&mut self) -> Option<&mut ActorContext<StatemActor<M>>> {
        self.ctx.as_deref_mut()
    }

    /// Answers a call.
    pub fn reply(&mut self, to: ReplyTo<M::Reply>, reply: M::Reply) {
        self.rec.replies.sent.push((to, reply));
    }

    /// Postpones `event`: it is handled again, oldest first, after the next
    /// state change. Refused, with the event handed back, when the events the
    /// machine holds (postponed events, inserted events, queued zero-duration
    /// time-out events and an event left by `continue_with`) have reached the
    /// mailbox capacity; under a [`Harness`] the bound is 64.
    pub fn postpone(&mut self, event: StatemEvent<M>) -> Result<(), StatemEvent<M>> {
        self.rec.postpone(event)
    }

    /// Inserts an event that is handled before any postponed or mailbox event.
    /// Refused under the same bound as [`Self::postpone`].
    pub fn next_event(&mut self, event: M::Event) -> Result<(), M::Event> {
        self.rec.next_event(event)
    }

    /// Starts the event time-out; any event handled first cancels it.
    ///
    /// # Panics
    ///
    /// Under [`StatemActor`] the three time-out setters panic at the call on a
    /// runtime without a time driver, as `ActorContext::schedule` does.
    pub fn event_timeout(&mut self, after: Duration, event: M::Event) {
        require_time_driver(&mut self.ctx);
        self.rec.ops.push(Op::Set(Fired::Event, after, event));
    }

    /// Cancels the event time-out. True when it was running; a cancelled
    /// time-out never reaches `handle_event`.
    pub fn cancel_event_timeout(&mut self) -> bool {
        self.rec.cancel(Fired::Event)
    }

    /// Starts the state time-out; a state change cancels it. Started while the
    /// state changes, it belongs to the new state.
    pub fn state_timeout(&mut self, after: Duration, event: M::Event) {
        require_time_driver(&mut self.ctx);
        self.rec.ops.push(Op::Set(Fired::State, after, event));
    }

    /// Cancels the state time-out. True when it was running; a cancelled
    /// time-out never reaches `handle_event`.
    pub fn cancel_state_timeout(&mut self) -> bool {
        self.rec.cancel(Fired::State)
    }

    /// Starts a generic time-out by name. It fires once; events and state
    /// changes leave it running, and before it fires only
    /// [`Self::cancel_timeout`], a new time-out of the same name or the end of
    /// the machine stops it.
    pub fn timeout(&mut self, name: M::TimerName, after: Duration, event: M::Event) {
        require_time_driver(&mut self.ctx);
        self.rec.ops.push(Op::Set(Fired::Named(name), after, event));
    }

    /// Cancels a generic time-out. True when it was running; a cancelled
    /// time-out never reaches `handle_event`.
    pub fn cancel_timeout(&mut self, name: &M::TimerName) -> bool {
        self.rec.cancel(Fired::Named(name.clone()))
    }
}

/// What [`StateMachine::on_enter`] may ask for: replies and time-outs, no
/// postponing, no inserted event and no state change. Its time-out calls answer
/// and panic as those of [`StatemContext`] do.
pub struct EnterContext<'a, M: StateMachine> {
    ctx: Option<&'a mut ActorContext<StatemActor<M>>>,
    rec: &'a mut Recorder<M>,
}

impl<M: StateMachine> fmt::Debug for EnterContext<'_, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnterContext").finish_non_exhaustive()
    }
}

impl<M: StateMachine> EnterContext<'_, M> {
    /// The machine's actor context, or `None` under a [`Harness`], with the
    /// rules of [`StatemContext::actor`].
    pub fn actor(&mut self) -> Option<&mut ActorContext<StatemActor<M>>> {
        self.ctx.as_deref_mut()
    }

    /// Answers a call the machine kept.
    pub fn reply(&mut self, to: ReplyTo<M::Reply>, reply: M::Reply) {
        self.rec.replies.sent.push((to, reply));
    }

    /// Starts the event time-out.
    pub fn event_timeout(&mut self, after: Duration, event: M::Event) {
        require_time_driver(&mut self.ctx);
        self.rec.ops.push(Op::Set(Fired::Event, after, event));
    }

    /// Cancels the event time-out. True when it was running; a cancelled
    /// time-out never reaches `handle_event`.
    pub fn cancel_event_timeout(&mut self) -> bool {
        self.rec.cancel(Fired::Event)
    }

    /// Starts the state time-out of the state being entered.
    pub fn state_timeout(&mut self, after: Duration, event: M::Event) {
        require_time_driver(&mut self.ctx);
        self.rec.ops.push(Op::Set(Fired::State, after, event));
    }

    /// Cancels the state time-out. True when it was running; a cancelled
    /// time-out never reaches `handle_event`.
    pub fn cancel_state_timeout(&mut self) -> bool {
        self.rec.cancel(Fired::State)
    }

    /// Starts a generic time-out by name.
    pub fn timeout(&mut self, name: M::TimerName, after: Duration, event: M::Event) {
        require_time_driver(&mut self.ctx);
        self.rec.ops.push(Op::Set(Fired::Named(name), after, event));
    }

    /// Cancels a generic time-out. True when it was running; a cancelled
    /// time-out never reaches `handle_event`.
    pub fn cancel_timeout(&mut self, name: &M::TimerName) -> bool {
        self.rec.cancel(Fired::Named(name.clone()))
    }
}

/// The message type of a [`StatemActor`]. Clients use [`ActorHandle::call`],
/// [`ActorHandle::call_timeout`] and [`ActorHandle::cast`]; a map that turns a
/// notice into an event builds one with [`Self::cast`].
pub struct StatemMessage<M: StateMachine> {
    kind: Kind<M>,
}

enum Kind<M: StateMachine> {
    Cast(M::Event),
    Call(M::Call, ReplyTo<M::Reply>),
    /// A time-out set with this generation fired.
    Fire(Fired<M::TimerName>, u64, M::Event),
    /// The engine's own step: it handles the next event of its queue.
    Step,
}

impl<M: StateMachine> fmt::Debug for StatemMessage<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatemMessage").finish_non_exhaustive()
    }
}

impl<M: StateMachine> StatemMessage<M> {
    /// The message that delivers `event` to the machine as [`StatemEvent::Cast`].
    /// Every generic verb of the handle carries it: `notify_priority` makes it a
    /// priority cast, and `send` answers `()` once the event has been handled.
    pub fn cast(event: M::Event) -> Self {
        Self {
            kind: Kind::Cast(event),
        }
    }

    fn step() -> Self {
        Self { kind: Kind::Step }
    }
}

impl<M, T> From<StreamEvent<T>> for StatemMessage<M>
where
    M: StateMachine,
    M::Event: From<StreamEvent<T>>,
{
    fn from(item: StreamEvent<T>) -> Self {
        Self::cast(item.into())
    }
}

/// The actor that runs a [`StateMachine`].
pub struct StatemActor<M: StateMachine> {
    core: Core<M>,
    /// The timer of each running time-out, by generation.
    timers: HashMap<u64, RecurringId>,
}

impl<M: StateMachine> fmt::Debug for StatemActor<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatemActor").finish_non_exhaustive()
    }
}

impl<M: StateMachine> StatemActor<M> {
    /// A machine that starts in `initial`.
    pub fn new(machine: M, initial: M::State) -> Self {
        Self {
            core: Core::new(machine, initial, DEFAULT_CAPACITY),
            timers: HashMap::new(),
        }
    }

    /// Starts and cancels the timers the engine asked for.
    async fn perform(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        for effect in std::mem::take(&mut self.core.effects) {
            match effect {
                Effect::Set {
                    kind,
                    generation,
                    after,
                    event: Some(event),
                } => {
                    let id = ctx
                        .schedule(StatemMessage {
                            kind: Kind::Fire(kind, generation, event),
                        })
                        .after(after)
                        .await?;
                    self.timers.insert(generation, id);
                }
                Effect::Set { .. } => {}
                Effect::Cancel { generation, .. } => {
                    if let Some(id) = self.timers.remove(&generation) {
                        ctx.cancel_timer(id);
                    }
                }
            }
        }
        Ok(())
    }

    /// Ends a turn: a message the event function left in the continue cell goes
    /// behind the events it inserted, and the engine's own step is set while its
    /// queue holds an event.
    fn finish(&mut self, ctx: &mut ActorContext<Self>, inserted: usize) {
        if let Some(message) = ctx.take_continuation() {
            match message.kind {
                Kind::Cast(event) if !self.core.ended => {
                    let at = inserted.min(self.core.queue.len());
                    self.core
                        .queue
                        .insert(at, Queued::plain(StatemEvent::Cast(event)));
                }
                other => drop_contained(other),
            }
        }
        if !self.core.ended && !self.core.queue.is_empty() {
            ctx.continue_with(StatemMessage::step());
        }
    }

    /// Handles one event, then its enter call, and performs the effects.
    async fn turn(&mut self, queued: Queued<M>, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.core.ended {
            drop_contained(queued.event);
            return Ok(());
        }
        let Some(event) = self.core.admit(queued) else {
            self.perform(ctx).await?;
            self.finish(ctx, 0);
            return Ok(());
        };
        let mut handled = self.core.handle(event, Some(&mut *ctx));
        handled.replies.send_all();
        if let Some(reason) = handled.stop.take() {
            return match ctx.stop(reason) {
                Ok(()) | Err(StopError::Closed) => Ok(()),
                Err(StopError::KillRefused) => Err(ActorError::user(
                    "a state machine cannot stop its actor with Kill",
                )),
            };
        }
        if let Some(old) = handled.enter.take() {
            let mut replies = self.core.enter(&old, Some(&mut *ctx));
            replies.send_all();
        }
        self.perform(ctx).await?;
        self.finish(ctx, handled.inserted);
        Ok(())
    }
}

impl<M: StateMachine> Actor for StatemActor<M> {
    type Message = StatemMessage<M>;
    type Response = ();

    async fn pre_start(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.core.machine.init(ctx).await
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.core.capacity = ctx.self_handle().mailbox_capacity();
        self.core.key = ctx.self_handle_ref().status_rx().record().key();
        let initial = self.core.state.clone();
        let mut replies = self.core.enter(&initial, Some(&mut *ctx));
        replies.send_all();
        self.perform(ctx).await?;
        self.finish(ctx, 0);
        Ok(())
    }

    async fn handle(
        &mut self,
        msg: StatemMessage<M>,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let queued = match msg.kind {
            Kind::Cast(event) => Queued::plain(StatemEvent::Cast(event)),
            Kind::Call(call, to) => Queued::plain(StatemEvent::Call(call, to)),
            Kind::Fire(kind, generation, event) => {
                self.timers.remove(&generation);
                Queued::firing(kind, generation, event)
            }
            Kind::Step => match self.core.queue.pop_front() {
                Some(queued) => queued,
                None => return Ok(()),
            },
        };
        self.turn(queued, ctx).await
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.core.end();
        self.core.machine.terminate(reason, &self.core.state);
        Ok(())
    }
}

/// The answer of a `Result` read from a call's channel.
fn read<R>(answer: Answer<R>) -> Result<R, AskError> {
    answer.map_err(AskError::Actor)
}

/// Waits for the answer of a call whose message was accepted: the reply, or the
/// machine's end, whichever comes first. A reply sent before the end is read
/// even when the end is seen first. A return address already closed without a
/// reply when the wait starts gives the machine's end, as one closed during the
/// wait does; the receiver is never polled or read once it has answered.
async fn settle<R>(
    mut plane: StatusRx,
    mut rx: oneshot::Receiver<Answer<R>>,
) -> Result<R, AskError> {
    match rx.try_recv() {
        Ok(answer) => return read(answer),
        Err(oneshot::error::TryRecvError::Closed) => {
            return Err(AskError::ResponseDropped {
                reason: wait_final_reason(&mut plane).await,
            });
        }
        Err(oneshot::error::TryRecvError::Empty) => {}
    }
    let reason = tokio::select! {
        biased;
        got = &mut rx => {
            return match got {
                Ok(answer) => read(answer),
                Err(_) => Err(AskError::ResponseDropped {
                    reason: wait_final_reason(&mut plane).await,
                }),
            };
        }
        reason = wait_final_reason(&mut plane) => reason,
    };
    match rx.try_recv() {
        Ok(answer) => read(answer),
        Err(_) => Err(AskError::ResponseDropped { reason }),
    }
}

impl<M: StateMachine> ActorHandle<StatemActor<M>> {
    /// Calls the machine and waits for its reply, without a bound.
    ///
    /// The wait ends with the reply; with [`AskError::Actor`] carrying
    /// [`ActorError::NoReply`] when the running machine dropped the call's
    /// [`ReplyTo`]; with [`AskError::ResponseDropped`] and the final reason when
    /// the machine ended first, wherever the `ReplyTo` is; with
    /// [`AskError::Closed`] when the mailbox closed before the call was queued;
    /// with [`AskError::CallingSelf`] when made from the machine's own task. A
    /// reply the machine sent is read even when its end is seen first.
    pub async fn call(&self, call: M::Call) -> Result<M::Reply, AskError> {
        let (tx, rx) = oneshot::channel();
        let message = StatemMessage {
            kind: Kind::Call(call, ReplyTo::call(tx, self.status_rx(), self.stop_lane())),
        };
        match self.send(message).await {
            Ok(()) | Err(AskError::Actor(_) | AskError::ResponseDropped { .. }) => {}
            Err(other) => return Err(other),
        }
        settle(self.status_rx(), rx).await
    }

    /// Calls the machine and waits for its reply for at most `within`, with the
    /// answers and the time-driver rule of `send_timeout`.
    pub async fn call_timeout(
        &self,
        call: M::Call,
        within: Duration,
    ) -> Result<M::Reply, AskError> {
        let start = Instant::now();
        let (tx, rx) = oneshot::channel();
        let message = StatemMessage {
            kind: Kind::Call(call, ReplyTo::call(tx, self.status_rx(), self.stop_lane())),
        };
        match self.send_timeout(message, within).await {
            Ok(()) | Err(AskError::Actor(_) | AskError::ResponseDropped { .. }) => {}
            Err(other) => return Err(other),
        }
        let deadline = start
            .checked_add(within)
            .unwrap_or_else(|| start + Duration::from_secs(86_400 * 365 * 30));
        match timeout_at(deadline, settle(self.status_rx(), rx)).await {
            Ok(answer) => answer,
            Err(_) => Err(AskError::Timeout { enqueued: true }),
        }
    }

    /// Sends an event to the machine, waiting for room in its mailbox.
    pub async fn cast(&self, event: M::Event) -> Result<(), SendError> {
        self.notify(StatemMessage::cast(event)).await
    }

    /// Sends an event to the machine without waiting.
    pub fn try_cast(&self, event: M::Event) -> Result<(), TrySendError> {
        self.try_notify(StatemMessage::cast(event))
    }
}

/// A kind of time-out a [`Harness`] can fire; the kind of a [`TimeoutId`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Fired<N> {
    /// The event time-out.
    Event,
    /// The state time-out.
    State,
    /// A generic time-out by name.
    Named(N),
}

/// The id of one setting of a time-out under a [`Harness`]; [`Step::set`] and
/// [`Step::cancelled`] carry it. Every setting gets an id of its own, so a
/// second setting of the same kind, or a setting after a cancel, has an id
/// unequal to the first. An id names a setting of the harness whose step
/// reported it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TimeoutId<N> {
    kind: Fired<N>,
    generation: u64,
}

impl<N> TimeoutId<N> {
    /// The kind of time-out this id names.
    pub fn kind(&self) -> &Fired<N> {
        &self.kind
    }
}

/// What one step of a [`Harness`] did.
pub struct Step<M: StateMachine> {
    /// The calls answered in this step, in order: the number of the call
    /// (calls are numbered from 0 in the order of [`Harness::call`]) and its
    /// reply, or `None` when its [`ReplyTo`] was dropped unanswered.
    pub replies: Vec<(usize, Option<M::Reply>)>,
    /// The time-outs set in this step, in order, each with its duration. An
    /// entry with a zero duration is not held: its event is handled in this
    /// step, and a firing of its id does nothing.
    pub set: Vec<(TimeoutId<M::TimerName>, Duration)>,
    /// The time-outs cancelled in this step, by the machine or by the engine,
    /// in order. A setting can be both set and cancelled in one step.
    pub cancelled: Vec<TimeoutId<M::TimerName>>,
    /// True when this step was a firing that named no running time-out: the
    /// setting was fired, cancelled or replaced already, or the machine has
    /// stopped. Such a step does nothing else.
    pub stale: bool,
    /// How many events are postponed after this step.
    pub postponed: usize,
    /// The states entered in this step, one per enter call, in order.
    pub entered: Vec<M::State>,
    /// The reason, when the machine stopped in this step.
    pub stopped: Option<StopReason>,
}

impl<M: StateMachine> fmt::Debug for Step<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Step")
            .field("replies", &self.replies.len())
            .field("set", &self.set.len())
            .field("cancelled", &self.cancelled.len())
            .field("postponed", &self.postponed)
            .field("entered", &self.entered.len())
            .field("stopped", &self.stopped)
            .field("stale", &self.stale)
            .finish()
    }
}

/// A time-out the harness holds until it is fired or cancelled.
struct Armed<M: StateMachine> {
    kind: Fired<M::TimerName>,
    generation: u64,
    event: M::Event,
}

/// Drives a [`StateMachine`] with no runtime and no clock, for table tests:
/// each call feeds one event and returns what the machine did. Postponed and
/// inserted events are handled inside the step that releases them.
///
/// The harness runs the same engine as [`StatemActor`], with these differences:
/// `init` is not run; the context's `actor()` answers `None`, so a registration
/// made through it leaves no trace; a time-out with a duration fires only when
/// the caller fires it, by [`Self::fire`] (the running setting of a kind) or
/// by [`Self::fire_id`] (only the setting an id names), and a zero duration
/// time-out fires inside the step that sets it; the time-out setters make no
/// time-driver check; the held events are bounded by a fixed 64; a panic of a
/// callback unwinds into the test. Its return
/// addresses follow the rule of [`ReplyTo`]: one dropped in a step is answered
/// `None` when the step ends, one dropped in a panic the callback itself
/// catches included, and one dropped in a step that unwinds has no entry. A
/// machine that stops runs `terminate`, unless it is stopped with
/// `StopReason::Kill`, and a step fed after that does nothing.
///
/// A driver that runs the time-outs on real timers starts one timer for each
/// entry of [`Step::set`] with a non-zero duration, carrying its
/// [`TimeoutId`], and fires it with [`Self::fire_id`]. The timer of a setting
/// that was cancelled, replaced or already fired gives a step that reads
/// `stale` and does nothing else. To cancel timers, handle `set` first, then
/// `cancelled`, since one setting can be in both. Once the machine stops,
/// every firing reads `stale`. An id names a setting of the harness whose step
/// reported it.
///
/// `Harness` is `Send`, and `Sync` when the machine and its associated types
/// are `Sync`.
pub struct Harness<M: StateMachine> {
    core: Core<M>,
    /// False once the machine has stopped; every return address reads it.
    alive: Arc<AtomicBool>,
    calls: usize,
    log_tx: mpsc::Sender<(usize, Option<M::Reply>)>,
    /// The reply log; the lock is never taken, it makes the harness `Sync`.
    log: Mutex<mpsc::Receiver<(usize, Option<M::Reply>)>>,
    armed: Vec<Armed<M>>,
}

impl<M: StateMachine> fmt::Debug for Harness<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Harness").finish_non_exhaustive()
    }
}

impl<M: StateMachine> Harness<M> {
    /// A machine in `initial`, with the step of its first enter call.
    pub fn new(machine: M, initial: M::State) -> (Self, Step<M>) {
        let (log_tx, log) = mpsc::channel();
        let mut harness = Self {
            core: Core::new(machine, initial, DEFAULT_CAPACITY),
            alive: Arc::new(AtomicBool::new(true)),
            calls: 0,
            log_tx,
            log: Mutex::new(log),
            armed: Vec::new(),
        };
        harness.core.key = Arc::as_ptr(&harness.alive) as usize;
        let mut step = harness.blank();
        let initial = harness.core.state.clone();
        harness.enter(&initial, &mut step);
        harness.drain(&mut step);
        harness.close(&mut step);
        (harness, step)
    }

    /// Feeds a cast.
    pub fn cast(&mut self, event: M::Event) -> Step<M> {
        self.run(Queued::plain(StatemEvent::Cast(event)))
    }

    /// Feeds a call. Its reply is in the step that answers it, under the
    /// call's number.
    pub fn call(&mut self, call: M::Call) -> Step<M> {
        let to = ReplyTo {
            sink: Some(Sink::Log {
                log: self.log_tx.clone(),
                number: self.calls,
                alive: Arc::clone(&self.alive),
            }),
            erase,
        };
        self.calls += 1;
        self.run(Queued::plain(StatemEvent::Call(call, to)))
    }

    /// Fires the running time-out of this kind, whichever setting it is; a
    /// kind with no running time-out does nothing, and the step reads `stale`.
    pub fn fire(&mut self, timeout: Fired<M::TimerName>) -> Step<M> {
        let at = self.armed.iter().position(|armed| armed.kind == timeout);
        self.fire_at(at)
    }

    /// Fires the time-out of this setting while it is the running one; a
    /// setting that is not the running one does nothing, and the step reads
    /// `stale`.
    pub fn fire_id(&mut self, id: &TimeoutId<M::TimerName>) -> Step<M> {
        let at = self
            .armed
            .iter()
            .position(|armed| armed.generation == id.generation && armed.kind == id.kind);
        self.fire_at(at)
    }

    fn fire_at(&mut self, at: Option<usize>) -> Step<M> {
        let Some(at) = at else {
            let mut step = self.blank();
            step.stale = true;
            self.close(&mut step);
            return step;
        };
        let armed = self.armed.remove(at);
        self.run(Queued::firing(armed.kind, armed.generation, armed.event))
    }

    /// Stops the machine from outside with `reason`, as a stop of its actor
    /// does: `terminate` runs unless `reason` is `StopReason::Kill`. A call the
    /// machine still holds has no entry in the step's replies; under the actor
    /// its caller reads the machine's end.
    pub fn stop(&mut self, reason: StopReason) -> Step<M> {
        let mut step = self.blank();
        if !self.core.ended {
            self.finish_stop(reason, &mut step);
        }
        self.close(&mut step);
        step
    }

    /// False once the machine has stopped.
    pub fn running(&self) -> bool {
        !self.core.ended
    }

    /// The current state.
    pub fn state(&self) -> &M::State {
        &self.core.state
    }

    /// The machine's data.
    pub fn machine(&self) -> &M {
        &self.core.machine
    }

    /// The machine's data, to change between steps.
    pub fn machine_mut(&mut self) -> &mut M {
        &mut self.core.machine
    }

    fn blank(&self) -> Step<M> {
        Step {
            replies: Vec::new(),
            set: Vec::new(),
            cancelled: Vec::new(),
            postponed: 0,
            entered: Vec::new(),
            stopped: None,
            stale: false,
        }
    }

    fn run(&mut self, first: Queued<M>) -> Step<M> {
        let mut step = self.blank();
        if !self.core.ended {
            self.feed(first, &mut step);
            self.drain(&mut step);
        } else {
            drop_contained(first.event);
        }
        self.close(&mut step);
        step
    }

    /// Handles the engine's queued events until it is empty or the machine
    /// stops.
    fn drain(&mut self, step: &mut Step<M>) {
        while !self.core.ended {
            let Some(queued) = self.core.queue.pop_front() else {
                break;
            };
            self.feed(queued, step);
        }
    }

    fn feed(&mut self, queued: Queued<M>, step: &mut Step<M>) {
        let Some(event) = self.core.admit(queued) else {
            return;
        };
        let mut handled = self.core.handle(event, None);
        handled.replies.send_all();
        if let Some(reason) = handled.stop.take() {
            let reason = match reason {
                StopReason::Kill => StopReason::Failure(ActorError::user(
                    "a state machine cannot stop its actor with Kill",
                )),
                other => other,
            };
            self.alive.store(false, Ordering::Release);
            self.core.machine.terminate(&reason, &self.core.state);
            step.stopped = Some(reason);
            return;
        }
        if let Some(old) = handled.enter.take() {
            self.enter(&old, step);
        }
    }

    fn enter(&mut self, old: &M::State, step: &mut Step<M>) {
        step.entered.push(self.core.state.clone());
        let mut replies = self.core.enter(old, None);
        replies.send_all();
    }

    fn finish_stop(&mut self, reason: StopReason, step: &mut Step<M>) {
        self.alive.store(false, Ordering::Release);
        self.core.end();
        if !matches!(reason, StopReason::Kill) {
            self.core.machine.terminate(&reason, &self.core.state);
        }
        step.stopped = Some(reason);
    }

    /// Collects what the step did: the time-outs, the replies and the count of
    /// postponed events.
    fn close(&mut self, step: &mut Step<M>) {
        for effect in std::mem::take(&mut self.core.effects) {
            match effect {
                Effect::Set {
                    kind,
                    generation,
                    after,
                    event,
                } => {
                    step.set.push((
                        TimeoutId {
                            kind: kind.clone(),
                            generation,
                        },
                        after,
                    ));
                    if let Some(event) = event {
                        self.armed.push(Armed {
                            kind,
                            generation,
                            event,
                        });
                    }
                }
                Effect::Cancel { kind, generation } => {
                    step.cancelled.push(TimeoutId { kind, generation });
                    self.armed.retain(|armed| armed.generation != generation);
                }
            }
        }
        if self.core.ended {
            self.armed.clear();
        }
        let log = self.log.get_mut().unwrap_or_else(PoisonError::into_inner);
        step.replies.extend(log.try_iter());
        step.postponed = self.core.postponed.len();
    }
}

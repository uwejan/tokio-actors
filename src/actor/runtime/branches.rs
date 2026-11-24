//! Readiness of the loop's upper branches, the mailbox fast path and the status reply.

use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use super::SupervisionEvent;
use crate::actor::context::ActorContext;
use crate::actor::incarnation::{LaneRx, SystemPlane};
use crate::actor::mailbox::MailRx;
use crate::actor::panic::send_contained;
use crate::actor::supervision::model::FAR_FUTURE;
use crate::actor::{Actor, ActorEnvelope};
use crate::types::ActorStatusInfo;

/// How many turns the run loop may run, once a reply is known to stand alone,
/// before it hands the thread back once.
pub(super) const REPLY_DRAIN_TURNS: u32 = 3;

/// How many turns after a status reply the run loop does not take the turn
/// boundary fast path: for these turns it runs the select, which reads every
/// branch in order, and then returns to the fast path. Only a reply to a
/// requester outside every runtime task opens the window (a thread of its
/// own, or a body driven by `block_on`, a `#[tokio::main]` or a
/// `#[tokio::test]`), and a reply inside the window restarts the count. Such
/// a client reaches the actor while it drains a backlog; the window makes the
/// actor read the system channel and every other branch in order for these
/// turns instead of taking the mailbox directly. A reply to a requester that
/// is a runtime task opens no window: its reply is marked for the hand-back
/// instead (see `ReplyDrain`). Each request carries its requester's kind, so
/// the kind a reply reads is that of the request it answers.
pub(super) const STATUS_FAST_PATH_OFF_TURNS: u32 = 32;

/// Answers one status request and records what the reply owes the loop: the
/// fast path window follows a reply to a requester outside every runtime
/// task, and the hand-back follows a delivered reply to a runtime task, or to
/// any requester on a current-thread runtime. A requester that is gone is
/// owed no hand-back.
pub(super) fn answer_status(
    request: crate::types::StatusRequest,
    info: ActorStatusInfo,
    same_thread: bool,
    fp_skip: &mut u32,
    drain: &mut ReplyDrain,
) {
    let crate::types::StatusRequest { reply, task } = request;
    let delivered = send_contained(reply, Box::new(info)).is_ok();
    if !task {
        *fp_skip = STATUS_FAST_PATH_OFF_TURNS;
    }
    if delivered && (task || same_thread) {
        drain.replied();
    }
}

/// The longest a run of fast-path turns holds the worker before the loop
/// gives it up (a requeue, or the armed deferral on one worker), so a worker
/// shared with hot actors reaches its periodic driver poll (timers, I/O)
/// within the runtime's maintenance interval of such turns even when
/// handlers are slow. A run is measured from its first take and starts over
/// whenever the worker was given up.
pub(super) const TURN_QUANTUM: std::time::Duration = std::time::Duration::from_micros(100);

/// Waits like `hand_back` but through tokio's deferral (`yield_now`): the
/// worker runs the rest of its queue, and parks and polls its drivers when
/// it has nothing else, before the task runs again. The wake slot is armed,
/// so a stop request or a status query that lands meanwhile wakes it at once.
pub(super) async fn defer_armed(
    slot: &crate::actor::incarnation::WakeSlot,
    mut idle: impl FnMut() -> bool,
) {
    let mut yielded = std::pin::pin!(tokio::task::yield_now());
    std::future::poll_fn(|cx| {
        slot.register(cx.waker());
        if !idle() {
            return Poll::Ready(());
        }
        yielded.as_mut().poll(cx)
    })
    .await;
    slot.disarm();
}

/// Ends a turn with the task queued again at the back of its worker's run
/// queue: the task wakes itself and returns pending once, so the tasks
/// already runnable on the worker, the senders the takes woke among them,
/// run before it, and the worker does not wait for an idle moment or poll
/// its drivers first, as a `tokio::task::yield_now` would.
pub(super) async fn requeue() {
    let mut queued = false;
    std::future::poll_fn(|cx| {
        if queued {
            return Poll::Ready(());
        }
        queued = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await;
}

/// Polls a turn's future and records whether it returned pending, that is,
/// whether the task gave its worker back while the turn ran.
pub(super) async fn watch_yield<F: Future>(
    start: impl FnOnce() -> F,
    yielded: &mut bool,
) -> F::Output {
    let mut turn = std::pin::pin!(start());
    std::future::poll_fn(|cx| {
        let polled = turn.as_mut().poll(cx);
        if polled.is_pending() {
            *yielded = true;
        }
        polled
    })
    .await
}

/// Starts a turn: whether the fast path is off for it, and counts the turn
/// off the window a status reply opened.
#[inline(always)]
pub(super) fn fp_skip_step(skip: &mut u32) -> bool {
    let off = *skip > 0;
    if off {
        *skip -= 1;
    }
    off
}

/// What the run loop remembers about the requesters it answered.
///
/// Which requesters a status reply marks: a requester that is a runtime task
/// is marked on every runtime: on the actor's own runtime the reply queues it
/// on the actor's worker, and a task of another runtime costs at most one
/// needless yield per lone reply; a requester that is not a task (a body
/// driven by `block_on`, a `#[tokio::main]` or `#[tokio::test]` body) is
/// marked only on a current-thread runtime, where the body runs on the
/// actor's own thread and waits for the poll to end. On a multi-thread runtime such a requester is
/// woken from another thread and does not wait for this poll, so its reply is
/// not marked. A reply to a call is always marked. A reply whose requester is
/// gone is marked for nobody: nothing waits for it.
///
/// A requester woken by a reply sent from this task is queued on this
/// worker, and runs only when this task's poll ends; with a backlog that is
/// the task's whole cooperative budget, so a status query or a call would
/// wait behind every message the poll still drains. When a reply stands
/// alone, with no reply on the turn after it, the loop therefore runs
/// `REPLY_DRAIN_TURNS` more turns and then hands the thread back once at a
/// turn boundary, if a lane still holds messages. Replies on consecutive
/// turns come from many requesters, each woken in turn and queued ahead of
/// the last, so no one is left behind the whole backlog and the loop keeps
/// draining. A loop with nothing queued at a boundary is about to wait
/// anyway, so the state is dropped there. Nothing here moves a signal or
/// changes which branch is read first: the hand-back is a suspension
/// between two turns, after which the loop reads every branch in order.
///
/// One word, zero while no reply is pending, so the turns that answer
/// nobody read it and change nothing: the turns left before the hand-back
/// while one is due, otherwise `PENDING` (a reply was seen and the turn
/// after it is awaited) with `REPLIED` set when the turn that just ran
/// replied.
pub(super) struct ReplyDrain(pub(super) u32);

impl ReplyDrain {
    /// The turn that just ran sent a reply.
    pub(super) const REPLIED: u32 = 1 << 31;
    /// A reply was seen; the turn after it decides whether it stood alone.
    const PENDING: u32 = 1 << 30;
    /// The turns left before the hand-back.
    const LEFT: u32 = Self::PENDING - 1;

    pub(super) const fn new() -> Self {
        Self(0)
    }

    /// A reply was sent.
    pub(super) fn replied(&mut self) {
        self.0 |= Self::REPLIED;
    }

    /// Advances the state at a turn boundary with a backlog; whether the
    /// loop should hand the thread back now.
    pub(super) fn step(&mut self) -> bool {
        let s = self.0;
        if s & Self::LEFT != 0 {
            self.0 = (s & Self::LEFT) - 1;
            self.0 == 0
        } else if s & Self::REPLIED != 0 {
            self.0 = Self::PENDING;
            false
        } else {
            self.0 = REPLY_DRAIN_TURNS;
            false
        }
    }
}

/// Hands the thread back for one scheduler round, the way a poll whose
/// cooperative budget ran out does: the task is queued again behind the
/// tasks already runnable on this worker, and a requester just woken runs
/// first. While handed back the loop waits like a loop in its select: the
/// wake slot is armed, so a stop request or a status query that lands now
/// wakes it at once instead of waiting for the worker's next idle moment,
/// and `idle` is read after the arming, so one that landed before it ends
/// the hand-back immediately.
pub(super) async fn hand_back(
    slot: &crate::actor::incarnation::WakeSlot,
    mut idle: impl FnMut() -> bool,
) {
    let mut yielded = std::pin::pin!(requeue());
    std::future::poll_fn(|cx| {
        slot.register(cx.waker());
        if !idle() {
            return Poll::Ready(());
        }
        yielded.as_mut().poll(cx)
    })
    .await;
    slot.disarm();
}

/// The run loop's select branches above the mailbox, one variant per branch.
/// The fast path reads each one's clause in `branch_idle`, whose match is
/// exhaustive, and every supervision event maps to one of them
/// (`SupervisionEvent::branch`), so a new plane or branch does not compile
/// until its clause is decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum UpperBranch {
    Stop,
    System,
    Restart,
    Death,
    Verdict,
    Forwarder,
    Priority,
}

impl UpperBranch {
    /// The select's branch order, highest first; the mailbox follows.
    pub(super) const ORDER: [UpperBranch; 7] = [
        UpperBranch::Stop,
        UpperBranch::System,
        UpperBranch::Restart,
        UpperBranch::Death,
        UpperBranch::Verdict,
        UpperBranch::Forwarder,
        UpperBranch::Priority,
    ];
}

impl SupervisionEvent {
    /// The select branch this event arrives on.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn branch(&self) -> UpperBranch {
        match self {
            SupervisionEvent::Restart(_) => UpperBranch::Restart,
            SupervisionEvent::Death(_) => UpperBranch::Death,
            SupervisionEvent::Verdict => UpperBranch::Verdict,
            SupervisionEvent::Forwarder(_) => UpperBranch::Forwarder,
        }
    }
}

/// The branches that end a reply's hand-back: a stop request and a system
/// message must not wait behind it. The other planes hold standing state
/// (a supervisor's running children, its timers, a priority backlog), which
/// reads busy while it is merely non-empty, so they do not decide whether the
/// requester a reply woke gets its turn.
pub(super) const HAND_BACK_EXIT: [UpperBranch; 2] = [UpperBranch::Stop, UpperBranch::System];

/// Whether none of the [`HAND_BACK_EXIT`] branches has anything for the run
/// loop now.
pub(super) fn hand_back_idle<A: Actor>(
    stop_rx: &LaneRx,
    system_rx: &SystemPlane,
    ctx: &mut ActorContext<A>,
    priority: &Option<mpsc::Receiver<ActorEnvelope<A>>>,
) -> bool {
    HAND_BACK_EXIT
        .into_iter()
        .all(|branch| branch_idle(branch, stop_rx, system_rx, ctx, priority))
}

/// Whether every select branch above the mailbox reads idle now, read in the
/// select's branch order.
pub(super) fn upper_idle<A: Actor>(
    stop_rx: &LaneRx,
    system_rx: &SystemPlane,
    ctx: &mut ActorContext<A>,
    priority: &Option<mpsc::Receiver<ActorEnvelope<A>>>,
) -> bool {
    UpperBranch::ORDER
        .into_iter()
        .all(|branch| branch_idle(branch, stop_rx, system_rx, ctx, priority))
}

/// One branch's clause: idle exactly when the select's poll of that branch
/// would pend.
fn branch_idle<A: Actor>(
    branch: UpperBranch,
    stop_rx: &LaneRx,
    system_rx: &SystemPlane,
    ctx: &mut ActorContext<A>,
    priority: &Option<mpsc::Receiver<ActorEnvelope<A>>>,
) -> bool {
    match branch {
        // No unseen version.
        UpperBranch::Stop => matches!(stop_rx.has_changed(), Ok(false)),
        // Not opened, or opened, taken by the run loop, empty and open. A
        // plane opened and not yet taken reads busy, so the select's branch
        // takes its receiver and answers.
        UpperBranch::System => system_rx.is_idle(),
        UpperBranch::Restart => {
            let (planes, _) = ctx.supervision_planes_mut();
            planes.is_none_or(|(restart, _, _)| restart.is_empty())
        }
        UpperBranch::Death => {
            let (planes, _) = ctx.supervision_planes_mut();
            planes.is_none_or(|(_, death, _)| death.is_empty())
        }
        // A supervisor's branch polls its verdict signal whenever any plane
        // holds a task, the forwarder plane included, so it reads idle only
        // with every plane empty.
        UpperBranch::Verdict => {
            let (planes, forwarders) = ctx.supervision_planes_mut();
            planes.is_none_or(|(restart, death, _)| {
                restart.is_empty() && death.is_empty() && forwarders.is_none_or(|f| f.is_empty())
            })
        }
        // A supervisor reads its forwarder plane by emptiness, as the
        // verdict clause does; any other actor by the forwarder end count,
        // without the plane's lock.
        UpperBranch::Forwarder => {
            if ctx.supervision_ref().is_some() {
                let (_, forwarders) = ctx.supervision_planes_mut();
                forwarders.is_none_or(|f| f.is_empty())
            } else {
                ctx.forwarders_idle()
            }
        }
        UpperBranch::Priority => priority
            .as_ref()
            .is_none_or(|lane| lane.is_empty() && !lane.is_closed()),
    }
}

/// The stop lane's next unseen version, woken through the run loop's
/// single-reader slot: armed before the second read, so a raise that lands
/// between the reads is seen by it, and one that lands after wakes the loop.
/// The slot is disarmed when this future drops (the select dropping its
/// branches), so no raise wakes the loop during a callback. The version is
/// marked seen by the branch's own `borrow_and_update`.
pub(super) fn lane_changed<'a>(
    rx: &'a LaneRx,
    slot: &'a crate::actor::incarnation::WakeSlot,
) -> LaneChanged<'a> {
    LaneChanged { rx, slot }
}

pub(super) struct LaneChanged<'a> {
    rx: &'a LaneRx,
    slot: &'a crate::actor::incarnation::WakeSlot,
}

impl LaneChanged<'_> {
    fn unseen(&self) -> bool {
        matches!(self.rx.has_changed(), Ok(true))
    }
}

impl Future for LaneChanged<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        if self.unseen() {
            return Poll::Ready(());
        }
        self.slot.register(cx.waker());
        if self.unseen() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for LaneChanged<'_> {
    fn drop(&mut self) {
        self.slot.disarm();
    }
}

/// The mailbox's next message if one is ready now, through the task's own
/// context (so the cooperative budget applies); `None` when it is empty,
/// closed or not ready, which the select then decides. Never waits.
pub(super) fn poll_now<T>(rx: &mut MailRx<T>) -> impl Future<Output = Option<T>> + '_ {
    std::future::poll_fn(move |cx| match rx.poll_recv(cx) {
        Poll::Ready(Some(value)) => Poll::Ready(Some(value)),
        Poll::Ready(None) | Poll::Pending => Poll::Ready(None),
    })
}

/// Awaits `rx`'s next value when it is `Some`, or pends forever when it is
/// `None`. Used as a `select!` branch for the priority lane: paired with an
/// `if priority.is_some()` precondition on that branch (so a disabled lane
/// is never even polled), the `pending()` arm here is a second line of
/// defense against the branch ever turning into a busy poll.
pub(super) async fn recv_optional<T>(rx: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

// ---------------------------------------------------------------------------
// Helper utilities
// ---------------------------------------------------------------------------

/// Adds `dur` to `base`, capping the addition at [`FAR_FUTURE`] instead of
/// letting it approach the edge of the representable `Instant` range.
///
/// A plain `base.checked_add(dur)` is not enough on its own: even when it
/// succeeds, the timer that later consumes this deadline rounds it up to
/// the next millisecond with its own unchecked addition, which panics if
/// the deadline this function returned was already within the last
/// millisecond of what `Instant` can represent. Capping `dur` at
/// `FAR_FUTURE` before adding it keeps every returned deadline at most 30
/// years past `base` - which any caller-facing timer treats as never
/// firing - so it is always safely convertible by the timer, on every
/// input, not just the ones a plain `checked_add` already rejects.
///
/// Used wherever a deadline `Instant` is computed from `Instant::now()` plus
/// a caller-controlled duration: `send_timeout`, system shutdown deadlines,
/// and timer scheduling. A caller-controlled [`Duration`] handed directly to
/// `tokio::time::sleep`/`timeout` instead - never turned into a deadline
/// `Instant` here - goes through [`bounded_wait`] instead, except the bound of
/// a `sys` call, which `Sys::timeout` gives to tokio's `timeout` as set.
pub(crate) fn saturating_deadline(base: Instant, dur: Duration) -> Instant {
    base.checked_add(dur.min(FAR_FUTURE)).unwrap_or(base)
}

/// Caps `dur` at [`FAR_FUTURE`]: a wait of at most 30 years, which any
/// caller-facing timer treats as never firing, and always safely
/// convertible by `tokio::time::sleep`/`timeout` on every input, not just
/// the ones already within the representable `Instant` range.
///
/// Used wherever a caller-controlled duration is handed directly to
/// `tokio::time::sleep`/`timeout`, or kept as a budget, instead of first being
/// turned into a deadline `Instant` via [`saturating_deadline`]:
/// `wait_terminal`, the supervisor teardown's wait for a child's end
/// (`wait_end_or_floor`), the bound of a lost runner's verdict and the bound
/// of a wait on an actor's subtree. A spawn's or a restart's `start_timeout`
/// and the window of a system shutdown are turned into deadlines with
/// [`saturating_deadline`].
pub(crate) fn bounded_wait(dur: Duration) -> Duration {
    dur.min(FAR_FUTURE)
}

pub(crate) fn build_status_info<A: Actor>(
    ctx: &ActorContext<A>,
    suspended: bool,
    state: Option<String>,
) -> ActorStatusInfo {
    let (child_count, name) = ctx
        .supervision_ref()
        .map(|sup| (sup.registry.len(), ctx.actor_name().map(str::to_string)))
        .unwrap_or((0, ctx.actor_name().map(str::to_string)));
    let handle = ctx.self_handle_ref();

    ActorStatusInfo {
        id: ctx.actor_id().clone(),
        name,
        status: ctx.status(),
        mailbox_len: handle.mailbox_len(),
        mailbox_capacity: handle.mailbox_capacity(),
        priority_len: handle.priority_len(),
        priority_capacity: handle.priority_capacity(),
        child_count,
        timer_count: ctx.active_timer_count(),
        stream_count: ctx.active_stream_count(),
        incarnation: ctx.stop_lane().abort_record().incarnation(),
        monitors: ctx.monitor_count(),
        monitored_by: ctx.stop_lane().abort_record().monitored_by(),
        trap_exit: ctx.stop_lane().abort_record().trap(),
        links: ctx.link_count(),
        pg_groups: ctx.stop_lane().abort_record().pg_joined(),
        group_monitors: ctx.group_monitor_count(),
        aliases: ctx.alias_count(),
        requests_in_flight: ctx.request_count(),
        suspended,
        state,
    }
}

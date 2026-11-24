use super::*;
use crate::actor::incarnation::StopLane;
use crate::actor::supervision::{SupervisionConfig, SupervisionState};

#[derive(Default)]
struct Dummy;

impl Actor for Dummy {
    type Message = ();
    type Response = ();

    async fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        Ok(())
    }
}

fn dummy_ctx() -> ActorContext<Dummy> {
    dummy_ctx_with_lane().0
}

/// A context and the receiving end of its stop lane.
fn dummy_ctx_with_lane() -> (ActorContext<Dummy>, crate::actor::incarnation::LaneRx) {
    let (tx, _rx) = crate::actor::mailbox::test_channel(1);
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let (stop_lane, stop_rx) = StopLane::new();
    let id = ActorId::from("dummy");
    let handle = ActorHandle::new(
        id.clone(),
        tx,
        stop_lane,
        1,
        status_rx,
        None,
        Some(Arc::new(std::sync::atomic::AtomicBool::new(false))),
    );
    let ctx = ActorContext::new(
        id,
        handle,
        Handle::current(),
        status_tx,
        None,
        None,
        Some(SupervisionState::new(SupervisionConfig::default())),
        TurnClock::default(),
        SubtreeHold::new(None),
    );
    (ctx, stop_rx)
}

/// `stop` refuses a `Kill` and writes nothing, reports a lane that takes
/// no request as `Closed`, and marks the turn as a self-stop only when the
/// lane took the request.
#[tokio::test]
async fn stop_refuses_kill_and_marks_the_turn_only_when_the_lane_took_it() {
    let (mut ctx, rx) = dummy_ctx_with_lane();
    assert_eq!(ctx.stop(StopReason::Kill), Err(StopError::KillRefused));
    assert!(
        rx.borrow().reason.is_none(),
        "a refused Kill writes nothing"
    );
    assert!(!ctx.take_self_stop());
    assert_eq!(ctx.stop(StopReason::Graceful), Ok(()));
    assert_eq!(rx.borrow().reason, Some(StopReason::Graceful));
    assert!(ctx.take_self_stop());
    assert!(!ctx.take_self_stop(), "the flag is read once");
    ctx.clear_self_stop();
    drop(rx);
    assert_eq!(ctx.stop(StopReason::Graceful), Err(StopError::Closed));
    assert!(!ctx.take_self_stop(), "a refused stop marks nothing");
    // A self-stop, an idle hook and a continuation leave nothing for the
    // end's signal checks; a signal cell does.
    assert!(ctx.monitors.is_none(), "a self-stop makes no signal state");
    ctx.continue_with(());
    assert!(!ctx.has_end_state(), "the turn state is no signal state");
    ctx.stop_lane().abort_record().set_exit_cap(1);
    assert!(ctx.has_end_state(), "a signal cell is checked at the end");
}

/// A context dropped outside its task's own poll writes `Kill`, even
/// during an unwind of another code path; a written reason stays.
async fn the_context_drop_writes_the_reason_of_how_its_task_ended() {
    let ctx = dummy_ctx();
    let plane = ctx.self_handle().status_rx();
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _held = ctx;
        panic!("an unwind that is not the task's own poll");
    }));
    assert!(unwound.is_err());
    assert_eq!(plane.borrow().reason, Some(StopReason::Kill));

    let ctx = dummy_ctx();
    let plane = ctx.self_handle().status_rx();
    drop(ctx);
    assert_eq!(plane.borrow().reason, Some(StopReason::Kill));

    let ctx = dummy_ctx();
    let plane = ctx.self_handle().status_rx();
    StatusValue::write_reason(&ctx.status_tx, StopReason::Graceful, false);
    drop(ctx);
    assert_eq!(plane.borrow().reason, Some(StopReason::Graceful));
}

/// What a [`Timed`] actor is asked.
enum TimedMsg {
    /// Schedules a one-shot timer due at once, waits until its forwarder
    /// has ended (it put its message in the mailbox), and cancels it,
    /// all in this one turn: the loop cannot reap the forwarder first.
    CancelAfterTheFiring,
    /// Fills the mailbox, schedules a one-shot timer due at once, lets
    /// its forwarder reach the full mailbox, and cancels it.
    CancelWhileTheSendWaits,
    /// Fills the one free slot of the mailbox.
    Filler,
    /// The timer's message.
    Fired,
    /// Answers once every message queued before it was handled.
    Ping,
}

struct Timed {
    fired: Arc<std::sync::atomic::AtomicUsize>,
    answers: Arc<std::sync::Mutex<Vec<bool>>>,
}

impl Actor for Timed {
    type Message = TimedMsg;
    type Response = ();

    async fn handle(
        &mut self,
        msg: TimedMsg,
        ctx: &mut ActorContext<Self>,
    ) -> crate::error::ActorResult<()> {
        match msg {
            TimedMsg::CancelAfterTheFiring => {
                let id = ctx.schedule(TimedMsg::Fired).after(Duration::ZERO).await?;
                let joined = ctx
                    .ex_mut()
                    .forwarders
                    .join_next_with_id()
                    .await
                    .expect("the timer's forwarder ends");
                let answer = ctx.cancel_timer(id);
                ctx.reap_forwarder(joined);
                self.answers.lock().unwrap().push(answer);
            }
            TimedMsg::CancelWhileTheSendWaits => {
                if ctx.self_handle().try_notify(TimedMsg::Filler).is_err() {
                    panic!("the mailbox has one free slot");
                }
                let id = ctx.schedule(TimedMsg::Fired).after(Duration::ZERO).await?;
                // The forwarder runs while this turn yields: its deadline
                // has passed and the mailbox is full.
                for _ in 0..16 {
                    tokio::task::yield_now().await;
                }
                let answer = ctx.cancel_timer(id);
                self.answers.lock().unwrap().push(answer);
            }
            TimedMsg::Fired => {
                self.fired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            TimedMsg::Filler | TimedMsg::Ping => {}
        }
        Ok(())
    }
}

/// Runs `msg` on a fresh [`Timed`] actor whose mailbox holds one
/// message, then a `Ping` behind everything the turn queued; returns
/// the cancel's answer and how many times the timer's message arrived.
async fn cancel_in_a_turn(msg: TimedMsg) -> (Vec<bool>, usize) {
    use crate::actor::ActorExt;
    let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let answers = Arc::new(std::sync::Mutex::new(Vec::new()));
    let actor = Timed {
        fired: fired.clone(),
        answers: answers.clone(),
    }
    .spawn()
    .with_config(runtime::ActorConfig::default().with_mailbox_capacity(1))
    .await
    .expect("the actor starts");
    time::timeout(Duration::from_secs(10), actor.send(msg))
        .await
        .expect("the turn ends")
        .expect("the turn succeeds");
    time::timeout(Duration::from_secs(10), actor.send(TimedMsg::Ping))
        .await
        .expect("the ping is answered")
        .expect("the ping succeeds");
    let answers = answers.lock().unwrap().clone();
    let fired = fired.load(std::sync::atomic::Ordering::SeqCst);
    let _ = actor.stop(StopReason::Graceful).await;
    (answers, fired)
}

/// A one-shot timer whose forwarder put its message in the mailbox is
/// not cancelled, even before the loop reaps the forwarder: the cancel
/// reads `false`, and the message arrives once.
async fn a_cancel_after_the_firing_reads_not_found() {
    let (answers, fired) = cancel_in_a_turn(TimedMsg::CancelAfterTheFiring).await;
    assert!(answers.as_slice() == [false], "{answers:?}");
    assert_eq!(fired, 1, "the message arrives once");
}

/// A one-shot timer whose send waits on a full mailbox is cancelled: the
/// cancel reads `true`, and the message never arrives.
async fn a_cancel_while_the_send_waits_reads_ok() {
    let (answers, fired) = cancel_in_a_turn(TimedMsg::CancelWhileTheSendWaits).await;
    assert_eq!(answers, [true]);
    assert_eq!(fired, 0, "the message never arrives");
}

/// The text of the panic a dropped factory's capture raises.
const CAPTURE_PANIC: &str = "the factory's capture panics when dropped";
/// The text of the panic a dropped built value raises.
const VALUE_PANIC: &str = "the built value panics when dropped";

/// Panics with its text when dropped outside an unwind, and counts the
/// panics it raised.
struct PanicOnDrop(&'static str, Arc<std::sync::atomic::AtomicUsize>);

impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("{}", self.0);
        }
    }
}

type TestFactory<T> = Box<dyn Fn() -> T + Send + Sync>;

/// The spec's strong reference to a factory, which the factory releases
/// during its own call, as a teardown that removes the spec while the
/// attempt builds does.
type SpecHold<T> = Arc<std::sync::Mutex<Option<Arc<TestFactory<T>>>>>;

/// A factory whose capture panics when dropped and whose spec is removed
/// during its call, before `build` runs; returns the spec's hold and the
/// attempt's weak reference.
fn removed_during_build<T: 'static>(
    build: impl Fn() -> T + Send + Sync + 'static,
    panics: &Arc<std::sync::atomic::AtomicUsize>,
) -> (SpecHold<T>, std::sync::Weak<TestFactory<T>>) {
    removed_during_build_with(build, Some(PanicOnDrop(CAPTURE_PANIC, panics.clone())))
}

/// As [`removed_during_build`], with a capture whose drop panics only
/// when one is given.
fn removed_during_build_with<T: 'static>(
    build: impl Fn() -> T + Send + Sync + 'static,
    capture: Option<PanicOnDrop>,
) -> (SpecHold<T>, std::sync::Weak<TestFactory<T>>) {
    let hold: SpecHold<T> = Arc::default();
    let inner = hold.clone();
    let factory: TestFactory<T> = Box::new(move || {
        let _ = &capture;
        let spec = inner.lock().unwrap().take();
        drop(spec);
        build()
    });
    let factory = Arc::new(factory);
    let weak = Arc::downgrade(&factory);
    *hold.lock().unwrap() = Some(factory);
    (hold, weak)
}

/// Counts its drops.
struct Counted(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The outcome a cell reads once its attempt's executor is gone.
async fn cell_after(ctl: &crate::actor::supervision::SlotCtl, io: SlotIo) -> CellOutcome {
    drop(io);
    let mut watch = ctl.watch();
    time::timeout(Duration::from_secs(10), await_cell(&mut watch, 2))
        .await
        .expect("the cell settles")
}

/// A spec removed during the build leaves the attempt the factory's last
/// reference: the attempt starts nothing even when no drop panics. The
/// value it built drops once, in the attempt, and the cancel is recorded
/// in the cell.
async fn a_spec_removed_during_the_build_cancels_the_attempt_without_a_panic() {
    let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = drops.clone();
    let (hold, weak) = removed_during_build_with(move || Counted(counted.clone()), None);
    let (ctl, io) = crate::actor::supervision::slot_pair(Default::default());
    let built = build_for_attempt(&weak, &io, &ActorId::from("sup"), &ActorId::from("c"), 2)
        .map_err(|outcome| *outcome);
    assert!(matches!(
        built,
        Err(RestartOutcome::Cancelled {
            incarnation: 2,
            drop_panic: None,
            ..
        })
    ));
    assert!(hold.lock().unwrap().is_none());
    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(matches!(cell_after(&ctl, io).await, CellOutcome::Cancelled));
}

/// A build that panics after the spec went cancels the attempt too: the
/// panic is logged, not charged as a factory failure.
async fn a_build_panic_after_the_spec_went_cancels_the_attempt() {
    #[cfg(feature = "tracing")]
    let logged = crate::actor::supervision::lost_runner::capture::install();
    let child = format!("build-panic-{}", uuid::Uuid::new_v4());
    let supervisor = format!("{child}-supervisor");
    let (_hold, weak) = removed_during_build_with(|| -> u32 { panic!("the build panics") }, None);
    let (ctl, io) = crate::actor::supervision::slot_pair(Default::default());
    let built = build_for_attempt(
        &weak,
        &io,
        &ActorId::from(supervisor.clone()),
        &ActorId::from(child.clone()),
        2,
    )
    .map_err(|outcome| *outcome);
    assert!(matches!(
        built,
        Err(RestartOutcome::Cancelled {
            drop_panic: None,
            ..
        })
    ));
    assert!(matches!(cell_after(&ctl, io).await, CellOutcome::Cancelled));
    // The build's panic is logged once, naming the supervisor and the
    // child.
    #[cfg(feature = "tracing")]
    {
        let (by_supervisor, by_child) = (
            format!("supervisor={supervisor} "),
            format!("child={child} "),
        );
        let lines: Vec<String> = logged
            .lock()
            .unwrap()
            .iter()
            .filter(|line| {
                line.contains(&by_supervisor)
                    && line.contains(&by_child)
                    && line.contains("the build panics")
            })
            .cloned()
            .collect();
        assert_eq!(lines.len(), 1, "one line: {lines:?}");
    }
    #[cfg(not(feature = "tracing"))]
    let _ = (child, supervisor);
}

/// The bound of a spawn with `await_init` passes at the moment the child
/// answers: an acknowledgment read at the expiry wins, and so does one that
/// lands after that read and before the close, and the child is not cut; an
/// init error read at the expiry is the start's failure and not a timeout;
/// an init that ended without a reply is reported as its own end and not
/// as a timeout, as it is when the end is read before the bound; with
/// nothing to read, the close fails the child's send and the cut runs.
#[tokio::test]
async fn an_ack_that_lands_as_the_bound_expires_is_not_cut() {
    #[derive(Clone, Copy)]
    enum Landing {
        Acknowledged,
        InitError,
        SenderDropped,
        BetweenReadAndClose,
        Nothing,
    }
    for landing in [
        Landing::Acknowledged,
        Landing::InitError,
        Landing::SenderDropped,
        Landing::BetweenReadAndClose,
        Landing::Nothing,
    ] {
        let (tx, mut rx) = oneshot::channel::<Result<(), ActorError>>();
        let tx = std::cell::RefCell::new(Some(tx));
        let cut = std::cell::Cell::new(false);
        if matches!(landing, Landing::BetweenReadAndClose) {
            let sender = tx.borrow_mut().take();
            BEFORE_CLOSE.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    if let Some(sender) = sender {
                        let _ = sender.send(Ok(()));
                    }
                }));
            });
        }
        let end = bounded_ack(
            &mut rx,
            async {
                match landing {
                    Landing::Acknowledged => {
                        if let Some(tx) = tx.borrow_mut().take() {
                            let _ = tx.send(Ok(()));
                        }
                    }
                    Landing::InitError => {
                        if let Some(tx) = tx.borrow_mut().take() {
                            let _ = tx.send(Err(ActorError::user("init")));
                        }
                    }
                    Landing::SenderDropped => drop(tx.borrow_mut().take()),
                    Landing::BetweenReadAndClose | Landing::Nothing => {}
                }
            },
            || cut.set(true),
        )
        .await;
        match landing {
            Landing::Acknowledged => {
                assert!(matches!(end, InitEnd::Started), "acknowledged: started");
                assert!(!cut.get(), "an acknowledged child is not cut");
            }
            Landing::InitError => {
                assert!(
                    matches!(end, InitEnd::Failed(_)),
                    "an init error read at the expiry is the start's failure"
                );
                assert!(!cut.get(), "a child that answered is not cut");
            }
            Landing::SenderDropped => {
                assert!(
                    matches!(end, InitEnd::Closed),
                    "a sender dropped without a reply: the init's own end"
                );
                assert!(!cut.get(), "an init that ended is not cut");
            }
            Landing::BetweenReadAndClose => {
                assert!(
                    matches!(end, InitEnd::Started),
                    "acknowledged after the first read and before the close: started"
                );
                assert!(!cut.get(), "that child is not cut");
            }
            Landing::Nothing => {
                assert!(matches!(end, InitEnd::TimedOut), "no ack: timed out");
                assert!(cut.get(), "the cut ran");
                let tx = tx.borrow_mut().take().expect("the sender is still here");
                assert!(
                    tx.send(Ok(())).is_err(),
                    "the child's acknowledgment cannot be delivered after the close"
                );
            }
        }
    }
}

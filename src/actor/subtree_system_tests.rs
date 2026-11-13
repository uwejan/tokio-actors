//! The system plane in isolation: the order it answers requests in, how many
//! it queues, and what becomes of the requests and the reserved slots when
//! the run loop ends it. Nothing here is ordered by a sleep.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use tokio::sync::oneshot;

use super::*;
use crate::types::{ActorId, ActorStatusInfo, StatusRequest, SystemMessage};

const BOUND: Duration = Duration::from_secs(5);

fn new_plane() -> (AbortRecord, SystemPlane) {
    let record = AbortRecord::new();
    let system = crate::system::ActorSystem::create(format!("plane-{}", uuid::Uuid::new_v4()))
        .expect("a fresh system");
    let plane = SystemPlane::new(
        record.clone(),
        ActorId::from("system-plane".to_string()),
        system,
    );
    (record, plane)
}

/// Polls `submit_status` once: whether the request was queued without
/// waiting. A request that has to wait is dropped, so it queues nothing.
fn submit_now(record: &AbortRecord, reply: oneshot::Sender<Box<ActorStatusInfo>>) -> bool {
    let mut cx = Context::from_waker(Waker::noop());
    let mut submit = std::pin::pin!(record.submit_status(reply));
    matches!(submit.as_mut().poll(&mut cx), Poll::Ready(Ok(())))
}

/// Takes the next request now, when one is ready.
fn take_request_now(plane: &mut SystemPlane) -> Option<StatusRequest> {
    let mut cx = Context::from_waker(Waker::noop());
    match plane.poll_recv(&mut cx) {
        Poll::Ready(Some(SystemMessage::GetStatus(request))) => Some(request),
        Poll::Ready(Some(SystemMessage::Signals))
        | Poll::Ready(Some(SystemMessage::Supervise(_)))
        | Poll::Ready(Some(SystemMessage::Sys(_)))
        | Poll::Ready(None)
        | Poll::Pending => None,
    }
}

/// Takes the next request's reply now, when one is ready.
fn take_now(plane: &mut SystemPlane) -> Option<oneshot::Sender<Box<ActorStatusInfo>>> {
    take_request_now(plane).map(|request| request.reply)
}

/// A request whose requester is gone reads closed, so a query can be told
/// from the next by whether its requester was dropped.
fn marked() -> (
    oneshot::Sender<Box<ActorStatusInfo>>,
    oneshot::Receiver<Box<ActorStatusInfo>>,
) {
    oneshot::channel()
}

/// A `sys` command submitted while a status request is held opens the
/// channel and leaves the held request first, as a second status request
/// does.
#[test]
fn a_command_behind_a_held_status_request_leaves_the_request_first() {
    let (record, mut plane) = new_plane();
    let (status, _requester) = marked();
    assert!(submit_now(&record, status), "the status request is held");
    let (reply, _caller) = oneshot::channel();
    let command = crate::types::SysCommand::Probe(Default::default(), reply);
    let mut cx = Context::from_waker(Waker::noop());
    let mut submit = std::pin::pin!(record.submit_sys(command));
    assert!(matches!(submit.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
    assert!(take_request_now(&mut plane).is_some(), "the held request");
    assert!(matches!(
        plane.poll_recv(&mut cx),
        Poll::Ready(Some(SystemMessage::Sys(_)))
    ));
}

/// The plane queues 64 requests in steady state, as the system channel of an
/// actor without a held slot does. Before the run loop has taken the request
/// held ahead of the channel, that request is queued beside the 64.
#[test]
fn the_plane_queues_sixty_four_in_steady_state() {
    let (record, mut plane) = new_plane();
    let mut cold = 0;
    loop {
        let (reply, _requester) = marked();
        if !submit_now(&record, reply) {
            break;
        }
        cold += 1;
    }
    let mut served = 0;
    while take_now(&mut plane).is_some() {
        served += 1;
    }
    assert_eq!(served, cold, "every queued request is served");

    let mut steady = 0;
    loop {
        let (reply, _requester) = marked();
        if !submit_now(&record, reply) {
            break;
        }
        steady += 1;
    }
    assert_eq!(cold, SYSTEM_CAPACITY + 1, "cold bound");
    assert_eq!(steady, SYSTEM_CAPACITY, "steady-state bound");
    assert_eq!(SYSTEM_CAPACITY, 64);
}

/// Queues one status request without waiting, as a requester inside a runtime
/// task (`task`) or outside every task (the body of `block_on`). The returned
/// receiver keeps the requester alive.
fn submit_as(
    rt: &tokio::runtime::Runtime,
    record: &AbortRecord,
    task: bool,
) -> oneshot::Receiver<Box<ActorStatusInfo>> {
    let (reply, requester) = marked();
    let queued = if task {
        let record = record.clone();
        rt.block_on(async move { tokio::spawn(async move { submit_now(&record, reply) }).await })
            .unwrap()
    } else {
        rt.block_on(async {
            assert!(tokio::task::try_id().is_none(), "the body is not a task");
            submit_now(record, reply)
        })
    };
    assert!(queued, "the plane has room");
    requester
}

fn current_thread_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

/// The kinds of the requests the plane hands out, in the order it hands them.
fn kinds_taken(plane: &mut SystemPlane) -> Vec<bool> {
    let mut kinds = Vec::new();
    while let Some(request) = take_request_now(plane) {
        kinds.push(request.task);
    }
    kinds
}

// ---------------------------------------------------------------------------
// The requesters' wakers
// ---------------------------------------------------------------------------

/// A waker that counts its wakes and panics in them while it is armed.
#[derive(Default)]
struct LoudWake {
    armed: AtomicBool,
    woken: std::sync::atomic::AtomicUsize,
}

impl LoudWake {
    fn arm(&self, armed: bool) {
        self.armed.store(armed, Ordering::SeqCst);
    }

    fn woken(&self) -> usize {
        self.woken.load(Ordering::SeqCst)
    }
}

impl std::task::Wake for LoudWake {
    fn wake(self: Arc<Self>) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        if self.armed.load(Ordering::SeqCst) {
            panic!("a requester's waker panics");
        }
    }
}

/// Queues a status request whose requester has polled for the reply with a
/// waker that panics when it is woken.
fn loud_request(record: &AbortRecord) -> (Arc<LoudWake>, oneshot::Receiver<Box<ActorStatusInfo>>) {
    let loud = Arc::new(LoudWake::default());
    let (reply, mut requester) = marked();
    assert!(submit_now(record, reply), "the plane has room");
    let waker = Waker::from(loud.clone());
    assert!(std::pin::Pin::new(&mut requester)
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    (loud, requester)
}

/// The plane ends with `requests` loud requests, the first held beside the
/// channel and the others queued in it, while the thread it is dropped on is
/// unwinding when `unwinding`. A panic of a requester's waker stays in the
/// drop, whichever place the request is in.
fn end_the_plane_with_loud_requests(requests: usize, unwinding: bool) {
    let (record, plane) = new_plane();
    let loud: Vec<_> = (0..requests).map(|_| loud_request(&record)).collect();
    for (waker, _) in &loud {
        waker.arm(true);
    }
    if unwinding {
        let unwound = std::thread::spawn(move || {
            let _owned = plane;
            panic!("the thread unwinds");
        })
        .join();
        assert!(unwound.is_err(), "the thread ended by its own panic");
    } else {
        let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(plane)));
        assert!(
            dropped.is_ok(),
            "a requester's waker unwound out of the plane's drop"
        );
    }
    for (waker, requester) in loud {
        waker.arm(false);
        assert_eq!(waker.woken(), 1, "every requester was woken once");
        let mut requester = requester;
        assert!(
            std::pin::Pin::new(&mut requester)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready(),
            "the requester reads the end of the plane"
        );
    }
}

/// A waker that counts its wakes and panics in them while it is armed.
#[derive(Default)]
struct CountingWake {
    armed: AtomicBool,
    woken: AtomicUsize,
}

impl Wake for CountingWake {
    fn wake(self: Arc<Self>) {
        self.woken.fetch_add(1, Ordering::SeqCst);
        if self.armed.load(Ordering::SeqCst) {
            panic!("a parked requester's waker panics");
        }
    }
}

type Submit = Pin<Box<dyn Future<Output = Result<(), ()>> + Send>>;

/// Parks a `submit_status` on the full plane, polled once with `wake`.
fn park_submit(record: &AbortRecord, wake: &Arc<CountingWake>) -> Submit {
    let record = record.clone();
    let (reply, _requester) = marked();
    let mut parked: Submit = Box::pin(async move { record.submit_status(reply).await });
    assert!(
        matches!(
            parked
                .as_mut()
                .poll(&mut Context::from_waker(&Waker::from(wake.clone()))),
            Poll::Pending
        ),
        "a status request into a full plane parks"
    );
    parked
}

/// A `sys` command whose caller is gone reads closed; one whose caller is
/// kept reads open.
fn sys_command(keep: bool) -> (crate::types::SysCommand, Option<oneshot::Receiver<()>>) {
    let (reply, caller) = oneshot::channel();
    (
        crate::types::SysCommand::Probe(Default::default(), reply),
        keep.then_some(caller),
    )
}

/// Polls `submit_sys` once: whether the command was queued without waiting.
fn submit_sys_now(record: &AbortRecord) -> bool {
    let (command, _caller) = sys_command(false);
    let mut cx = Context::from_waker(Waker::noop());
    let mut submit = std::pin::pin!(record.submit_sys(command));
    matches!(submit.as_mut().poll(&mut cx), Poll::Ready(Ok(())))
}

/// Parks a `submit_sys` on the full plane, polled once with `wake`.
fn park_submit_sys(
    record: &AbortRecord,
    wake: &Arc<CountingWake>,
) -> (Submit, oneshot::Receiver<()>) {
    let record = record.clone();
    let (command, caller) = sys_command(true);
    let mut parked: Submit = Box::pin(async move { record.submit_sys(command).await });
    assert!(
        matches!(
            parked
                .as_mut()
                .poll(&mut Context::from_waker(&Waker::from(wake.clone()))),
            Poll::Pending
        ),
        "a sys command into a full plane parks"
    );
    (parked, caller.expect("kept"))
}

/// Whether each `sys` command the plane holds has a caller that is gone, in
/// the order the plane hands them out.
fn sys_callers_gone(plane: &mut SystemPlane) -> Vec<bool> {
    let mut cx = Context::from_waker(Waker::noop());
    let mut gone = Vec::new();
    while let Poll::Ready(Some(SystemMessage::Sys(command))) = plane.poll_recv(&mut cx) {
        match *command {
            crate::types::SysCommand::Probe(_, reply) => gone.push(reply.is_closed()),
            _ => unreachable!("only probes are queued"),
        }
    }
    gone
}

/// A `sys` caller parked on the full plane gets the slot the run loop frees;
/// if it is dropped before it takes the slot, the slot goes to the caller
/// parked behind it, whose waker panics. The panic stays in the drop: the
/// caller behind gets its slot, its command lands last, and it is not woken at
/// all when the caller ahead of it held no slot.
#[test]
fn a_cancelled_sys_command_wakes_the_caller_behind_it_inside_a_catch() {
    for (row, granted) in [
        ("dropped holding the slot the run loop freed", true),
        ("dropped while parked, holding nothing", false),
    ] {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime");
        rt.block_on(async {
            let (record, mut plane) = new_plane();
            let mut queued = 0;
            while submit_sys_now(&record) {
                queued += 1;
            }
            assert_eq!(queued, SYSTEM_CAPACITY, "{row}: the plane is full");
            let first_wake = Arc::new(CountingWake::default());
            let behind_wake = Arc::new(CountingWake::default());
            let (first, _first_caller) = park_submit_sys(&record, &first_wake);
            let (mut behind, _behind_caller) = park_submit_sys(&record, &behind_wake);
            behind_wake.armed.store(true, Ordering::SeqCst);
            if granted {
                let mut cx = Context::from_waker(Waker::noop());
                assert!(matches!(
                    plane.poll_recv(&mut cx),
                    Poll::Ready(Some(SystemMessage::Sys(_)))
                ));
                assert_eq!(first_wake.woken.load(Ordering::SeqCst), 1, "{row}");
                assert_eq!(behind_wake.woken.load(Ordering::SeqCst), 0, "{row}");
            }
            let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(first)));
            assert!(
                dropped.is_ok(),
                "{row}: a waker's panic unwound out of the drop of the caller ahead"
            );
            assert_eq!(
                behind_wake.woken.load(Ordering::SeqCst),
                usize::from(granted),
                "{row}: wakes of the caller behind"
            );
            behind_wake.armed.store(false, Ordering::SeqCst);
            if !granted {
                let mut cx = Context::from_waker(Waker::noop());
                assert!(matches!(
                    plane.poll_recv(&mut cx),
                    Poll::Ready(Some(SystemMessage::Sys(_)))
                ));
            }
            let outcome = behind
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()));
            assert!(
                matches!(outcome, Poll::Ready(Ok(()))),
                "{row}: the caller behind gets its slot"
            );
            let gone = sys_callers_gone(&mut plane);
            assert_eq!(gone.len(), SYSTEM_CAPACITY, "{row}: the commands left");
            assert_eq!(
                gone.iter().rposition(|gone| !gone),
                Some(SYSTEM_CAPACITY - 1),
                "{row}: the command of the caller behind lands last"
            );
            assert_eq!(
                gone.iter().filter(|gone| !**gone).count(),
                1,
                "{row}: only that command has a caller"
            );
        });
    }
}

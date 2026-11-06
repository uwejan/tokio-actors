//! Raisers of one call's `Kill` step, some accepted by the stop lane and some
//! refused by it or unwound. A refused or unwound raiser gives back only its
//! own share of the step's count, and the step reads `Killed` from the first
//! accepted raise on, to every reader at every moment, so the call reports
//! the `Kill` the lane accepted.

use std::future::Future;
use std::sync::Arc;

use super::StopStep;
use crate::types::{RaiseResult, StopLane, StopOutcome};

/// The second raiser takes a share while the first is in flight and is
/// refused after the first was accepted; the refusal leaves the `Killed`
/// step alone.
#[test]
fn a_refused_second_raiser_does_not_erase_the_killed_step() {
    let (lane, _rx) = StopLane::new();
    let step = Arc::new(StopStep::accepted());
    let (a_swapped_tx, a_swapped_rx) = std::sync::mpsc::channel::<()>();
    let (b_swapped_tx, b_swapped_rx) = std::sync::mpsc::channel::<()>();
    let (sealed_tx, sealed_rx) = std::sync::mpsc::channel::<()>();
    let a = {
        let (lane, step) = (lane.clone(), step.clone());
        std::thread::spawn(move || {
            step.raise_kill(&lane, || {
                a_swapped_tx.send(()).unwrap();
                b_swapped_rx.recv().unwrap();
            })
        })
    };
    let b = {
        let (lane, step) = (lane.clone(), step.clone());
        std::thread::spawn(move || {
            a_swapped_rx.recv().unwrap();
            step.raise_kill(&lane, || {
                b_swapped_tx.send(()).unwrap();
                sealed_rx.recv().unwrap();
            })
        })
    };
    let first = a.join().unwrap();
    assert_ne!(
        first,
        RaiseResult::Refused,
        "the first raiser's Kill is accepted"
    );
    lane.seal();
    sealed_tx.send(()).unwrap();
    let second = b.join().unwrap();
    assert_eq!(
        second,
        RaiseResult::Refused,
        "the second raiser finds the lane sealed"
    );
    let outcome = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(step.requested());
    assert_eq!(
        outcome,
        StopOutcome::Killed,
        "the lane accepted a Kill for this call, so a Stopped it publishes reads Killed"
    );
}

/// A raiser that is still in flight over the settled step does not leave the
/// step open: a later raiser refused by the sealed lane returns while the
/// first refused one has not finished, and a reader in that moment still
/// reads the accepted `Kill`.
#[test]
fn a_reader_while_a_refused_raiser_is_in_flight_over_a_killed_step_reads_killed() {
    let (lane, _rx) = StopLane::new();
    let step = Arc::new(StopStep::accepted());
    assert_ne!(step.raise_kill(&lane, || {}), RaiseResult::Refused);
    lane.seal();
    let (b_in_flight_tx, b_in_flight_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let b = {
        let (lane, step) = (lane.clone(), step.clone());
        std::thread::spawn(move || {
            step.raise_kill(&lane, || {
                b_in_flight_tx.send(()).unwrap();
                go_rx.recv().unwrap();
            })
        })
    };
    b_in_flight_rx.recv().unwrap();
    assert_eq!(
        step.raise_kill(&lane, || {}),
        RaiseResult::Refused,
        "the later raiser finds the lane sealed"
    );
    let started = step.kill_started();
    let outcome = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(step.requested());
    go_tx.send(()).unwrap();
    assert_eq!(b.join().unwrap(), RaiseResult::Refused);
    assert!(started, "an accepted Kill reads as started");
    assert_eq!(
        outcome,
        StopOutcome::Killed,
        "a reader between two refusals reads the accepted Kill"
    );
}

/// Three raisers of one call: the first is accepted, then the lane is
/// sealed and two more are refused, one while the other is in flight. The
/// step reads `Killed` before, between and after them.
#[test]
fn a_killed_step_stays_killed_while_two_later_raisers_are_refused() {
    let (lane, _rx) = StopLane::new();
    let step = Arc::new(StopStep::accepted());
    let first = step.raise_kill(&lane, || {});
    assert_ne!(first, RaiseResult::Refused);
    lane.seal();
    let (b_in_flight_tx, b_in_flight_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let b = {
        let (lane, step) = (lane.clone(), step.clone());
        std::thread::spawn(move || {
            step.raise_kill(&lane, || {
                b_in_flight_tx.send(()).unwrap();
                go_rx.recv().unwrap();
            })
        })
    };
    b_in_flight_rx.recv().unwrap();
    let c = step.raise_kill(&lane, || {});
    assert_eq!(c, RaiseResult::Refused);
    go_tx.send(()).unwrap();
    assert_eq!(b.join().unwrap(), RaiseResult::Refused);
    let outcome = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(step.requested());
    assert_eq!(outcome, StopOutcome::Killed);
    assert!(step.kill_started());
}

/// A later raiser on a lane that still takes the `Kill` is accepted by the
/// lane's own answer, and the step stays `Killed`.
#[test]
fn a_later_raiser_on_an_open_lane_keeps_the_killed_step() {
    let (lane, _rx) = StopLane::new();
    let step = StopStep::accepted();
    assert_ne!(step.raise_kill(&lane, || {}), RaiseResult::Refused);
    assert_ne!(
        step.raise_kill(&lane, || {}),
        RaiseResult::Refused,
        "the lane has not sealed, so it answers the later Kill"
    );
    let outcome = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(step.requested());
    assert_eq!(outcome, StopOutcome::Killed);
}

fn poll_requested_once(step: &StopStep) -> std::task::Poll<StopOutcome> {
    let mut wait = std::pin::pin!(step.requested());
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    wait.as_mut().poll(&mut cx)
}

/// A raiser whose `Kill` the lane accepted has not left yet; a later raiser
/// is refused and leaves. A reader in that moment finds the step started and
/// still pending, never the plain request, and reads `Killed` once the
/// accepted raiser has left.
#[test]
fn a_refused_raiser_leaving_does_not_settle_the_step_of_an_accepted_one_still_in_flight() {
    let (lane, _rx) = StopLane::new();
    let step = Arc::new(StopStep::accepted());
    let seen = Arc::new(std::sync::Mutex::new(None));
    {
        let (lane, step, seen) = (lane.clone(), step.clone(), seen.clone());
        super::run_once_after_the_next_raise(move || {
            lane.seal();
            let refused = step.raise_kill(&lane, || {});
            *seen.lock().unwrap() = Some((
                refused,
                step.kill_started(),
                poll_requested_once(&step).is_pending(),
            ));
        });
    }
    let accepted = step.raise_kill(&lane, || {});
    assert_ne!(accepted, RaiseResult::Refused);
    let (refused, started, pending) = seen.lock().unwrap().take().unwrap();
    assert_eq!(
        refused,
        RaiseResult::Refused,
        "the later raiser finds the lane sealed"
    );
    assert!(
        started,
        "an accepted Kill reads as started while its raiser is in flight"
    );
    assert!(
        pending,
        "the step is pending while the accepted raiser is in flight"
    );
    assert_eq!(
        poll_requested_once(&step),
        std::task::Poll::Ready(StopOutcome::Killed)
    );
}

/// A raiser unwinds in its `between` while another raiser of the call is in
/// flight: the count keeps the other's share, and the other's accepted
/// `Kill` settles the step at `Killed`.
#[test]
fn a_raiser_that_unwinds_leaves_the_share_of_one_in_flight() {
    let (lane, _rx) = StopLane::new();
    let step = Arc::new(StopStep::accepted());
    let (a_took_tx, a_took_rx) = std::sync::mpsc::channel::<()>();
    let (b_in_flight_tx, b_in_flight_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let a = {
        let (lane, step) = (lane.clone(), step.clone());
        std::thread::spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                step.raise_kill(&lane, || {
                    a_took_tx.send(()).unwrap();
                    b_in_flight_rx.recv().unwrap();
                    panic!("unwind in between");
                })
            }))
        })
    };
    let b = {
        let (lane, step) = (lane.clone(), step.clone());
        std::thread::spawn(move || {
            a_took_rx.recv().unwrap();
            step.raise_kill(&lane, || {
                b_in_flight_tx.send(()).unwrap();
                go_rx.recv().unwrap();
            })
        })
    };
    assert!(a.join().unwrap().is_err(), "the first raiser unwound");
    assert_eq!(step.cell(), 1, "only the raiser still in flight is counted");
    assert!(step.kill_started());
    go_tx.send(()).unwrap();
    assert_ne!(b.join().unwrap(), RaiseResult::Refused);
    assert_eq!(step.cell(), StopStep::KILLED);
    assert_eq!(
        poll_requested_once(&step),
        std::task::Poll::Ready(StopOutcome::Killed)
    );
}

/// Two raisers of one call are both in flight and both refused: the count
/// rises to two, falls back to zero, and the step reads the plain request.
#[test]
fn two_refused_raisers_leave_the_plain_request() {
    let (lane, _rx) = StopLane::new();
    lane.seal();
    let step = StopStep::accepted();
    let inner = step.raise_kill(&lane, || {
        assert_eq!(step.cell(), 1);
        let second = step.raise_kill(&lane, || assert_eq!(step.cell(), 2));
        assert_eq!(second, RaiseResult::Refused);
        assert_eq!(
            step.cell(),
            1,
            "the second raiser gave back only its own share"
        );
        assert!(step.kill_started());
    });
    assert_eq!(inner, RaiseResult::Refused);
    assert_eq!(step.cell(), StopStep::REQUESTED);
    assert!(!step.kill_started());
    assert_eq!(
        poll_requested_once(&step),
        std::task::Poll::Ready(StopOutcome::Graceful)
    );
}

/// One raiser of a call, held at the three points of its raise so a test
/// orders them against another raiser's: inside `between` (entered), after
/// the lane's answer (tried) and after it left.
struct HeldRaiser {
    go: std::sync::mpsc::Sender<()>,
    reached: std::sync::mpsc::Receiver<()>,
    thread: std::thread::JoinHandle<RaiseResult>,
}

impl HeldRaiser {
    fn enter(lane: &StopLane, step: &Arc<StopStep>) -> Self {
        let (go, go_rx) = std::sync::mpsc::channel::<()>();
        let (reached_tx, reached) = std::sync::mpsc::channel::<()>();
        let (lane, step) = (lane.clone(), step.clone());
        let thread = std::thread::spawn(move || {
            let go_rx = std::rc::Rc::new(go_rx);
            let hook = {
                let (reached_tx, go_rx) = (reached_tx.clone(), go_rx.clone());
                move || {
                    reached_tx.send(()).unwrap();
                    go_rx.recv().unwrap();
                }
            };
            super::run_once_after_the_next_raise(hook);
            step.raise_kill(&lane, || {
                reached_tx.send(()).unwrap();
                go_rx.recv().unwrap();
            })
        });
        reached.recv().unwrap();
        Self {
            go,
            reached,
            thread,
        }
    }

    fn try_raise(&self) {
        self.go.send(()).unwrap();
        self.reached.recv().unwrap();
    }

    fn leave(self) -> RaiseResult {
        self.go.send(()).unwrap();
        self.thread.join().unwrap()
    }
}

/// Every order of two raisers' three points (enter, try, leave), as a
/// sequence of raiser indexes: each occurrence stands for that raiser's next
/// point.
fn interleavings() -> Vec<Vec<usize>> {
    fn go(left: [usize; 2], so_far: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        if left == [0, 0] {
            out.push(so_far.clone());
            return;
        }
        for who in 0..2 {
            if left[who] > 0 {
                let mut next = left;
                next[who] -= 1;
                so_far.push(who);
                go(next, so_far, out);
                so_far.pop();
            }
        }
    }
    let mut out = Vec::new();
    go([3, 3], &mut Vec::new(), &mut out);
    out
}

/// The position of `who`'s try, its second point, in `order`.
fn try_position(order: &[usize], who: usize) -> usize {
    order
        .iter()
        .enumerate()
        .filter(|(_, w)| **w == who)
        .nth(1)
        .unwrap()
        .0
}

fn assert_reads_killed(step: &StopStep, when: &str) {
    assert!(
        step.kill_started(),
        "{when}: an accepted Kill reads as started"
    );
    assert_ne!(
        poll_requested_once(step),
        std::task::Poll::Ready(StopOutcome::Graceful),
        "{when}: the step never reads the plain request after an accepted Kill"
    );
}

/// One raiser is accepted and one refused, in every order of their points.
/// From the accepted raiser's answer on, every state reads started and never
/// Graceful, and the step ends at `Killed`.
#[test]
fn an_accepted_and_a_refused_raiser_never_show_the_plain_request_in_any_order() {
    let mut ran = 0;
    for order in interleavings() {
        let (lane, _rx) = StopLane::new();
        let step = Arc::new(StopStep::accepted());
        let mut held: [Option<HeldRaiser>; 2] = [None, None];
        let mut points = [0usize; 2];
        let mut answers = [None, None];
        let mut accepted_answered = false;
        let mut tried = [false, false];
        // The refusal is final: the second raiser is refused only if the
        // first one's raise came first.
        if try_position(&order, 0) > try_position(&order, 1) {
            continue;
        }
        for who in &order {
            let who = *who;
            let label = format!("order {order:?} at raiser {who} point {}", points[who]);
            match points[who] {
                0 => held[who] = Some(HeldRaiser::enter(&lane, &step)),
                1 => {
                    held[who].as_ref().unwrap().try_raise();
                    tried[who] = true;
                    if who == 0 {
                        lane.seal();
                        accepted_answered = true;
                    }
                }
                _ => answers[who] = Some(held[who].take().unwrap().leave()),
            }
            points[who] += 1;
            if accepted_answered {
                assert_reads_killed(&step, &label);
            }
        }
        ran += 1;
        assert_ne!(answers[0], Some(RaiseResult::Refused), "{order:?}");
        assert_eq!(answers[1], Some(RaiseResult::Refused), "{order:?}");
        assert!(tried[0] && tried[1]);
        assert_eq!(step.cell(), StopStep::KILLED, "{order:?}");
        assert_eq!(
            poll_requested_once(&step),
            std::task::Poll::Ready(StopOutcome::Killed),
            "{order:?}"
        );
    }
    assert_eq!(ran, 10, "the orders in which the first raiser tries first");
}

/// Two raisers are both refused, in every order of their points: the step
/// reads started exactly while one is in flight and ends at the plain
/// request.
#[test]
fn two_refused_raisers_in_any_order_end_at_the_plain_request() {
    for order in interleavings() {
        let (lane, _rx) = StopLane::new();
        lane.seal();
        let step = Arc::new(StopStep::accepted());
        let mut held: [Option<HeldRaiser>; 2] = [None, None];
        let mut points = [0usize; 2];
        let mut in_flight = 0;
        for who in &order {
            let who = *who;
            match points[who] {
                0 => {
                    held[who] = Some(HeldRaiser::enter(&lane, &step));
                    in_flight += 1;
                }
                1 => held[who].as_ref().unwrap().try_raise(),
                _ => {
                    assert_eq!(held[who].take().unwrap().leave(), RaiseResult::Refused);
                    in_flight -= 1;
                }
            }
            points[who] += 1;
            assert_eq!(step.kill_started(), in_flight > 0, "order {order:?}");
        }
        assert_eq!(step.cell(), StopStep::REQUESTED, "{order:?}");
        assert_eq!(
            poll_requested_once(&step),
            std::task::Poll::Ready(StopOutcome::Graceful)
        );
    }
}

/// The lane writes a `Kill` before its wakes run, so an unwind out of the
/// raise comes after the write: the step reads `Killed`, never the plain
/// request.
#[test]
fn an_unwind_out_of_the_lane_after_its_write_reads_killed() {
    let (lane, _rx) = StopLane::new();
    let step = StopStep::accepted();
    crate::types::stop_lane::unwind_after_the_next_write();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        step.raise_kill(&lane, || {})
    }));
    assert!(outcome.is_err(), "the raise unwound");
    assert!(
        matches!(lane.reason(), Some(crate::types::StopReason::Kill)),
        "the lane holds the Kill it wrote"
    );
    assert_eq!(step.cell(), StopStep::KILLED);
    assert!(step.kill_started());
    assert_eq!(
        poll_requested_once(&step),
        std::task::Poll::Ready(StopOutcome::Killed)
    );
}

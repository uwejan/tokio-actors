//! The state machine engine through the harness.

use std::time::Duration;

use tokio_actors::statem::{
    EnterContext, Fired, Harness, ReplyTo, StateMachine, StatemContext, StatemEvent, Step,
    Transition,
};
use tokio_actors::StopReason;

#[rustfmt::skip]
enum Ev { Move(u8), Hold(u8), Tag(u8), Ins(u8), MoveIns(u8, u8), Arm, Zero, Cancel, Flood(u8), Stop, StopKill, Repeat, Fwd(ReplyTo<u16>) }

#[derive(Default)]
struct M {
    log: Vec<String>,
    held: Option<ReplyTo<u16>>,
}

const S: Duration = Duration::from_secs(1);

#[rustfmt::skip]
impl StateMachine for M {
    type State = u8;
    type Event = Ev;
    type Call = u16;
    type Reply = u16;
    type TimerName = &'static str;

    fn handle_event(&mut self, state: &u8, event: StatemEvent<Self>, cx: &mut StatemContext<'_, Self>) -> Transition<u8> {
        use StatemEvent::{Call, Cast, EventTimeout, Internal, StateTimeout, Timeout};
        let at = *state;
        match event {
            Cast(Ev::Move(n)) => { self.log.push(format!("m{n}")); return Transition::Next(n); }
            Cast(Ev::Hold(n)) if at == 0 => { if cx.postpone(Cast(Ev::Hold(n))).is_err() { self.log.push("full".into()); } }
            Cast(Ev::Hold(n)) => self.log.push(format!("h{n}@{at}")),
            Cast(Ev::Tag(n)) | Internal(Ev::Tag(n)) => self.log.push(format!("t{n}@{at}")),
            EventTimeout(Ev::Tag(n)) | StateTimeout(Ev::Tag(n)) | Timeout(_, Ev::Tag(n)) => self.log.push(format!("to{n}@{at}")),
            Cast(Ev::Ins(n)) => { let _ = (cx.next_event(Ev::Tag(n)), cx.next_event(Ev::Tag(n + 1))); }
            Cast(Ev::MoveIns(to, n)) => { let _ = cx.next_event(Ev::Tag(n)); self.log.push("mi".into()); return Transition::Next(to); }
            Cast(Ev::Arm) => { cx.event_timeout(S, Ev::Tag(100)); cx.state_timeout(2 * S, Ev::Tag(101)); cx.timeout("n", 3 * S, Ev::Tag(102)); }
            Cast(Ev::Zero) => cx.state_timeout(Duration::ZERO, Ev::Tag(50)),
            Cast(Ev::Cancel) => { let seen = (cx.cancel_event_timeout(), cx.cancel_state_timeout(), cx.cancel_timeout(&"n")); self.log.push(format!("{seen:?}")); }
            Cast(Ev::Flood(n)) => { let kept = (0..n).filter(|_| cx.postpone(Cast(Ev::Hold(0))).is_ok()).count(); self.log.push(format!("acc{kept}")); }
            Cast(Ev::Stop) => return Transition::Stop(StopReason::Graceful),
            Cast(Ev::StopKill) => return Transition::Stop(StopReason::Kill),
            Cast(Ev::Repeat) => return Transition::Repeat,
            Cast(Ev::Fwd(to)) => { cx.reply(to, 1); panic!("the callback panics holding another machine's address") }
            Call(254, to) if at == 0 => { let _ = cx.postpone(Call(254, to)); }
            Call(255, to) => drop(to),
            Call(253, to) => self.held = Some(to),
            Call(249, to) => { let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || { let _to = to; panic!("caught in the callback") })); }
            Call(252, to) => { cx.reply(to, 1000); return Transition::Next(2); }
            Call(c, to) => cx.reply(to, c * 2),
            _ => {}
        }
        Transition::Keep
    }

    fn on_enter(&mut self, old: &u8, new: &u8, cx: &mut EnterContext<'_, Self>) {
        self.log.push(format!("e{old}{new}"));
        if *new == 2 {
            if let Some(to) = self.held.take() { cx.reply(to, 2000); }
            cx.state_timeout(5 * S, Ev::Tag(7));
        }
    }

    fn terminate(&mut self, _reason: &StopReason, _state: &u8) { self.log.push("term".into()); }
}

type Out = (Step<M>, Vec<String>);

/// Drives a harness from its drop, which runs while the thread unwinds.
struct Late<'a>(&'a mut Vec<(usize, Option<u16>)>);

impl Drop for Late<'_> {
    fn drop(&mut self) {
        *self.0 = fresh().call(255).replies;
    }
}

fn fresh() -> Harness<M> {
    Harness::new(M::default(), 0).0
}

fn go(h: &mut Harness<M>, step: impl FnOnce(&mut Harness<M>) -> Step<M>) -> Out {
    h.machine_mut().log.clear();
    let step = step(h);
    (step, std::mem::take(&mut h.machine_mut().log))
}

fn cast(h: &mut Harness<M>, ev: Ev) -> Out {
    go(h, |h| h.cast(ev))
}

fn fire(h: &mut Harness<M>, kind: Fired<&'static str>) -> Out {
    go(h, |h| h.fire(kind))
}

fn strs(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
#[rustfmt::skip]
fn statem_engine_table() {
    // Start: the first enter call has old == new.
    let (mut h, step) = Harness::new(M::default(), 0);
    assert_eq!((step.entered, h.machine().log.clone()), (vec![0], strs(&["e00"])));

    // Postponed events retry oldest first, only after a state change; inserted events come before them.
    cast(&mut h, Ev::Hold(1));
    cast(&mut h, Ev::Hold(2));
    let (step, log) = cast(&mut h, Ev::Tag(9));
    assert_eq!((step.postponed, log), (2, strs(&["t9@0"])));
    let (step, log) = cast(&mut h, Ev::Move(1));
    assert_eq!((step.postponed, step.entered, log), (0, vec![1], strs(&["m1", "e01", "h1@1", "h2@1"])));
    assert_eq!(cast(&mut h, Ev::Ins(5)).1, ["t5@1", "t6@1"]);
    let mut h = fresh();
    cast(&mut h, Ev::Hold(3));
    assert_eq!(cast(&mut h, Ev::MoveIns(1, 5)).1, ["mi", "e01", "t5@1", "h3@1"]);

    // The three time-outs: set, fired once, cancelled by an event, by a state change, by the machine, replaced.
    let mut h = fresh();
    assert_eq!(cast(&mut h, Ev::Arm).0.set, [(Fired::Event, S), (Fired::State, 2 * S), (Fired::Named("n"), 3 * S)]);
    assert_eq!(fire(&mut h, Fired::Event).1, ["to100@0"]);
    assert!(fire(&mut h, Fired::Event).1.is_empty(), "a fired time-out is not running");
    assert!(cast(&mut h, Ev::Tag(1)).0.cancelled.is_empty(), "the event time-out already fired");
    cast(&mut h, Ev::Arm);
    assert_eq!(cast(&mut h, Ev::Tag(1)).0.cancelled, [Fired::Event], "an event handled first cancels the event time-out");
    assert!(fire(&mut h, Fired::Event).1.is_empty(), "a cancelled time-out never reaches the handler");
    assert_eq!(cast(&mut h, Ev::Move(1)).0.cancelled, [Fired::State], "a state change cancels the state time-out");
    assert!(fire(&mut h, Fired::State).1.is_empty());
    assert_eq!(fire(&mut h, Fired::Named("n")).1, ["to102@1"], "the named time-out outlives both");
    cast(&mut h, Ev::Arm);
    assert_eq!(cast(&mut h, Ev::Arm).0.cancelled, [Fired::Event, Fired::State, Fired::Named("n")], "a new time-out replaces the running one");
    assert_eq!(fire(&mut h, Fired::State).1, ["to101@1"]);
    assert!(fire(&mut h, Fired::State).1.is_empty(), "one firing per setting, whatever the generation");
    cast(&mut h, Ev::Arm);
    let (step, log) = cast(&mut h, Ev::Cancel);
    assert_eq!((log, step.cancelled.len()), (strs(&["(false, true, true)"]), 3));
    assert!(fire(&mut h, Fired::Named("n")).1.is_empty());
    let (step, log) = cast(&mut h, Ev::Zero);
    assert_eq!((step.set, log), (vec![(Fired::State, Duration::ZERO)], strs(&["to50@1"])));

    // Replies go out before the enter call's, and a call postponed is answered after the change.
    let mut h = fresh();
    assert_eq!([3, 255, 254].map(|c| h.call(c).replies), [vec![(0, Some(6))], vec![(1, None)], vec![]]);
    assert_eq!(h.cast(Ev::Move(1)).replies, [(2, Some(508))]);
    assert!(h.call(253).replies.is_empty());
    let step = h.call(252);
    assert_eq!((step.replies, step.entered, step.set), (vec![(4, Some(1000)), (3, Some(2000))], vec![2], vec![(Fired::State, 5 * S)]));

    // An address dropped in a panic the callback catches answers NoReply, and the machine goes on.
    let mut h = fresh();
    assert_eq!([249, 3].map(|c| h.call(c).replies), [vec![(0, None)], vec![(1, Some(6))]]);
    // A harness driven while the thread unwinds answers a dropped address when its step ends.
    let mut seen = Vec::new();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| { let _late = Late(&mut seen); panic!("the thread unwinds") }));
    assert_eq!(seen, [(0, None)]);

    // A reply to another harness's address recorded by a callback that panics answers it with no reply.
    let (mut a, mut b) = (fresh(), fresh());
    a.call(253);
    let to = a.machine_mut().held.take().unwrap();
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| b.cast(Ev::Fwd(to)))).is_err());
    assert_eq!((a.call(3).replies, a.running()), (vec![(0, None), (1, Some(6))], true));

    // A stop runs terminate, drops the calls it holds without an entry and ends the harness.
    let mut h = fresh();
    h.call(254);
    let step = h.cast(Ev::Stop);
    assert_eq!((step.stopped, step.replies.len(), h.running()), (Some(StopReason::Graceful), 0, false));
    assert_eq!(h.machine().log.last().map(String::as_str), Some("term"));
    assert!(h.cast(Ev::Tag(1)).entered.is_empty() && h.stop(StopReason::Graceful).stopped.is_none());

    // Repeat runs the enter call again and, like Keep, retries no postponed event.
    let mut h = fresh();
    cast(&mut h, Ev::Hold(1));
    let (step, log) = cast(&mut h, Ev::Repeat);
    assert_eq!((step.entered, step.postponed, log), (vec![0], 1, strs(&["e00"])));
    let (step, log) = cast(&mut h, Ev::Tag(2));
    assert_eq!((step.postponed, log), (1, strs(&["t2@0"])));

    // Stop(Kill) is refused: the machine stops with a failure and terminate runs; a stop from outside with Kill runs no terminate.
    let mut h = fresh();
    let (step, log) = cast(&mut h, Ev::StopKill);
    assert!(matches!(step.stopped, Some(StopReason::Failure(_))), "{:?}", step.stopped);
    assert_eq!((log, h.running()), (strs(&["term"]), false));
    let mut h = fresh();
    h.machine_mut().log.clear();
    assert_eq!((h.stop(StopReason::Kill).stopped, h.machine().log.is_empty(), h.running()), (Some(StopReason::Kill), true, false));

    // Held events stay within the mailbox capacity; a postpone over it is handed back.
    let mut h = fresh();
    let (step, log) = cast(&mut h, Ev::Flood(70));
    assert_eq!((step.postponed, log), (64, strs(&["acc64"])));
    assert_eq!(cast(&mut h, Ev::Hold(1)).1, ["full"]);
}

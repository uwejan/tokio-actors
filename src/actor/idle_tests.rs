//! The state of an idle spell, one table of operations and what each leaves.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::actor::context::ActorContext;
use crate::actor::turn::{IdleKind, TurnState};
use crate::actor::Actor;
use crate::error::ActorResult;
use crate::types::ActorId;

struct Dummy;

impl Actor for Dummy {
    type Message = u8;
    type Response = ();

    async fn handle(&mut self, _: u8, _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

enum Op {
    SetIdle(u64),
    SetReceive(u64),
    ClearIdle,
    ClearReceive,
    /// The loop is about to wait, `at` ms after the base, with both queues
    /// empty (`true`) or not; what `idle_next` answers: the instant in ms
    /// after the base and the kind.
    Park(u64, bool, Option<(u64, IdleKind)>),
    /// A user turn starts.
    Turn,
    Fire(IdleKind),
    Inline(bool),
}

fn run(name: &str, ops: &[Op]) {
    let base = Instant::now();
    let mut turn = TurnState::<Dummy>::new(crate::actor::runtime::Owner::new(
        ActorId::from("spell"),
        false,
    ));
    for (step, op) in ops.iter().enumerate() {
        match op {
            Op::SetIdle(n) => turn.set_idle(ms(*n)),
            Op::SetReceive(n) => turn.set_receive(ms(*n), Arc::new(|| 0u8)),
            Op::ClearIdle => turn.clear_idle(),
            Op::ClearReceive => turn.clear_receive(),
            Op::Park(at, quiet, want) => {
                let got = turn.idle_next(base + ms(*at), *quiet);
                let want = want.map(|(at, kind)| (base + ms(at), kind));
                assert_eq!(got, want, "{name}: step {step}");
            }
            Op::Turn => turn.end_spell(),
            Op::Fire(kind) => turn.mark_fired(*kind),
            Op::Inline(on) => turn.set_inline(*on),
        }
    }
}

#[test]
fn spell_state_table() {
    use IdleKind::{Idle, Receive};
    use Op::*;
    run(
        "a later park does not restart the spell",
        &[
            SetIdle(100),
            Park(0, true, Some((100, Idle))),
            Park(40, true, Some((100, Idle))),
            Park(90, true, Some((100, Idle))),
        ],
    );
    run(
        "a loop holding a message starts no spell",
        &[
            SetIdle(100),
            Park(0, false, None),
            Park(5, true, Some((105, Idle))),
        ],
    );
    run(
        "a threshold fires once per spell and again after a turn",
        &[
            SetIdle(100),
            Park(0, true, Some((100, Idle))),
            Fire(Idle),
            Park(100, true, None),
            Park(500, true, None),
            Turn,
            Park(600, true, Some((700, Idle))),
        ],
    );
    run(
        "a turn that a fire causes does not end its spell",
        &[
            SetIdle(0),
            Park(10, true, Some((10, Idle))),
            Inline(true),
            Turn,
            Inline(false),
            Fire(Idle),
            Park(10, true, None),
        ],
    );
    run(
        "a turn with no fire running ends the spell",
        &[
            SetIdle(0),
            Park(10, true, Some((10, Idle))),
            Fire(Idle),
            Turn,
            Park(20, true, Some((20, Idle))),
        ],
    );
    run(
        "both thresholds due together: the receive timeout first",
        &[
            SetIdle(100),
            SetReceive(100),
            Park(0, true, Some((100, Receive))),
            Fire(Receive),
            Park(100, true, Some((100, Idle))),
            Fire(Idle),
            Park(100, true, None),
        ],
    );
    run(
        "the nearer threshold comes first whichever kind it is",
        &[
            SetIdle(50),
            SetReceive(100),
            Park(0, true, Some((50, Idle))),
            Fire(Idle),
            Park(50, true, Some((100, Receive))),
        ],
    );
    run(
        "a setter moves the threshold and not the spell",
        &[
            SetIdle(100),
            Park(0, true, Some((100, Idle))),
            SetIdle(30),
            Park(10, true, Some((30, Idle))),
            ClearIdle,
            Park(10, true, None),
            SetReceive(70),
            Park(10, true, Some((70, Receive))),
            ClearReceive,
            Park(10, true, None),
        ],
    );
    run(
        "a huge length is capped, never an overflow",
        &[
            SetIdle(u64::MAX / 4_000_000),
            Park(0, true, Some((946_080_000_000, Idle))),
        ],
    );
}

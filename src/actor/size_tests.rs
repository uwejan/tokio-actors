//! The hot structures every actor holds keep their size: an actor that uses
//! none of the optional features pays for none of them.

use std::mem::size_of;

use crate::actor::context::ActorContext;
use crate::actor::monitor::MonitorState;
use crate::actor::Actor;
use crate::error::ActorResult;

struct Counter;

impl Actor for Counter {
    type Message = u32;
    type Response = u32;

    async fn handle(&mut self, msg: u32, _: &mut ActorContext<Self>) -> ActorResult<u32> {
        Ok(msg)
    }
}

/// A message type that is large.
struct Big;

impl Actor for Big {
    type Message = [u8; 1024];
    type Response = u32;

    async fn handle(&mut self, _: [u8; 1024], _: &mut ActorContext<Self>) -> ActorResult<u32> {
        Ok(0)
    }
}

/// The monitor state, which an actor with only a monitor or a link holds,
/// carries no cell for a message: its size does not follow the message type.
/// The turn state, which holds the `continue_with` cell, sits behind a pointer
/// of its own that stays null until the first use of a timeout, an idle
/// length, a continuation or an alias.
#[test]
fn the_monitor_state_holds_no_message_cell() {
    assert_eq!(
        size_of::<MonitorState<Big>>(),
        size_of::<MonitorState<Counter>>(),
        "the monitor state grew with the message type"
    );
}

/// An actor whose message is an enum.
#[cfg_attr(
    not(all(target_arch = "x86_64", target_os = "linux")),
    allow(dead_code)
)]
struct Tagged;

#[allow(dead_code)]
enum Say {
    Ping,
    Text(String),
    Pair(u32, u32),
}

impl Actor for Tagged {
    type Message = Say;
    type Response = u32;

    async fn handle(&mut self, _: Say, _: &mut ActorContext<Self>) -> ActorResult<u32> {
        Ok(0)
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
const RUN_FUTURE_U32: usize = 1344;
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
const RUN_FUTURE_ENUM: usize = 1344;

use super::*;
use crate::error::ActorResult;

struct Counter {
    n: u64,
    spin_ns: u64,
}

enum Msg {
    Add(u64),
    Get,
}

impl Actor for Counter {
    type Message = Msg;
    type Response = u64;

    async fn handle(&mut self, msg: Msg, _ctx: &mut ActorContext<Self>) -> ActorResult<u64> {
        if let Msg::Add(k) = msg {
            self.n += k + self.spin_ns;
        }
        Ok(self.n)
    }
}

fn size_of_output<T>(_: impl FnOnce() -> T) -> usize {
    std::mem::size_of::<T>()
}

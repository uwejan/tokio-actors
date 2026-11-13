//! The graph of sends that wait on other actors, searched for cycles.
//!
//! A send or notify that is waiting (for room in a full mailbox or priority
//! lane, or for the reply to a request) is an edge from the actor whose hook
//! made the call to the actor it waits on. Sends with a deadline, notifies
//! through an alias and the system-channel calls are not edges. An edge exists exactly as long as the waiting future does: the
//! future's guard removes it when the wait completes, is cancelled or is
//! aborted. The graph only reports a cycle; it changes the outcome of no send.
//!
//! One mutex guards the whole graph. It is never held across an await, a wake
//! or a call into user code, and the search runs under it so that two edges
//! added at the same moment cannot both report the cycle they close.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use super::actor_ref::ActorRef;
use super::event::{EventKind, TelemetryEvent};
use super::mode::{Mode, CTL_DEADLOCK};
use super::token;
use crate::actor::subtree::AbortRecord;

/// The id of one recorded wait edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EdgeId(pub(crate) u64);

/// One edge: the actor it waits on, and that actor's reference when the
/// recording site knew it.
#[derive(Debug)]
struct Edge {
    id: EdgeId,
    target: u64,
    who: Option<ActorRef>,
}

#[derive(Debug, Default)]
struct Edges {
    /// The edges of each caller, oldest first.
    by_caller: HashMap<u64, Vec<Edge>>,
    /// The id of the last edge recorded; ids start at 1.
    last: u64,
}

impl Edges {
    fn insert(&mut self, caller: u64, target: u64, who: Option<ActorRef>) -> EdgeId {
        self.last += 1;
        let id = EdgeId(self.last);
        self.by_caller
            .entry(caller)
            .or_default()
            .push(Edge { id, target, who });
        id
    }
}

/// The waits in progress, as edges from a caller's token to the token of the
/// actor it waits on.
#[derive(Debug, Default)]
pub(crate) struct WaitGraph {
    edges: Mutex<Edges>,
}

/// The members of a cycle, caller first, each with the reference the edge
/// into it carries.
type Cycle = Vec<(u64, Option<ActorRef>)>;

impl WaitGraph {
    fn lock(&self) -> MutexGuard<'_, Edges> {
        self.edges.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Records that `caller` waits on `target` and returns the edge's id.
    pub(crate) fn add(&self, caller: u64, target: u64) -> EdgeId {
        self.lock().insert(caller, target, None)
    }

    /// Records the edge and searches for the cycle it closes in one step, so
    /// of two edges added at once the later one reports the cycle and the
    /// earlier one does not. The members come back only when an edge names
    /// every one of them; `search` false skips the search.
    pub(crate) fn record(
        &self,
        caller: u64,
        target: u64,
        who: ActorRef,
        search: bool,
    ) -> (EdgeId, Option<Vec<ActorRef>>) {
        let mut edges = self.lock();
        let id = edges.insert(caller, target, Some(who));
        let members = if search {
            find_cycle(&edges, caller)
                .and_then(|cycle| cycle.into_iter().map(|(_, who)| who).collect())
        } else {
            None
        };
        (id, members)
    }

    /// The tokens of the cycle the newest edge of `caller` closed, caller
    /// first, or `None` when it closed none.
    pub(crate) fn cycle_through(&self, caller: u64) -> Option<Vec<u64>> {
        let edges = self.lock();
        let cycle = find_cycle(&edges, caller)?;
        Some(cycle.into_iter().map(|(token, _)| token).collect())
    }

    /// Removes the edge `id` of `caller`, and only that edge.
    pub(crate) fn remove(&self, caller: u64, id: EdgeId) {
        let removed = {
            let mut edges = self.lock();
            let Some(list) = edges.by_caller.get_mut(&caller) else {
                return;
            };
            let removed = list
                .iter()
                .position(|edge| edge.id == id)
                .map(|at| list.remove(at));
            if list.is_empty() {
                edges.by_caller.remove(&caller);
            }
            removed
        };
        // The reference goes after the lock is released.
        drop(removed);
    }

    /// The target of the first current edge of `caller`.
    pub(crate) fn target_of(&self, caller: u64) -> Option<u64> {
        let edges = self.lock();
        edges
            .by_caller
            .get(&caller)?
            .first()
            .map(|edge| edge.target)
    }

    /// The number of edges recorded.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().by_caller.values().map(Vec::len).sum()
    }
}

/// A search frame: a node and the position of its next edge to try.
struct Frame<'g> {
    node: u64,
    edges: &'g [Edge],
    next: usize,
}

/// The cycle the newest edge of `caller` closes: a depth-first search from
/// that edge's target for a way back to `caller`. The members are the caller
/// and every node on the way, each paired with the reference of the edge
/// into it.
fn find_cycle(edges: &Edges, caller: u64) -> Option<Cycle> {
    let newest = edges.by_caller.get(&caller)?.last()?;
    if newest.target == caller {
        return Some(vec![(caller, newest.who.clone())]);
    }
    let frame = |node: u64| Frame {
        node,
        edges: edges.by_caller.get(&node).map_or(&[][..], Vec::as_slice),
        next: 0,
    };
    let mut seen: HashSet<u64> = HashSet::from([newest.target]);
    let mut stack = vec![frame(newest.target)];
    // The edge that entered each frame of the stack.
    let mut entered: Vec<&Edge> = vec![newest];
    while let Some(top) = stack.last_mut() {
        let Some(edge) = top.edges.get(top.next) else {
            stack.pop();
            entered.pop();
            continue;
        };
        top.next += 1;
        if edge.target == caller {
            let mut cycle: Cycle = vec![(caller, edge.who.clone())];
            cycle.extend(
                stack
                    .iter()
                    .zip(&entered)
                    .map(|(frame, via)| (frame.node, via.who.clone())),
            );
            return Some(cycle);
        }
        if seen.insert(edge.target) {
            stack.push(frame(edge.target));
            entered.push(edge);
        }
    }
    None
}

/// The edge a waiting send holds in the graph of the target's system; the
/// edge goes when the guard drops.
pub(crate) struct WaitEdge<'a> {
    mode: &'a Arc<Mode>,
    caller: u64,
    id: EdgeId,
}

impl Drop for WaitEdge<'_> {
    fn drop(&mut self) {
        self.mode.wait.remove(self.caller, self.id);
    }
}

/// Whether a send that first returns `Pending` may be recorded: the thread
/// polls a hook that carries a caller token, and the target's system has
/// wait-cycle detection on. The token is one load of a plain thread-local and
/// a branch, and the mode word is read only behind it, so a send from a task
/// that is in no hook pays that load and nothing else, and no call.
#[inline(always)]
pub(crate) fn wait_wanted(record: &AbortRecord) -> bool {
    token::peek() != 0
        && record
            .mode()
            .is_some_and(|mode| mode.word() & CTL_DEADLOCK != 0)
}

/// Records the wait of the actor whose hook this thread polls on the
/// incarnation behind `record`; called when a send first returns `Pending`
/// and [`wait_wanted`] says detection is on.
///
/// Nothing is recorded when the target's system has wait-cycle detection off,
/// when the thread is in no hook (token 0), or when the target is not in the
/// registry (it has ended). A target spawned before the switch or any sink
/// has its identity, and its token, resolved from the registry. When the new
/// edge closes a cycle, one `WaitCycle` event is queued for the pump; the send
/// itself is not touched.
#[inline(never)]
pub(crate) fn begin_wait(record: &AbortRecord) -> Option<WaitEdge<'_>> {
    let mode = record.mode()?;
    if mode.word() & CTL_DEADLOCK == 0 {
        return None;
    }
    let caller = token::current();
    if caller == 0 {
        return None;
    }
    // An actor spawned before the switch or any sink has no identity yet: the
    // registry gives it one, as it gives a late subscriber's, once per actor.
    let target = super::identity_for(mode, record, None, None)?;
    let target_token = target.token();
    if target_token == 0 {
        return None;
    }
    let (id, members) = mode.wait.record(
        caller,
        target_token,
        target.actor().clone(),
        mode.armed(EventKind::WaitCycle),
    );
    if let Some(members) = members {
        // The caller closed the cycle on its own task, and its next hot
        // event must not overtake the report: the caller owns the event.
        let owner = members
            .first()
            .and_then(|caller| mode.system()?.resolve_identity_of_id(caller.id()));
        super::push_for(mode, owner.as_ref(), EventKind::WaitCycle, || {
            TelemetryEvent::WaitCycle { members }
        });
    }
    Some(WaitEdge { mode, caller, id })
}

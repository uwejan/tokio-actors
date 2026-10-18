//! Exhaustive, deterministic exploration of the supervision rules: every
//! interleaving of child behavior, supervisor turns, manual calls and stop
//! signals up to the configured bounds, with every invariant checked after
//! every step and the serial outcome checked at every quiescent state.
//!
//! The module is a dev-only model: standard library only. The model has no
//! runtime, clock or I/O; only the test runner times and prints. It states
//! what a supervisor must do, as a transition system over a small value type,
//! and a depth-first search visits every reachable state of each
//! configuration. The model's decisions (the "logic") are separated from the
//! facts the checks compare them against (the "harness" parts of each step and
//! the `oracle` record), so an edit to one decision is seen by a check that
//! does not share it.
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::rc::Rc;

/// Incarnation token: drawn from one counter, never reused within a run.
type Tok = u8;
/// Child key: an index into the configuration's child list.
type Key = u8;

/// A supervisor: the one under test, or the supervision state of a
/// supervisor incarnation.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum SupId {
    S,
    Inc(Tok),
}

/// The registry key of a child: its id, which is also its name when it is
/// named.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum IdKey {
    /// A child of the supervisor under test.
    S(Key),
    /// A named worker of a supervisor incarnation: the id repeats across the
    /// incarnations of its supervisor.
    W(Key),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Cause {
    Crash,
    InitErr,
    NameTaken,
    StartTimeout,
    Shutdown,
    Panic,
    Duplicate,
    Unresponsive(Key),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Reason {
    Graceful,
    ParentRequest,
    Failure(Cause),
    ParentKilled,
    Cancelled,
    Kill,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Strength {
    Requested,
    Forced,
    Kill,
}

fn strength(r: Reason) -> Strength {
    match r {
        Reason::Graceful | Reason::ParentRequest => Strength::Requested,
        Reason::Failure(_) | Reason::ParentKilled | Reason::Cancelled => Strength::Forced,
        Reason::Kill => Strength::Kill,
    }
}

/// Rank inside a strength: two failures rank equal.
fn rank(r: Reason) -> u8 {
    match r {
        Reason::Graceful | Reason::Cancelled | Reason::Kill => 0,
        Reason::ParentRequest | Reason::ParentKilled => 1,
        Reason::Failure(_) => 2,
    }
}

fn vetoable(r: Option<Reason>) -> bool {
    matches!(r, Some(Reason::Graceful | Reason::ParentRequest))
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Kind {
    Bounce,
    Terminate,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Pend {
    Nothing,
    Snapshot,
    Evaluate,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Life {
    Running(Tok),
    Stopping { n: Tok, kind: Kind },
    Restarting { prev: Tok, next: Tok },
    Down { n: Tok, pend: Pend },
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Origin {
    Initial,
    Strategy,
    Bounce,
    Manual,
    Chain,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum OrderFrom {
    Manual,
    Group,
    Teardown,
    ChainStop,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Order {
    from: OrderFrom,
    raised: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum AttemptStage {
    Wait,
    Factory,
    Init,
    InTransit,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Exec {
    Attempt(AttemptStage),
    Runner,
    RunnerFailed,
    Orphan,
    Returned,
}

impl Exec {
    fn executor_gone(self) -> bool {
        matches!(self, Exec::RunnerFailed | Exec::Orphan | Exec::Returned)
    }
}

/// The stage of a deadline entry the final sweep armed on a leftover: armed
/// until its own global deadline passes, then its `Kill` due, then its abort
/// one kill grace later.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum SweepStage {
    Armed,
    KillDue,
    AbortDue,
}

/// A backstop the system holds on a live incarnation.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Reap {
    /// An abort due once the target's kill grace has passed.
    AbortDue,
    /// A supervisor child under a killed parent: a Kill at its window, then
    /// the abort.
    KillThenAbort,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Ladder {
    Off,
    Window,
    WindowInf,
    KillGrace,
    AbortGrace,
    Spent,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Outcome {
    Adopted,
    Failed(Reason),
    Cancelled,
    Stopped(Reason),
    /// The successor wait on the predecessor's subtree expired.
    PredecessorUnresponsive,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Action {
    RestartInitiated,
    Removed,
    BudgetExhausted,
    SupervisorStopping,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum At {
    Turn,
    PreStart,
    OnStarted,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Stage {
    Pending,
    PreStart,
    OnStarted,
    Idle,
    Turn,
    Stuck(At),
    Gate(Reason),
    Final,
    Hook,
    Teardown,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Status {
    Initializing,
    Running,
    Stopping,
    Stopped,
    Closed,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum LaneMode {
    Open,
    KillOnly,
    Sealed,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Answer {
    Ok,
    ChildNotFound,
    ChildRunning,
    ChildRestarting,
    ChildUnresponsive,
    DuplicateChild,
    NameTaken,
    Absorbed,
    SystemShuttingDown,
    SpawnUnresponsive,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum SPhase {
    Loop,
    Final(Reason),
    Hook(Reason),
    Teardown,
    Done(Reason),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum SysPhase {
    Active,
    ShuttingDown,
    Defunct,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum ShutStep {
    Requested,
    KillRaised,
    Swept,
}

/// The system's shutdown driver.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Shut {
    step: ShutStep,
    /// Whether its ladder has raised the first request on S yet.
    reached: bool,
    abandoned: bool,
    deadline: bool,
    /// Whether its final sweep took its snapshot and armed the deadline
    /// entries.
    sweep_started: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum SCause {
    Budget,
    Unresponsive,
    ParentStop,
    ParentKill,
    Shutdown,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Path {
    Commit,
    Join,
    Override,
}

/// The one handler of the supervisor that is awaiting, if any.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Caller {
    c: Key,
    t: Tok,
    path: Path,
    kind: CKind,
}

/// What the handler waits for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum CKind {
    /// A manual stop of the incarnation `t`.
    Stop,
    /// A `terminate_child` waiting for the subtree of `t` to end.
    TermSubtree,
    /// A same-id `spawn_child` waiting for the retired subtree of `t`.
    SpawnWait,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Memb {
    Outside,
    Awaited,
    Queued,
    Released,
}

/// The ledger state of a key as the manual table reads it: a live or ended
/// occupant, an attempt in flight, or a down child.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Row {
    NoSpec,
    RunningLive,
    RunningEnded,
    BounceLive,
    BounceEnded,
    TerminateLive,
    TerminateEnded,
    Attempt,
    DownPlain,
    DownSnapshot,
    DownOwed,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum CallOp {
    Stop,
    Terminate,
    Restart,
    Delete,
    Spawn,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct CallRecord {
    op: CallOp,
    c: Key,
    t: Tok,
    row: Row,
    memb: Memb,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum GKind {
    All,
    Rest,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Phase {
    Stop {
        awaiting: BTreeMap<Key, Tok>,
        queue: VecDeque<Key>,
        signalled: BTreeSet<Key>,
        restart_order: Vec<Key>,
    },
    Chain {
        queue: VecDeque<Key>,
        started: BTreeMap<Key, Tok>,
    },
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Group {
    kind: GKind,
    members: BTreeMap<Key, Option<Tok>>,
    phase: Phase,
    overrides: BTreeSet<Key>,
    restarted: BTreeSet<Key>,
    outside: BTreeSet<Key>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Driver {
    walk: VecDeque<Key>,
    current: Option<(Key, Tok)>,
    pool_wait: BTreeSet<(Key, Tok)>,
    reason: Reason,
    /// Whether the supervisor's window covers the stop budgets of the keys
    /// it had to stop when its teardown began; fixed then.
    covers: bool,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
struct Sup {
    order: Vec<Key>,
    ledger: BTreeMap<Key, Life>,
    group: Option<Group>,
    queued: Vec<(Key, Tok, Reason)>,
    snapshots: BTreeMap<(Key, Tok), (Reason, Action)>,
    deaths: BTreeSet<(Key, Tok, Reason)>,
    outcomes: BTreeMap<Tok, (Key, Outcome)>,
    charges: u8,
    /// The most recent incarnation spawned for each key, adopted or not.
    pred: BTreeMap<Key, Tok>,
    /// The key's last incarnation whose subtree had not ended when the spec
    /// was removed.
    retired: BTreeMap<Key, Tok>,
    teardown: Option<Driver>,
    final_q: Vec<(Key, Tok, Reason)>,
    final_err: bool,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Cell {
    sup: SupId,
    key: Key,
    origin: Origin,
    exec: Exec,
    order: Option<Order>,
    ladder: Ladder,
    fate: Option<Reason>,
    verdict: bool,
    cancelled: bool,
    closed: bool,
    st_fired: bool,
    /// Harness fact: the start timeout found no Kill pending in the lane.
    st_named: bool,
    /// The predecessor whose subtree an attempt in its wait stage waits for.
    waits_on: Option<Tok>,
    /// Harness fact: why the registration of an attempt that never spawned
    /// was refused, read from the state before its spawn step.
    refusal: Option<Cause>,
    owed: bool,
    delivered: bool,
    excused: bool,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Inc {
    sup: SupId,
    key: Key,
    id: IdKey,
    /// The supervisor incarnation that spawned it; none for the supervisor
    /// under test's own children.
    parent: Option<Tok>,
    /// The finite budget of the subtree: raised at every creation of a child
    /// and never lowered.
    below: u64,
    stage: Stage,
    lane: Option<Reason>,
    unseen: bool,
    lane_mode: LaneMode,
    vetoed: bool,
    reasked: bool,
    /// A `terminate_child` override gave the one more ask (recorded only in
    /// the configurations that set `mark_override_asks`).
    ask_by_override: bool,
    status: Status,
    stop_reason: Option<Reason>,
    end: Option<Reason>,
    acked: bool,
    aborted: bool,
}

/// Facts the checks compare against, written by the harness part of a step or
/// by check code at the moment a fact happens, never by the logic under test.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
struct Oracle {
    /// Triggering events counted per supervisor, each capped at its budget
    /// plus one.
    charges_s: u8,
    charges_p: BTreeMap<Tok, u8>,
    /// The supervisors that stopped for budget exhaustion.
    exhausted_s: bool,
    exhausted_p: BTreeSet<Tok>,
    /// The supervisors one of whose final deliveries failed.
    final_failed_s: bool,
    final_failed_p: BTreeSet<Tok>,
    stays_down: BTreeSet<Key>,
    s_cause: Option<SCause>,
    /// Why each supervisor incarnation left its loop.
    p_cause: BTreeMap<Tok, SCause>,
    /// The stop request a supervisor incarnation's lane holds while the
    /// strongest reason raised on it is a request, recorded from the raises
    /// themselves.
    p_req: BTreeMap<Tok, Reason>,
    /// The supervisor incarnations whose teardown window covers the stop
    /// budgets of the keys it began with, computed from the configuration.
    p_covers: BTreeSet<Tok>,
    chain_log: Vec<Key>,
    group_log: BTreeSet<Key>,
    group_outside: BTreeSet<Key>,
    removed: BTreeSet<Key>,
    decided: BTreeMap<(Key, Tok), Action>,
    /// The most recent incarnation created for each child, failed attempts
    /// included, kept across spec removal; none once the token was dropped.
    /// Index: key of the supervisor under test; 0 never spawned, 255 gone.
    last_s: [u8; 4],
    last_p: BTreeMap<(Tok, Key), Option<Tok>>,
    pending_call: Option<CallRecord>,
    /// A later shutdown call happened.
    later_shutdown: bool,
    /// The live incarnations the final sweep found at its snapshot, recorded
    /// by the harness whatever the logic arms for them.
    sweep_leftovers: BTreeSet<Tok>,
}

impl Oracle {
    fn charges_of(&self, x: SupId) -> u8 {
        match x {
            SupId::S => self.charges_s,
            SupId::Inc(p) => self.charges_p.get(&p).copied().unwrap_or(0),
        }
    }

    fn set_charges(&mut self, x: SupId, v: u8) {
        match x {
            SupId::S => self.charges_s = v,
            SupId::Inc(p) => {
                self.charges_p.insert(p, v);
            }
        }
    }

    fn exhausted(&self, x: SupId) -> bool {
        match x {
            SupId::S => self.exhausted_s,
            SupId::Inc(p) => self.exhausted_p.contains(&p),
        }
    }

    fn mark_exhausted(&mut self, x: SupId) {
        match x {
            SupId::S => self.exhausted_s = true,
            SupId::Inc(p) => {
                self.exhausted_p.insert(p);
            }
        }
    }

    fn failed_final(&self, x: SupId) -> bool {
        match x {
            SupId::S => self.final_failed_s,
            SupId::Inc(p) => self.final_failed_p.contains(&p),
        }
    }

    fn mark_failed_final(&mut self, x: SupId) {
        match x {
            SupId::S => self.final_failed_s = true,
            SupId::Inc(p) => {
                self.final_failed_p.insert(p);
            }
        }
    }

    /// Whether an incarnation was ever created for the child.
    fn spawned_before(&self, x: SupId, c: Key) -> bool {
        match x {
            SupId::S => self.last_s[c as usize] != 0,
            SupId::Inc(p) => self.last_p.contains_key(&(p, c)),
        }
    }

    /// The most recent incarnation created for the child; none once its
    /// subtree is gone.
    fn last_of(&self, x: SupId, c: Key) -> Option<Tok> {
        match x {
            SupId::S => match self.last_s[c as usize] {
                0 | 255 => None,
                t => Some(t),
            },
            SupId::Inc(p) => self.last_p.get(&(p, c)).copied().flatten(),
        }
    }

    fn set_last(&mut self, x: SupId, c: Key, t: Option<Tok>) {
        match x {
            SupId::S => self.last_s[c as usize] = t.unwrap_or(255),
            SupId::Inc(p) => {
                self.last_p.insert((p, c), t);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
struct Budgets {
    crash_s: u8,
    crash_w: u8,
    /// The shutdown calls, each at most once: begin, abandon, a later one,
    /// reactivate.
    shut_begin: u8,
    shut_abandon: u8,
    shut_later: u8,
    shut_reactivate: u8,
    manual: u8,
    parent_stop: u8,
    parent_kill: u8,
    /// One shared unit, spent by either a parent stop or a parent kill.
    parent: u8,
    third: u8,
    bare_kill: u8,
    stuck: u8,
    runner_fail: u8,
    final_fail: u8,
    new_spawn: u8,
    init_fail: u8,
    foreign: u8,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct State {
    sys: SysPhase,
    shutdown: Option<Shut>,
    next_tok: Tok,
    s_phase: SPhase,
    s_lane: Option<Reason>,
    s_unseen: bool,
    s_abort: bool,
    sup: Sup,
    /// The supervision state of every supervisor incarnation from the end of
    /// its `pre_start` until its task ends or drops it.
    psups: BTreeMap<Tok, Sup>,
    cells: BTreeMap<Tok, Cell>,
    incs: BTreeMap<Tok, Inc>,
    held: BTreeMap<IdKey, Tok>,
    foreign: BTreeSet<IdKey>,
    reaper: BTreeMap<Tok, Reap>,
    /// The deadline entries the final sweep armed, held by the system's
    /// reaper whatever becomes of the shutdown call; each holds whether its
    /// own deadline has passed.
    sweep: BTreeMap<Tok, SweepStage>,
    caller: Option<Caller>,
    left: Budgets,
    oracle: Oracle,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Strategy {
    OneForOne,
    OneForAll,
    RestForOne,
    Pool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Restart {
    Permanent,
    Transient,
    Temporary,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Policy {
    Kill,
    Timeout,
    Infinity,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Behavior {
    Cooperative,
    Vetoing,
    VetoOnce,
    ParentOnly,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Grace {
    Zero,
    Standard,
    /// A configured value in nanoseconds.
    Ns(u64),
}

/// An unbounded duration: a sum that reaches it stays there.
const INF: u64 = u64::MAX;
/// The wait after an abort before an aborted outcome or the verdict.
const ABORT_SETTLE: u64 = 100_000_000;
const DEFAULT_WINDOW: u64 = 5_000_000_000;

fn sat(a: u64, b: u64) -> u64 {
    a.saturating_add(b)
}

#[derive(Clone, Copy, Debug)]
struct ChildCfg {
    restart: Restart,
    policy: Policy,
    start_timeout: bool,
    behavior: Behavior,
    kill_grace: Grace,
    named: bool,
    turns: bool,
    may_stick: bool,
    /// The child is itself a supervisor of the configuration's workers.
    is_supervisor: bool,
    /// The window of a `Timeout` policy, in nanoseconds.
    window_ns: u64,
}

impl ChildCfg {
    const fn new() -> ChildCfg {
        ChildCfg {
            restart: Restart::Permanent,
            policy: Policy::Timeout,
            start_timeout: false,
            behavior: Behavior::Cooperative,
            kill_grace: Grace::Standard,
            named: true,
            turns: false,
            may_stick: false,
            is_supervisor: false,
            window_ns: DEFAULT_WINDOW,
        }
    }
    const fn supervisor(mut self) -> ChildCfg {
        self.is_supervisor = true;
        self
    }
    const fn window_ms(mut self, ms: u64) -> ChildCfg {
        self.window_ns = ms * 1_000_000;
        self
    }
    /// The kill grace, in nanoseconds.
    fn kg(&self) -> u64 {
        match self.kill_grace {
            Grace::Zero => 0,
            Grace::Standard => 120_000_000,
            Grace::Ns(n) => n,
        }
    }
    /// The window of the stop that bounds the child: zero for `Kill`.
    fn window(&self) -> u64 {
        match self.policy {
            Policy::Kill => 0,
            Policy::Timeout => self.window_ns,
            Policy::Infinity => INF,
        }
    }
    /// What a stop of the child may take: its window, its kill grace and the
    /// abort settle; unbounded under `Infinity`.
    fn stop_budget(&self) -> u64 {
        if self.policy == Policy::Infinity {
            INF
        } else {
            sat(sat(self.window(), self.kg()), ABORT_SETTLE)
        }
    }
    /// The time the child can outlive a killed parent: a worker its kill
    /// grace and the abort settle whatever its own policy, a supervisor child
    /// its whole stop budget.
    fn own_budget(&self) -> u64 {
        if self.is_supervisor {
            self.stop_budget()
        } else {
            sat(self.kg(), ABORT_SETTLE)
        }
    }
    /// As `own_budget`, with a finite stand-in for an unbounded supervisor
    /// child.
    fn own_fin(&self) -> u64 {
        if self.is_supervisor && self.policy == Policy::Infinity {
            sat(self.kg(), ABORT_SETTLE)
        } else {
            self.own_budget()
        }
    }
    const fn restart(mut self, r: Restart) -> ChildCfg {
        self.restart = r;
        self
    }
    const fn policy(mut self, p: Policy) -> ChildCfg {
        self.policy = p;
        self
    }
    const fn behavior(mut self, b: Behavior) -> ChildCfg {
        self.behavior = b;
        self
    }
    const fn start_timeout(mut self) -> ChildCfg {
        self.start_timeout = true;
        self
    }
    const fn grace(mut self, g: Grace) -> ChildCfg {
        self.kill_grace = g;
        self
    }
    const fn turns(mut self) -> ChildCfg {
        self.turns = true;
        self
    }
    const fn sticks(mut self) -> ChildCfg {
        self.may_stick = true;
        self
    }
}

#[derive(Clone, Debug)]
struct Cfg {
    name: &'static str,
    strategy: Strategy,
    budget_s: u8,
    kids: Vec<ChildCfg>,
    /// `kids[..initial]` are spawned at start, the rest are spare keys.
    initial: usize,
    budgets: Budgets,
    third_reasons: Vec<Reason>,
    limit: usize,
    /// The workers every supervisor incarnation supervises under
    /// `OneForOne`; empty when no child is a supervisor.
    workers: Vec<ChildCfg>,
    budget_p: u8,
    /// The keys of the supervisor under test's children that `BareKill` may
    /// target; none given means every key.
    bare_kill_on: Option<Vec<Key>>,
    /// Each incarnation records whether a `terminate_child` override gave
    /// its one more ask, so that the witness of a member accepting after an
    /// ask counts only an ask an override gave.
    mark_override_asks: bool,
}

// ---------------------------------------------------------------------------
// State accessors and the derived predicates (defined once, used everywhere)
// ---------------------------------------------------------------------------

impl Cfg {
    fn kid(&self, c: Key) -> &ChildCfg {
        &self.kids[c as usize]
    }

    fn is_pool(&self) -> bool {
        self.strategy == Strategy::Pool
    }

    /// A child whose end removes its spec: every pool child, and any child of
    /// restart type `Temporary`.
    fn prunable(&self, c: Key) -> bool {
        self.is_pool() || self.kid(c).restart == Restart::Temporary
    }

    fn first_signal(&self, c: Key) -> Reason {
        self.first_signal_of(SupId::S, c)
    }

    fn bare_kill_targets(&self, c: Key) -> bool {
        self.bare_kill_on.as_ref().is_none_or(|ks| ks.contains(&c))
    }

    /// The configuration of child `c` of supervisor `x`.
    fn kid_of(&self, x: SupId, c: Key) -> &ChildCfg {
        match x {
            SupId::S => &self.kids[c as usize],
            SupId::Inc(_) => &self.workers[c as usize],
        }
    }

    fn budget_of(&self, x: SupId) -> u8 {
        match x {
            SupId::S => self.budget_s,
            SupId::Inc(_) => self.budget_p,
        }
    }

    /// Only the supervisor under test can be a pool; every supervisor
    /// incarnation is `OneForOne`.
    fn strategy_of(&self, x: SupId) -> Strategy {
        match x {
            SupId::S => self.strategy,
            SupId::Inc(_) => Strategy::OneForOne,
        }
    }

    fn pool_of(&self, x: SupId) -> bool {
        x == SupId::S && self.is_pool()
    }

    fn prunable_of(&self, x: SupId, c: Key) -> bool {
        self.pool_of(x) || self.kid_of(x, c).restart == Restart::Temporary
    }

    fn first_signal_of(&self, x: SupId, c: Key) -> Reason {
        match self.kid_of(x, c).policy {
            Policy::Kill => Reason::Kill,
            Policy::Timeout | Policy::Infinity => Reason::ParentRequest,
        }
    }
}

fn restart_eligible(restart: Restart, r: Reason) -> bool {
    match restart {
        Restart::Permanent => true,
        Restart::Transient => !matches!(
            r,
            Reason::Graceful | Reason::ParentRequest | Reason::ParentKilled
        ),
        Restart::Temporary => false,
    }
}

/// The restart rule the checks hold the model to, stated apart from the
/// model's own: a `Transient` child restarts after a crash, a cancellation
/// or a kill, and stays down after a stop ordered from above (a graceful
/// one, its parent's request or an inherited one).
fn restart_expected(restart: Restart, r: Reason) -> bool {
    match restart {
        Restart::Permanent => true,
        Restart::Temporary => false,
        Restart::Transient => matches!(r, Reason::Failure(_) | Reason::Cancelled | Reason::Kill),
    }
}

impl Phase {
    fn restart_set_contains(&self, c: Key) -> bool {
        match self {
            Phase::Stop { restart_order, .. } => restart_order.contains(&c),
            Phase::Chain { queue, .. } => queue.contains(&c),
        }
    }
}

impl State {
    fn life(&self, c: Key) -> Option<Life> {
        self.sup.ledger.get(&c).copied()
    }

    /// The supervision state of `x`; it must exist.
    fn sup_ref(&self, x: SupId) -> &Sup {
        match x {
            SupId::S => &self.sup,
            SupId::Inc(p) => self
                .psups
                .get(&p)
                .expect("model invariant: the supervision state exists"),
        }
    }

    fn sup_mut(&mut self, x: SupId) -> &mut Sup {
        match x {
            SupId::S => &mut self.sup,
            SupId::Inc(p) => self
                .psups
                .get_mut(&p)
                .expect("model invariant: the supervision state exists"),
        }
    }

    /// Whether `x`'s supervision state exists: S until it is done, a
    /// supervisor incarnation from the end of its `pre_start` until its task
    /// ends or drops it.
    fn has_sup(&self, x: SupId) -> bool {
        match x {
            SupId::S => self.sup_alive(),
            SupId::Inc(p) => self.psups.contains_key(&p),
        }
    }

    fn life_of(&self, x: SupId, c: Key) -> Option<Life> {
        self.sup_ref(x).ledger.get(&c).copied()
    }

    fn occ_of(&self, x: SupId, c: Key) -> Option<Tok> {
        match self.life_of(x, c)? {
            Life::Running(n) | Life::Stopping { n, .. } => Some(n),
            Life::Restarting { next, .. } => Some(next),
            Life::Down { .. } => None,
        }
    }

    /// `x`'s loop has begun and has not ended: reports that enter its planes
    /// now are owed.
    fn x_in_loop(&self, x: SupId) -> bool {
        match x {
            SupId::S => self.s_phase == SPhase::Loop,
            SupId::Inc(p) => self.incs.get(&p).is_some_and(|i| {
                i.acked
                    && !matches!(
                        i.stage,
                        Stage::Final | Stage::Hook | Stage::Teardown | Stage::Done
                    )
            }),
        }
    }

    /// `x` is delivering the reports its final drain collected.
    fn x_final(&self, x: SupId) -> bool {
        match x {
            SupId::S => matches!(self.s_phase, SPhase::Final(_)),
            SupId::Inc(p) => self.incs.get(&p).is_some_and(|i| i.stage == Stage::Final),
        }
    }

    /// `x` is stopping its children or past that: no restart, strategy or
    /// delivery may happen for it.
    fn x_torn_down(&self, x: SupId) -> bool {
        if !self.has_sup(x) {
            return true;
        }
        self.sup_ref(x).teardown.is_some()
            || match x {
                SupId::S => matches!(
                    self.s_phase,
                    SPhase::Hook(_) | SPhase::Teardown | SPhase::Done(_)
                ),
                SupId::Inc(p) => self
                    .incs
                    .get(&p)
                    .is_none_or(|i| matches!(i.stage, Stage::Hook | Stage::Teardown | Stage::Done)),
            }
    }

    /// The supervisor is inside its loop at a turn boundary with no waiting
    /// caller and no unseen request.
    fn loop_free_of(&self, x: SupId) -> bool {
        match x {
            SupId::S => self.loop_free(),
            SupId::Inc(p) => self
                .incs
                .get(&p)
                .is_some_and(|i| i.stage == Stage::Idle && !i.unseen),
        }
    }

    /// The pairing gate of supervisor incarnation `t`: closed while its
    /// teardown, which its window covers, still has a key to stop.
    fn gate_open(&self, t: Tok) -> bool {
        let x = SupId::Inc(t);
        if !self.has_sup(x) {
            return true;
        }
        match &self.sup_ref(x).teardown {
            Some(d) if d.covers => {
                d.walk.is_empty()
                    && d.current.is_none_or(|(_, u)| self.settled(u))
                    && d.pool_wait.iter().all(|(_, u)| self.settled(*u))
            }
            _ => true,
        }
    }

    /// Whether a Kill-stage step on `t` may fire: a supervisor incarnation's
    /// pairing gate; every other incarnation has none.
    fn gate_open_for(&self, t: Tok) -> bool {
        self.gate_open(t)
    }

    fn drain_enabled_of(&self, x: SupId) -> bool {
        self.has_sup(x) && (self.loop_free_of(x) || self.sup_ref(x).teardown.is_some())
    }

    /// The occupant of a key: the token of a `Running` or `Stopping` life, the
    /// attempt of a `Restarting` life, nothing for a `Down` life.
    fn occ(&self, c: Key) -> Option<Tok> {
        match self.life(c)? {
            Life::Running(n) | Life::Stopping { n, .. } => Some(n),
            Life::Restarting { next, .. } => Some(next),
            Life::Down { .. } => None,
        }
    }

    fn inc(&self, t: Tok) -> &Inc {
        self.incs
            .get(&t)
            .expect("model invariant: incarnation exists")
    }

    fn cell(&self, t: Tok) -> &Cell {
        self.cells.get(&t).expect("model invariant: cell exists")
    }

    fn live(&self, t: Tok) -> bool {
        self.incs.get(&t).is_some_and(|i| i.end.is_none())
    }

    fn spawned(&self, t: Tok) -> bool {
        self.incs.contains_key(&t)
    }

    /// The live incarnations among `t` and its descendants.
    fn subtree_live(&self, t: Tok) -> Vec<Tok> {
        let mut out = Vec::new();
        if self.live(t) {
            out.push(t);
        }
        for (&u, i) in &self.incs {
            if i.parent == Some(t) {
                out.extend(self.subtree_live(u));
            }
        }
        out
    }

    /// Some live descendant of `t` (not `t` itself) is a supervisor child
    /// under `Infinity`; none exists at depth two, where every descendant of
    /// a child of the supervisor under test is a worker.
    fn inf_live(&self, cfg: &Cfg, t: Tok) -> bool {
        self.subtree_live(t).into_iter().any(|u| {
            u != t && {
                let i = self.inc(u);
                let kid = cfg.kid_of(i.sup, i.key);
                kid.is_supervisor && kid.policy == Policy::Infinity
            }
        })
    }

    /// The bound a wait on the subtree of `t` carries.
    fn release_bound(&self, cfg: &Cfg, t: Tok) -> u64 {
        if self.inf_live(cfg, t) {
            INF
        } else {
            sat(self.incs.get(&t).map_or(0, |i| i.below), ABORT_SETTLE)
        }
    }

    /// A bounded subtree wait on `t` may expire: its bound is finite and the
    /// subtree holds an incarnation that never yields.
    fn expirable(&self, cfg: &Cfg, t: Tok) -> bool {
        let live = self.subtree_live(t);
        self.release_bound(cfg, t) != INF
            && live
                .iter()
                .any(|u| matches!(self.inc(*u).stage, Stage::Stuck(_)))
            && !live.iter().any(|u| self.abort_owed(*u))
    }

    /// An abort is owed to the live incarnation `u` and none was issued yet:
    /// a backstop the system holds on it, a sweep entry, or a ladder that
    /// raised its `Kill`. No verdict on a wait that covers `u` comes before
    /// that abort.
    ///
    /// The model ends a supervisor and hands its children to their guards in
    /// one step (`drop_supervision`), so a subtree never reads as neither
    /// owed nor aborted between an owed member's end and the guards of its
    /// children. The implementation approximates that step with the settle
    /// time: the end of an owed member is raised as the subtree's latest
    /// abort before its mark drops, and an abort that finds its task already
    /// finished releases the mark the same way, so no verdict comes within
    /// the settle time after the end; a host stall longer than that inside
    /// the cascade is the residual the model does not show.
    fn abort_owed(&self, u: Tok) -> bool {
        if !self.live(u) || self.inc(u).aborted {
            return false;
        }
        self.reaper.contains_key(&u)
            || self.sweep.contains_key(&u)
            || self.cells.get(&u).is_some_and(|c| {
                c.ladder == Ladder::KillGrace
                    && matches!(c.exec, Exec::Runner | Exec::Attempt(AttemptStage::Init))
            })
    }

    /// The incarnation and its whole subtree ended; a token that never
    /// spawned is gone.
    fn gone(&self, t: Tok) -> bool {
        gone_in(&self.incs, t)
    }

    /// The supervisor can see that the task of `t` ended: its fate is written,
    /// or its runner failed or its supervision state was dropped and the task
    /// has since ended (the task's own end is then its fate).
    fn ended(&self, t: Tok) -> bool {
        match (self.cells.get(&t), self.incs.get(&t)) {
            (Some(c), Some(i)) => {
                c.fate.is_some()
                    || (matches!(c.exec, Exec::RunnerFailed | Exec::Orphan) && i.end.is_some())
            }
            (Some(c), None) => c.fate.is_some(),
            _ => false,
        }
    }

    /// How the incarnation actually ended, as its report carries it.
    fn seen_reason(&self, t: Tok) -> Reason {
        self.cells
            .get(&t)
            .and_then(|c| c.fate)
            .or_else(|| self.incs.get(&t).and_then(|i| i.end))
            .expect("model invariant: an ended incarnation has a reason")
    }

    /// A fate or the verdict. A token that never spawned is settled by the
    /// return of its attempt.
    fn settled(&self, t: Tok) -> bool {
        if self.spawned(t) {
            self.ended(t)
                || self.cells.get(&t).is_some_and(|c| c.verdict)
                || self.orphan_abort_taken(t)
        } else {
            self.cells.get(&t).is_none_or(|c| c.cancelled || c.closed)
        }
    }

    /// An orphan whose guard's abort was taken: with its supervisor gone and
    /// its cell closed nothing can write a verdict, and a task that yields has
    /// already ended, so the abort taken stands for the verdict.
    fn orphan_abort_taken(&self, t: Tok) -> bool {
        self.cells.get(&t).is_some_and(|c| c.exec == Exec::Orphan)
            && self.incs.get(&t).is_some_and(|i| i.aborted)
    }

    /// Whether the supervisor's own wait on `t` may end: the logic's reading
    /// of `settled`.
    fn settled_for_wait(&self, t: Tok) -> bool {
        self.settled(t)
    }

    fn loop_free(&self) -> bool {
        self.s_phase == SPhase::Loop && self.caller.is_none() && !self.s_unseen
    }

    fn drain_enabled(&self) -> bool {
        self.loop_free() || self.sup.teardown.is_some()
    }

    fn sup_alive(&self) -> bool {
        !matches!(self.s_phase, SPhase::Done(_))
    }

    /// The awaited end of `c` was classified inline and its report is still
    /// undrained: the barrier entry stays recognizable by the pending snapshot.
    fn handled(&self, c: Key) -> bool {
        match self.sup.group.as_ref().map(|g| &g.phase) {
            Some(Phase::Stop { awaiting, .. }) => awaiting
                .get(&c)
                .is_some_and(|t| self.sup.snapshots.contains_key(&(c, *t))),
            _ => false,
        }
    }

    fn membership(&self, c: Key) -> Memb {
        let Some(g) = &self.sup.group else {
            return Memb::Outside;
        };
        if let Phase::Stop { awaiting, .. } = &g.phase {
            if awaiting.contains_key(&c) && !self.handled(c) {
                return Memb::Awaited;
            }
        }
        if g.phase.restart_set_contains(c) {
            return Memb::Queued;
        }
        if g.overrides.contains(&c) {
            return Memb::Released;
        }
        Memb::Outside
    }

    /// Whether the pending group restart will restart the key: it is in the
    /// stop phase's restart order or the chain's queue, and not released.
    fn in_restart_set(&self, c: Key) -> bool {
        self.sup
            .group
            .as_ref()
            .is_some_and(|g| g.phase.restart_set_contains(c) && !g.overrides.contains(&c))
    }

    /// Group membership of a key of `x`: only the supervisor under test has
    /// a group.
    fn membership_of(&self, x: SupId, c: Key) -> Memb {
        if x == SupId::S {
            self.membership(c)
        } else {
            Memb::Outside
        }
    }

    /// `is_alive` and `restart_pending` of `children()`.
    fn child_info_flags(&self, c: Key) -> (bool, bool) {
        match self.life(c) {
            None => (false, false),
            Some(Life::Running(n)) => {
                let ended = self.ended(n);
                (!ended, ended)
            }
            Some(Life::Stopping { n, .. }) => (!self.ended(n), false),
            Some(Life::Restarting { .. }) => (false, true),
            Some(Life::Down { pend, .. }) => {
                (false, pend == Pend::Evaluate || self.in_restart_set(c))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Step application: the shared procedures
// ---------------------------------------------------------------------------

/// How a delivery was decided, for the check that the action it carries is
/// true when read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Delivery {
    /// Decided in the loop; whether it ran the budget out is read from the
    /// oracle's count of this report.
    Decision,
    /// The report of a manual completion, delivered from its snapshot.
    Snapshot,
    /// A final delivery, made after the loop ended.
    Final,
}

/// One application of a step to a copy of a state: the logic mutates `s`, the
/// harness parts record failures and witnesses.
struct Ap<'a> {
    cfg: &'a Cfg,
    pre: &'a State,
    s: State,
    fails: Vec<&'static str>,
    wit: Vec<Wit>,
    answer: Option<Answer>,
    /// Reports of a `complete_group` being routed in this step, oldest first.
    worklist: VecDeque<(Key, Tok, Reason)>,
    in_worklist: bool,
    /// Reports a final collection took from a worklist, for the witness.
    collected_from_queue: bool,
    /// Every request to stop an incarnation made in this step, accepted or not.
    raised: Vec<(Tok, Reason)>,
    /// The decisions of this step that production computes with a `rules`
    /// function, for the conformance checks to replay.
    obs: Vec<Obs>,
}

impl<'a> Ap<'a> {
    fn new(cfg: &'a Cfg, pre: &'a State) -> Ap<'a> {
        Ap {
            cfg,
            pre,
            s: pre.clone(),
            fails: Vec::new(),
            wit: Vec::new(),
            answer: None,
            worklist: VecDeque::new(),
            in_worklist: false,
            collected_from_queue: false,
            raised: Vec::new(),
            obs: Vec::new(),
        }
    }

    fn fail(&mut self, name: &'static str) {
        self.fails.push(name);
    }

    fn hit(&mut self, w: Wit) {
        self.wit.push(w);
    }

    fn cfg_of(&self, t: Tok) -> &ChildCfg {
        let c = self.s.cell(t);
        self.cfg.kid_of(c.sup, c.key)
    }

    // ---- lanes -----------------------------------------------------------

    /// A request to stop incarnation `t`, with the lane's acceptance rules and
    /// the checks that the lane never weakens and that a raised Kill lands.
    fn raise(&mut self, t: Tok, r: Reason) {
        self.raised.push((t, r));
        let Some(inc) = self.s.incs.get(&t) else {
            return;
        };
        let live = inc.end.is_none();
        let mode = inc.lane_mode;
        let old = inc.lane;
        let open = live
            && !(mode == LaneMode::Sealed || (mode == LaneMode::KillOnly && r != Reason::Kill));
        // Facts recomputed from the old lane, independent of the logic below.
        let accepted = open && old.is_none_or(|cur| strength(r) >= strength(cur));
        let in_loop = !matches!(
            self.s.inc(t).stage,
            Stage::Final | Stage::Hook | Stage::Teardown | Stage::Done
        );
        if accepted && in_loop && self.s.cells.contains_key(&t) && self.cfg_of(t).is_supervisor {
            let held = self.s.oracle.p_req.get(&t).copied();
            if strength(r) != Strength::Requested {
                self.s.oracle.p_req.remove(&t);
            } else if held.is_none_or(|h| strength(r) > strength(h) || rank(r) >= rank(h)) {
                self.s.oracle.p_req.insert(t, r);
            }
        }
        self.raise_logic(t, r);
        let inc = self.s.inc(t);
        let unseen = inc.unseen;
        let lane = inc.lane;
        let answer = if !open {
            LaneAnswer::Refused
        } else if old.is_some_and(|cur| strength(r) < strength(cur)) {
            LaneAnswer::Superseded
        } else {
            LaneAnswer::Recorded
        };
        self.obs.push(Obs::Lane {
            stored: old,
            new: r,
            mode,
            receiver_gone: !live,
            answer,
            after: lane,
        });
        if accepted && !unseen {
            self.fail(STOP_REQUESTS_NEVER_WEAKEN);
        }
        if r == Reason::Kill && open && lane != Some(Reason::Kill) {
            self.fail(KILL_IS_FINAL);
        }
    }

    fn raise_logic(&mut self, t: Tok, r: Reason) {
        let inc = self
            .s
            .incs
            .get_mut(&t)
            .expect("model invariant: raise target exists");
        if inc.end.is_some() {
            return;
        }
        match inc.lane_mode {
            LaneMode::Sealed => return,
            LaneMode::KillOnly if r != Reason::Kill => return,
            _ => {}
        }
        match inc.lane {
            None => inc.lane = Some(r),
            Some(cur) => {
                if strength(r) < strength(cur) {
                    return;
                }
                if strength(r) > strength(cur) || rank(r) >= rank(cur) {
                    inc.lane = Some(r);
                }
            }
        }
        inc.unseen = true;
    }

    /// The supervisor's own lane, written by its parent and the system.
    fn s_raise(&mut self, r: Reason) {
        let closed = matches!(self.s.s_phase, SPhase::Done(_))
            || (self.s.s_phase == SPhase::Teardown && r != Reason::Kill);
        let open = !closed;
        let accepted = open && self.s.s_lane.is_none_or(|cur| strength(r) >= strength(cur));
        let stored = self.s.s_lane;
        let mode = match self.s.s_phase {
            SPhase::Done(_) => LaneMode::Sealed,
            SPhase::Teardown => LaneMode::KillOnly,
            _ => LaneMode::Open,
        };
        self.s_raise_logic(r);
        let answer = if !open {
            LaneAnswer::Refused
        } else if stored.is_some_and(|cur| strength(r) < strength(cur)) {
            LaneAnswer::Superseded
        } else {
            LaneAnswer::Recorded
        };
        self.obs.push(Obs::Lane {
            stored,
            new: r,
            mode,
            receiver_gone: false,
            answer,
            after: self.s.s_lane,
        });
        if accepted && !self.s.s_unseen {
            self.fail(STOP_REQUESTS_NEVER_WEAKEN);
        }
        if r == Reason::Kill && open && self.s.s_lane != Some(Reason::Kill) {
            self.fail(KILL_IS_FINAL);
        }
    }

    fn s_raise_logic(&mut self, r: Reason) {
        if matches!(self.s.s_phase, SPhase::Done(_)) {
            return;
        }
        if self.s.s_phase == SPhase::Teardown && r != Reason::Kill {
            return;
        }
        match self.s.s_lane {
            None => self.s.s_lane = Some(r),
            Some(cur) => {
                if strength(r) < strength(cur) {
                    return;
                }
                if strength(r) > strength(cur) || rank(r) >= rank(cur) {
                    self.s.s_lane = Some(r);
                }
            }
        }
        self.s.s_unseen = true;
    }

    // ---- stop orders and ladders ----------------------------------------

    /// Raise the first signal of an order whose executor runs a ladder, and
    /// start the ladder unless one is already past its start.
    fn arm(&mut self, t: Tok) {
        if !self.s.live(t) {
            return;
        }
        let exec = self.s.cell(t).exec;
        if !matches!(exec, Exec::Runner | Exec::Attempt(AttemptStage::Init)) {
            return;
        }
        let (owner, key) = (self.s.cell(t).sup, self.s.cell(t).key);
        if exec == Exec::Attempt(AttemptStage::Init) {
            self.obs.push(Obs::Attempt {
                stage: AtStage::Init,
                event: AtEvent::Order,
                ladder: self.s.cell(t).ladder,
                kill_stored: self.s.inc(t).lane == Some(Reason::Kill),
                acked: self.s.inc(t).acked,
                want: AtWant::ArmLadder,
            });
        }
        let raised = self.s.cell(t).order.is_some_and(|o| o.raised);
        if !raised {
            let first = self.cfg.first_signal_of(owner, key);
            self.raise(t, first);
            if let Some(o) = self.s.cells.get_mut(&t).and_then(|c| c.order.as_mut()) {
                o.raised = true;
            }
        }
        let policy = self.cfg.kid_of(owner, key).policy;
        let cell = self.s.cells.get_mut(&t).expect("model invariant: cell");
        if cell.ladder == Ladder::Off {
            cell.ladder = match policy {
                Policy::Kill => Ladder::KillGrace,
                Policy::Timeout => Ladder::Window,
                Policy::Infinity => Ladder::WindowInf,
            };
        }
    }

    /// Write a stop order into the cell of `t`; the first order wins and keeps
    /// its clock.
    fn order(&mut self, t: Tok, from: OrderFrom) {
        if self.s.cell(t).order.is_some() {
            return;
        }
        let exec = self.s.cell(t).exec;
        match exec {
            Exec::Attempt(AttemptStage::Wait) => self.hit(Wit::AttemptOrderedWait),
            Exec::Attempt(AttemptStage::Factory) => self.hit(Wit::AttemptOrderedFactory),
            Exec::Attempt(AttemptStage::Init) => self.hit(Wit::AttemptOrderedInit),
            Exec::Attempt(AttemptStage::InTransit) => self.hit(Wit::AttemptOrderedInTransit),
            _ => {}
        }
        self.s
            .cells
            .get_mut(&t)
            .expect("model invariant: cell")
            .order = Some(Order {
            from,
            raised: false,
        });
        if matches!(exec, Exec::Runner | Exec::Attempt(AttemptStage::Init)) && self.s.live(t) {
            self.arm(t);
        }
    }

    /// The system takes over the stop of a live incarnation whose supervision
    /// state was dropped: a Kill and an armed abort.
    fn fire_guard(&mut self, t: Tok, exec: Exec) {
        if !self.s.live(t) {
            return;
        }
        let kid = self.cfg_of(t);
        let (supervisor, policy) = (kid.is_supervisor, kid.policy);
        let (window_ns, kill_grace_ns) = (kid.window_ns, kid.kg());
        if supervisor
            && self.s.cell(t).sup == SupId::S
            && self
                .s
                .shutdown
                .is_some_and(|sh| sh.step == ShutStep::KillRaised)
        {
            self.hit(Wit::ShutdownKillReachesSupervisorChild);
        }
        let kill_stored = self.s.inc(t).lane == Some(Reason::Kill);
        let raised = if supervisor && kill_stored {
            // A `Kill` the lane already holds keeps the abort it is owed,
            // whatever the child's policy.
            self.hit(match (self.s.reaper.contains_key(&t), policy) {
                (true, Policy::Infinity) => Wit::GuardKeepsBareKillAbortUnderInfinity,
                (true, _) => Wit::GuardKeepsBareKillAbortUnderBoundedPolicy,
                (false, Policy::Infinity) => Wit::GuardKeepsLadderKillAbortUnderInfinity,
                (false, _) => Wit::GuardKeepsLadderKillAbortUnderBoundedPolicy,
            });
            self.raise(t, Reason::ParentKilled);
            self.s.reaper.entry(t).or_insert(Reap::AbortDue);
            Reason::Kill
        } else if supervisor {
            // An orderly inherited stop, bounded by the child's own window:
            // its Kill stage is due at the window, zero under `Kill`.
            self.raise(t, Reason::ParentKilled);
            if policy != Policy::Infinity {
                self.s.reaper.insert(t, Reap::KillThenAbort);
            }
            Reason::ParentKilled
        } else {
            self.raise(t, Reason::Kill);
            self.s.reaper.entry(t).or_insert(Reap::AbortDue);
            Reason::Kill
        };
        self.obs.push(Obs::Guard {
            is_supervisor: supervisor,
            policy,
            window_ns,
            kill_grace_ns,
            raised,
            entry: self.s.reaper.get(&t).copied(),
            kill_stored,
        });
        self.s
            .cells
            .get_mut(&t)
            .expect("model invariant: cell")
            .exec = exec;
    }

    /// Cancel a task at its next await: it ends as `Kill`, unless it never
    /// yields.
    fn abort(&mut self, t: Tok) {
        if !self.s.live(t) {
            return;
        }
        self.s
            .incs
            .get_mut(&t)
            .expect("model invariant: inc")
            .aborted = true;
        if !matches!(self.s.inc(t).stage, Stage::Stuck(_)) {
            if self.s.has_sup(SupId::Inc(t)) {
                self.drop_supervision(SupId::Inc(t));
            }
            self.end(t, Reason::Kill);
        }
    }

    // ---- ending an incarnation ---------------------------------------------

    fn put_death(&mut self, x: SupId, c: Key, t: Tok, r: Reason) {
        if !self.s.has_sup(x) {
            return;
        }
        self.s.sup_mut(x).deaths.insert((c, t, r));
        if self.s.x_in_loop(x) {
            self.s
                .cells
                .get_mut(&t)
                .expect("model invariant: cell")
                .owed = true;
        }
    }

    fn put_outcome(&mut self, x: SupId, m: Tok, c: Key, o: Outcome) {
        if !self.s.has_sup(x) {
            return;
        }
        self.s.sup_mut(x).outcomes.insert(m, (c, o));
        if self.s.x_in_loop(x) && matches!(o, Outcome::Failed(_) | Outcome::Stopped(_)) {
            self.s
                .cells
                .get_mut(&m)
                .expect("model invariant: cell")
                .owed = true;
        }
    }

    /// The task of `t` ends with reason `r`.
    fn end(&mut self, t: Tok, r: Reason) {
        assert!(self.s.live(t), "model invariant: end of a live incarnation");
        let mut r = r;
        let (key, aborted, lane, acked) = {
            let i = self.s.inc(t);
            (i.key, i.aborted, i.lane, i.acked)
        };
        let x = self.s.cell(t).sup;
        if !aborted && lane == Some(Reason::Kill) && r != Reason::Kill {
            r = Reason::Kill;
        }
        if lane == Some(Reason::Kill) && r != Reason::Kill {
            self.fail(KILL_IS_FINAL);
        }
        {
            let i = self.s.incs.get_mut(&t).expect("model invariant: inc");
            i.status = if aborted {
                Status::Closed
            } else {
                Status::Stopped
            };
            i.lane_mode = LaneMode::Sealed;
            i.stage = Stage::Done;
            i.end = Some(r);
        }
        self.s
            .cells
            .get_mut(&t)
            .expect("model invariant: cell")
            .ladder = Ladder::Off;
        self.s.held.retain(|_, v| *v != t);
        let (exec, order, st_fired) = {
            let c = self.s.cell(t);
            (c.exec, c.order, c.st_fired)
        };
        match exec {
            Exec::Runner => {
                self.s
                    .cells
                    .get_mut(&t)
                    .expect("model invariant: cell")
                    .fate = Some(r);
                self.put_death(x, key, t, r);
            }
            Exec::RunnerFailed => self.put_death(x, key, t, r),
            Exec::Attempt(AttemptStage::Init) => {
                let ladder = self.s.cell(t).ladder;
                // Harness: the ladder was cleared above; the attempt read it
                // before the incarnation ended.
                let ladder_before = self.pre.cells.get(&t).map_or(ladder, |c| c.ladder);
                let kill_stored = lane == Some(Reason::Kill);
                if acked && order.is_none() && !st_fired && ladder_before == Ladder::Off {
                    self.obs.push(Obs::Attempt {
                        stage: AtStage::Init,
                        event: AtEvent::Ack {
                            ordered: false,
                            ended: true,
                        },
                        ladder: ladder_before,
                        kill_stored,
                        acked: true,
                        want: AtWant::Adopt,
                    });
                    self.hit(Wit::StartAckedThenDied);
                    self.put_outcome(x, t, key, Outcome::Adopted);
                    self.s
                        .cells
                        .get_mut(&t)
                        .expect("model invariant: cell")
                        .exec = Exec::Attempt(AttemptStage::InTransit);
                } else {
                    let outcome = if st_fired {
                        Outcome::Failed(Reason::Failure(Cause::StartTimeout))
                    } else if order.is_some() {
                        Outcome::Stopped(r)
                    } else {
                        Outcome::Failed(r)
                    };
                    self.obs.push(Obs::Attempt {
                        stage: AtStage::Init,
                        event: AtEvent::Ended {
                            ordered: order.is_some(),
                            timeout_raised: st_fired,
                        },
                        ladder: ladder_before,
                        kill_stored,
                        acked,
                        want: AtWant::Report(if st_fired {
                            AtReport::StartTimeout
                        } else if order.is_some() {
                            AtReport::Stopped
                        } else {
                            AtReport::Failed
                        }),
                    });
                    {
                        let c = self.s.cells.get_mut(&t).expect("model invariant: cell");
                        c.fate = Some(r);
                        c.exec = Exec::Returned;
                    }
                    self.put_outcome(x, t, key, outcome);
                }
            }
            Exec::Attempt(AttemptStage::InTransit | AttemptStage::Factory | AttemptStage::Wait)
            | Exec::Orphan
            | Exec::Returned => {}
        }
    }

    /// The closing point of an incarnation's run loop.
    fn close(&mut self, t: Tok) {
        {
            let i = self.s.incs.get_mut(&t).expect("model invariant: inc");
            if i.stop_reason != Some(Reason::Kill) && i.lane == Some(Reason::Kill) {
                i.stop_reason = Some(Reason::Kill);
            }
            i.lane_mode = LaneMode::KillOnly;
        }
        let r = self
            .s
            .inc(t)
            .stop_reason
            .expect("model invariant: stop reason at close");
        let x = SupId::Inc(t);
        if !self.s.has_sup(x) {
            self.end(t, r);
        } else if r == Reason::Kill {
            // The inversion: the supervisor's task ends without awaiting its
            // children, which go to their guards.
            self.drop_supervision(x);
            self.end(t, Reason::Kill);
        } else {
            self.begin_teardown(x, r);
            self.s.incs.get_mut(&t).expect("model invariant: inc").stage = Stage::Teardown;
        }
    }

    // ---- the supervisor's ledger -----------------------------------------

    fn legal(&self, old: Option<Life>, new: Option<Life>) -> bool {
        let drawn = self.pre.next_tok;
        match (old, new) {
            (None, Some(Life::Running(t))) => t >= drawn,
            (Some(o), None) => match o {
                Life::Running(n) | Life::Stopping { n, .. } => !self.s.live(n),
                Life::Restarting { next, .. } => !self.s.live(next),
                Life::Down { .. } => true,
            },
            (Some(Life::Running(n)), Some(Life::Stopping { n: m, .. })) => n == m,
            (Some(Life::Running(n)), Some(Life::Down { n: m, .. })) => n == m,
            (Some(Life::Stopping { n, .. }), Some(Life::Down { n: m, .. })) => n == m,
            (
                Some(Life::Stopping {
                    n,
                    kind: Kind::Bounce,
                }),
                Some(Life::Stopping {
                    n: m,
                    kind: Kind::Terminate,
                }),
            ) => n == m,
            (Some(Life::Restarting { next, .. }), Some(Life::Running(m))) => next == m,
            (Some(Life::Restarting { next, .. }), Some(Life::Down { n: m, .. })) => next == m,
            (Some(Life::Down { n, .. }), Some(Life::Restarting { prev, next })) => {
                n == prev && next >= drawn
            }
            (Some(Life::Down { n, .. }), Some(Life::Down { n: m, .. })) => n == m,
            _ => false,
        }
    }

    /// Every write of a ledger life goes through here, creation and removal
    /// included.
    fn set_life(&mut self, x: SupId, c: Key, new: Option<Life>) {
        let old = self.s.sup_ref(x).ledger.get(&c).copied();
        let legal = self.legal(old, new);
        if let (Some(from), Some(to)) = (old, new) {
            self.obs.push(Obs::Transition { from, to, legal });
        }
        if !legal {
            self.fail(EVERY_TRANSITION_IS_IN_THE_TABLE);
        }
        match new {
            Some(l) => {
                if let (None, Life::Running(t)) = (old, l) {
                    self.s.sup_mut(x).pred.insert(c, t);
                }
                self.s.sup_mut(x).ledger.insert(c, l);
            }
            None => {
                self.s.sup_mut(x).ledger.remove(&c);
                self.s.sup_mut(x).pred.remove(&c);
            }
        }
    }

    fn down(&mut self, x: SupId, c: Key, n: Tok, pend: Pend) {
        self.set_life(x, c, Some(Life::Down { n, pend }));
    }

    /// Remove a child's spec: the key leaves the ledger and the start order,
    /// and a pending group forgets it.
    fn remove_spec(&mut self, x: SupId, c: Key) {
        // The subtree of the key's last incarnation is recorded while it runs.
        if let Some(t) = self.s.sup_ref(x).pred.get(&c).copied() {
            if !self.s.gone(t) {
                self.s.sup_mut(x).retired.insert(c, t);
            }
        }
        self.set_life(x, c, None);
        self.s.sup_mut(x).order.retain(|k| *k != c);
        if let Some(g) = self.s.sup_mut(x).group.as_mut() {
            g.overrides.remove(&c);
            g.restarted.remove(&c);
            g.outside.remove(&c);
            // Harness: a removed spec leaves its group; a same-id child is new.
            self.s.oracle.removed.insert(c);
            self.s.oracle.group_log.remove(&c);
            self.s.oracle.group_outside.remove(&c);
        }
    }

    /// Start a restart attempt for a down child.
    fn initiate(&mut self, x: SupId, c: Key, origin: Origin) {
        if self.s.x_torn_down(x) || self.s.x_final(x) {
            self.fail(NO_RESTART_STRATEGY_OR_DELIVERY_DURING_TEARDOWN);
        }
        let Some(Life::Down { n, .. }) = self.s.life_of(x, c) else {
            panic!("model invariant: initiate needs a down child");
        };
        // Harness checks on a chain start.
        if origin == Origin::Chain {
            self.check_chain_initiate(c);
        }
        let m = self.s.next_tok;
        self.s.next_tok += 1;
        self.set_life(x, c, Some(Life::Restarting { prev: n, next: m }));
        // The successor waits for its predecessor's subtree before its factory.
        let waits_on = self
            .s
            .sup_ref(x)
            .pred
            .get(&c)
            .copied()
            .filter(|t| !self.s.gone(*t));
        let mut cell = new_cell(
            x,
            c,
            origin,
            Exec::Attempt(if waits_on.is_some() {
                AttemptStage::Wait
            } else {
                AttemptStage::Factory
            }),
        );
        cell.waits_on = waits_on;
        self.s.cells.insert(m, cell);
    }

    fn charge(&mut self, x: SupId) -> bool {
        if self.s.x_torn_down(x) || self.s.x_final(x) {
            self.fail(NO_RESTART_STRATEGY_OR_DELIVERY_DURING_TEARDOWN);
        }
        if self.s.sup_ref(x).charges < self.cfg.budget_of(x) {
            self.s.sup_mut(x).charges += 1;
            true
        } else {
            false
        }
    }

    /// The supervisor's `on_child_stopped` runs with `(c, r, a)`.
    fn deliver(&mut self, x: SupId, c: Key, t: Tok, r: Reason, a: Action, how: Delivery) {
        if self.s.x_in_loop(x) {
        } else if self.s.x_final(x) {
            if a != Action::SupervisorStopping {
                self.fail(NO_RESTART_STRATEGY_OR_DELIVERY_DURING_TEARDOWN);
            }
        } else {
            self.fail(NO_RESTART_STRATEGY_OR_DELIVERY_DURING_TEARDOWN);
        }
        if self.s.sup_ref(x).teardown.is_some() {
            self.fail(NO_RESTART_STRATEGY_OR_DELIVERY_DURING_TEARDOWN);
        }
        let already = self.s.cells.get(&t).is_some_and(|x| x.delivered);
        if already {
            self.fail(NO_INCARNATION_DELIVERED_TWICE);
        }
        if self.s.expected_reason(self.cfg, t) != Some(r) {
            self.fail(REPORTS_CARRY_THE_REAL_REASON);
        }
        let expected_action = match how {
            Delivery::Final => Some(Action::SupervisorStopping),
            Delivery::Snapshot => self.s.oracle.decided.get(&(c, t)).copied(),
            Delivery::Decision => Some(if self.s.oracle.charges_of(x) > self.cfg.budget_of(x) {
                Action::BudgetExhausted
            } else if self.s.life_of(x, c).is_some()
                && (matches!(self.s.life_of(x, c), Some(Life::Restarting { .. }))
                    || self.s.o_restarts_of(self.cfg, x, c))
            {
                Action::RestartInitiated
            } else {
                Action::Removed
            }),
        };
        if expected_action != Some(a) {
            self.fail(REPORTS_CARRY_THE_REAL_REASON);
        }
        // A `Temporary` member's end in a group is reported `Removed`, with its
        // spec removed.
        if matches!(how, Delivery::Decision)
            && self.cfg.kid_of(x, c).restart == Restart::Temporary
            && matches!(
                self.pre.o_membership_of(self.cfg, x, c),
                Memb::Awaited | Memb::Queued
            )
            && !(a == Action::Removed && self.s.life_of(x, c).is_none())
        {
            self.fail(GROUP_BARRIER_HOLDS);
        }
        if let Some(cell) = self.s.cells.get_mut(&t) {
            cell.delivered = true;
        }
        if how == Delivery::Snapshot {
            self.s.oracle.decided.remove(&(c, t));
        }
    }
}

// ---------------------------------------------------------------------------
// Manual completions, evaluation, groups and chains
// ---------------------------------------------------------------------------

impl Ap<'_> {
    /// The decision of a manual completion; first matching row.
    fn classify_manual(&mut self, c: Key, n: Tok, kind: Kind, r: Reason) {
        if self.s.expected_reason(self.cfg, n) != Some(r) {
            self.fail(REPORTS_CARRY_THE_REAL_REASON);
        }
        let decided = self.s.o_decide(self.cfg, c, kind);
        let restart = self.cfg.kid(c).restart;
        let in_restart_set = self
            .s
            .sup
            .group
            .as_ref()
            .is_some_and(|g| g.phase.restart_set_contains(c) && !g.overrides.contains(&c));
        let observed = |kind, restart, pool, action, pending, remove, initiate| Obs::Classify {
            kind,
            reason: r,
            restart,
            pool,
            in_restart_set,
            action,
            pending,
            remove,
            initiate,
        };
        let pool = self.cfg.is_pool();
        if kind == Kind::Bounce && matches!(r, Reason::Failure(_)) {
            // A crash racing a bounce is still a crash.
            self.obs.push(observed(
                kind,
                restart,
                pool,
                None,
                Pend::Evaluate,
                false,
                false,
            ));
            self.down(SupId::S, c, n, Pend::Evaluate);
            return;
        }
        let action;
        let prunable = self.cfg.prunable(c);
        if in_restart_set {
            // A stop committed before the group's evaluation rejoins it.
            self.down(SupId::S, c, n, Pend::Snapshot);
            action = Action::RestartInitiated;
            self.obs.push(observed(
                kind,
                restart,
                pool,
                Some(action),
                Pend::Snapshot,
                false,
                false,
            ));
        } else if kind == Kind::Terminate || pool {
            action = Action::Removed;
            self.obs.push(observed(
                kind,
                restart,
                pool,
                Some(action),
                Pend::Snapshot,
                prunable,
                false,
            ));
            self.finish_without_restart(c, n);
        } else if restart == Restart::Permanent {
            self.down(SupId::S, c, n, Pend::Snapshot);
            action = Action::RestartInitiated;
            self.obs.push(observed(
                kind,
                restart,
                pool,
                Some(action),
                Pend::Snapshot,
                false,
                true,
            ));
        } else {
            action = Action::Removed;
            self.obs.push(observed(
                kind,
                restart,
                pool,
                Some(action),
                Pend::Snapshot,
                prunable,
                false,
            ));
            self.finish_without_restart(c, n);
        }
        self.s.sup.snapshots.insert((c, n), (r, action));
        self.s.oracle.decided.insert((c, n), decided);
        if action == Action::RestartInitiated && !in_restart_set && restart == Restart::Permanent {
            // A budget-free bounce: the child restarts at once.
            self.initiate(SupId::S, c, Origin::Bounce);
        }
    }

    /// The end of a child that is not restarted: prune a prunable spec, keep
    /// any other as a down child with a pending snapshot.
    fn finish_without_restart(&mut self, c: Key, n: Tok) {
        if self.cfg.prunable(c) {
            self.remove_spec(SupId::S, c);
        } else {
            self.down(SupId::S, c, n, Pend::Snapshot);
        }
    }

    /// A trigger that reaches the supervisor's strategy.
    fn evaluate(&mut self, x: SupId, c: Key, t: Tok, r: Reason) {
        let restart = self.cfg.kid_of(x, c).restart;
        if !restart_eligible(restart, r) {
            if self.cfg.prunable_of(x, c) {
                self.remove_spec(x, c);
            }
            self.deliver(x, c, t, r, Action::Removed, Delivery::Decision);
            return;
        }
        if !self.charge(x) {
            self.s.oracle.mark_exhausted(x);
            if x != SupId::S {
                self.hit(Wit::SupervisorIncarnationExhaustedItsBudget);
            }
            self.deliver(x, c, t, r, Action::BudgetExhausted, Delivery::Decision);
            self.o_set_cause(x, SCause::Budget);
            self.final_drain(x, Reason::ParentRequest);
            return;
        }
        match self.cfg.strategy_of(x) {
            Strategy::OneForOne | Strategy::Pool => self.initiate(x, c, Origin::Strategy),
            Strategy::OneForAll | Strategy::RestForOne => self.begin_group(c),
        }
        self.deliver(x, c, t, r, Action::RestartInitiated, Delivery::Decision);
    }

    // ---- group restarts ---------------------------------------------------

    /// The members a group of `kind` covers when it starts at `c0`, each with
    /// its occupant now.
    fn group_members(&self, kind: &GKind, c0: Key) -> BTreeMap<Key, Option<Tok>> {
        let order = &self.s.sup.order;
        let pos = order.iter().position(|k| *k == c0).unwrap_or(0);
        let keys: &[Key] = match kind {
            GKind::All => order,
            GKind::Rest => &order[pos..],
        };
        keys.iter().map(|k| (*k, self.s.occ(*k))).collect()
    }

    fn begin_group(&mut self, c0: Key) {
        let kind = if self.cfg.strategy == Strategy::OneForAll {
            GKind::All
        } else {
            GKind::Rest
        };
        let members = self.group_members(&kind, c0);
        let outside: BTreeSet<Key> = match kind {
            GKind::All => BTreeSet::new(),
            GKind::Rest => {
                let order = &self.s.sup.order;
                let pos = order.iter().position(|k| *k == c0).unwrap_or(0);
                order[..pos].iter().copied().collect()
            }
        };
        // Harness: a new layout starts a fresh log; the scope of a terminate
        // that keeps a child down is read from the first layout's front.
        self.s.oracle.chain_log.clear();
        self.s.oracle.removed.clear();
        self.s.oracle.group_outside = {
            let order = &self.pre.sup.order;
            match kind {
                GKind::All => BTreeSet::new(),
                GKind::Rest => order.iter().copied().take_while(|k| *k != c0).collect(),
            }
        };
        self.check_layout_members(&kind, c0, &members);
        self.s.sup.group = Some(Group {
            kind,
            members,
            phase: Phase::Stop {
                awaiting: BTreeMap::new(),
                queue: VecDeque::new(),
                signalled: BTreeSet::new(),
                restart_order: Vec::new(),
            },
            overrides: BTreeSet::new(),
            restarted: BTreeSet::new(),
            outside,
        });
        self.lay_out(c0, false);
    }

    /// The group's affected set, the absorbed and rejoined reports, the exact
    /// barrier and the stop queue, for a first evaluation or a retry.
    fn lay_out(&mut self, c0: Key, retry: bool) {
        let members: Vec<(Key, Option<Tok>)> = {
            let g = self.s.sup.group.as_ref().expect("model invariant: group");
            self.s
                .sup
                .order
                .iter()
                .filter_map(|k| g.members.get(k).map(|o| (*k, *o)))
                .collect()
        };
        // Absorb every owed evaluation of a member, wherever its report is.
        for (c, _) in &members {
            let c = *c;
            let Some(Life::Down {
                n,
                pend: Pend::Evaluate,
            }) = self.s.life(c)
            else {
                self.obs.push(Obs::Absorb {
                    place: AbPlace::DeathPlane,
                    in_plan: true,
                    life: self.s.life(c),
                    want: AbWant::Keep,
                });
                continue;
            };
            let in_queued = self
                .s
                .sup
                .queued
                .iter()
                .position(|(k, t, _)| *k == c && *t == n);
            let in_worklist = self
                .worklist
                .iter()
                .position(|(k, t, _)| *k == c && *t == n);
            let temporary = self.member_end_prunes(SupId::S, c);
            self.obs.push(Obs::Absorb {
                place: if in_queued.is_some() {
                    AbPlace::Queued
                } else if in_worklist.is_some() {
                    AbPlace::Worklist
                } else {
                    AbPlace::DeathPlane
                },
                in_plan: true,
                life: self.s.life(c),
                want: if in_queued.is_some() || in_worklist.is_some() {
                    AbWant::Deliver
                } else {
                    AbWant::Snapshot
                },
            });
            if in_queued.is_some() || in_worklist.is_some() {
                self.hit(Wit::QueuedDeathAtRetry);
                let r = if let Some(i) = in_queued {
                    self.s.sup.queued.remove(i).2
                } else {
                    self.worklist
                        .remove(in_worklist.expect("model invariant: worklist position"))
                        .expect("model invariant: worklist entry")
                        .2
                };
                if temporary {
                    self.remove_spec(SupId::S, c);
                    self.deliver(SupId::S, c, n, r, Action::Removed, Delivery::Decision);
                } else {
                    self.down(SupId::S, c, n, Pend::Nothing);
                    self.deliver(
                        SupId::S,
                        c,
                        n,
                        r,
                        Action::RestartInitiated,
                        Delivery::Decision,
                    );
                }
            } else {
                self.hit(Wit::OwedEvaluationAbsorbedIntoSnapshot);
                let r = self.s.seen_reason(n);
                if temporary {
                    self.remove_spec(SupId::S, c);
                    self.s.sup.snapshots.insert((c, n), (r, Action::Removed));
                    self.s.oracle.decided.insert((c, n), Action::Removed);
                } else {
                    self.down(SupId::S, c, n, Pend::Snapshot);
                    self.s
                        .sup
                        .snapshots
                        .insert((c, n), (r, Action::RestartInitiated));
                    self.s
                        .oracle
                        .decided
                        .insert((c, n), Action::RestartInitiated);
                }
            }
        }
        // Rejoin: a stop committed before this layout joins the restart.
        for (c, _) in &members {
            let c = *c;
            let g = self.s.sup.group.as_ref().expect("model invariant: group");
            let restarts =
                self.cfg.kid(c).restart != Restart::Temporary && !g.overrides.contains(&c);
            if !restarts {
                continue;
            }
            let Some(Life::Down {
                n,
                pend: Pend::Snapshot,
            }) = self.s.life(c)
            else {
                continue;
            };
            if let Some((r, Action::Removed)) = self.s.sup.snapshots.get(&(c, n)).copied() {
                self.s
                    .sup
                    .snapshots
                    .insert((c, n), (r, Action::RestartInitiated));
                self.s
                    .oracle
                    .decided
                    .insert((c, n), Action::RestartInitiated);
            }
        }
        // Harness: a layout that restarts a child clears its mark of staying down.
        for (c, _) in &members {
            let c = *c;
            let g = self.s.sup.group.as_ref().expect("model invariant: group");
            if self.cfg.kid(c).restart != Restart::Temporary
                && !g.overrides.contains(&c)
                && !self.s.oracle.removed.contains(&c)
            {
                self.s.oracle.stays_down.remove(&c);
            }
        }
        // Commit every member that has an occupant into an exact barrier.
        let mut awaiting: BTreeMap<Key, Tok> = BTreeMap::new();
        for (c, _) in &members {
            if *c == c0 {
                continue;
            }
            if let Some(t) = self.s.occ(*c) {
                awaiting.insert(*c, t);
            }
        }
        let keys: Vec<Key> = awaiting.keys().copied().collect();
        for c in &keys {
            let t = awaiting[c];
            if retry && matches!(self.s.life(*c), Some(Life::Restarting { .. })) {
                self.hit(Wit::AttemptCommittedAtRetry);
            }
            match self.s.life(*c) {
                Some(Life::Stopping { kind, .. }) => {
                    self.hit(Wit::MemberStoppingAtEvaluation);
                    if kind == Kind::Terminate {
                        self.hit(Wit::TerminateCommittedBeforeEvaluation);
                    }
                }
                Some(Life::Restarting { .. })
                    if matches!(
                        self.s.cell(t).exec,
                        Exec::Attempt(AttemptStage::Wait | AttemptStage::Factory)
                    ) =>
                {
                    let waiting = self.s.cell(t).exec == Exec::Attempt(AttemptStage::Wait);
                    self.order(t, OrderFrom::Group);
                    // Counted only where the attempt's cell holds the order.
                    if self.s.cell(t).order.is_some() {
                        self.hit(match (waiting, retry) {
                            (true, false) => Wit::WaitingAttemptOrderedAtEvaluation,
                            (false, false) => Wit::UnspawnedAttemptOrderedAtEvaluation,
                            (true, true) => Wit::WaitingAttemptOrderedAtRetry,
                            (false, true) => Wit::UnspawnedAttemptOrderedAtRetry,
                        });
                    }
                }
                _ => {}
            }
        }
        let g = self.s.sup.group.as_mut().expect("model invariant: group");
        let restart_order: Vec<Key> = self
            .s
            .sup
            .order
            .iter()
            .copied()
            .filter(|k| g.members.contains_key(k))
            .filter(|k| self.cfg.kid(*k).restart != Restart::Temporary)
            .filter(|k| !g.overrides.contains(k))
            .collect();
        let queue: VecDeque<Key> = self
            .s
            .sup
            .order
            .iter()
            .rev()
            .copied()
            .filter(|k| awaiting.contains_key(k))
            .collect();
        {
            let inputs: Vec<CommitInput> = self
                .s
                .sup
                .order
                .iter()
                .filter_map(|k| {
                    let life = self.s.life(*k)?;
                    Some(CommitInput {
                        key: *k,
                        life,
                        restart: self.cfg.kid(*k).restart,
                    })
                })
                .collect();
            self.obs.push(Obs::Commit {
                failed: c0,
                rest_for_one: self
                    .s
                    .sup
                    .group
                    .as_ref()
                    .is_some_and(|g| g.kind == GKind::Rest),
                members: inputs,
                awaiting: awaiting.clone(),
                restart_order: restart_order.clone(),
                retry,
                overrides: self
                    .s
                    .sup
                    .group
                    .as_ref()
                    .map_or_else(Vec::new, |g| g.overrides.iter().copied().collect()),
            });
        }
        let g = self.s.sup.group.as_mut().expect("model invariant: group");
        g.phase = Phase::Stop {
            awaiting,
            queue,
            signalled: BTreeSet::new(),
            restart_order,
        };
        self.advance_stop_queue();
    }

    /// The one more ask rule: a stop that joins a member already stopping
    /// asks a vetoing `Infinity` member once more, at most once per group.
    fn ask_once_more(&mut self, c: Key, t: Tok) {
        if self.cfg.kid(c).policy != Policy::Infinity || !self.s.live(t) {
            return;
        }
        if vetoable(self.s.inc(t).lane) && !self.s.inc(t).reasked {
            self.raise(t, Reason::ParentRequest);
            self.s
                .incs
                .get_mut(&t)
                .expect("model invariant: inc")
                .reasked = true;
        }
    }

    /// Signal the next member of the stop queue, one at a time, newest first.
    fn advance_stop_queue(&mut self) {
        loop {
            let Some(g) = self.s.sup.group.as_ref() else {
                return;
            };
            let Phase::Stop {
                awaiting,
                queue,
                signalled,
                ..
            } = &g.phase
            else {
                return;
            };
            let Some(h) = queue.front().copied() else {
                if awaiting.is_empty() {
                    self.start_chain();
                }
                return;
            };
            let aw = awaiting.get(&h).copied();
            let is_signalled = signalled.contains(&h);
            let settled_by_verdict = aw.is_some_and(|t| self.s.cell(t).verdict);
            let handled = self.s.handled(h);
            let input = self.queue_input(h, aw, is_signalled, settled_by_verdict, handled);
            let prod = self.prod_queue_head(h);
            if aw.is_none() || settled_by_verdict || (!is_signalled && handled) {
                self.obs.push(Obs::Queue {
                    input,
                    want: QueueWant::Pop,
                    prod,
                    reached: false,
                });
                self.pop_queue_head();
                continue;
            }
            if is_signalled {
                self.obs.push(Obs::Queue {
                    input,
                    want: QueueWant::Wait,
                    prod,
                    reached: true,
                });
                return;
            }
            let t = aw.expect("model invariant: awaited token");
            let owes = self.queue_join_owes_ask(h);
            let before = self.raised.len();
            let kind = match self.s.life(h) {
                Some(Life::Running(_)) => {
                    if self.s.cell(t).order.is_some() {
                        self.ask_once_more(h, t);
                        QueueWant::Join { ask: false }
                    } else {
                        self.order(t, OrderFrom::Group);
                        QueueWant::Order
                    }
                }
                Some(Life::Stopping { .. }) => {
                    self.ask_once_more(h, t);
                    QueueWant::Join { ask: false }
                }
                Some(Life::Restarting { next, .. }) => {
                    let had_order = self.s.cell(next).order.is_some();
                    self.order(next, OrderFrom::Group);
                    let past_init = self.s.live(next)
                        && self.s.cell(next).exec == Exec::Attempt(AttemptStage::Init)
                        && self.s.inc(next).acked;
                    if had_order && past_init {
                        self.ask_once_more(h, next);
                    }
                    QueueWant::OrderAttempt { ask: false }
                }
                other => panic!("model invariant: awaited member has an occupant, got {other:?}"),
            };
            if let Some(tk) = owes {
                if !self.raised[before..].contains(&(tk, Reason::ParentRequest))
                    || !self.s.inc(tk).reasked
                {
                    self.fail(GROUP_BARRIER_HOLDS);
                }
            }
            // The one more ask the queue gave: the occupant's record turned
            // set in this step.
            let asked = input.life.is_some() && !input.reasked && {
                let current = aw.expect("model invariant: awaited token");
                self.s.incs.get(&current).is_some_and(|i| i.reasked)
            };
            let want = if input.life.is_none() {
                // The ledger holds another incarnation than the one the
                // barrier names: the order lands on the ended one and nothing
                // else is signalled; the entry waits for its report.
                QueueWant::Await
            } else {
                match kind {
                    QueueWant::Join { .. } => QueueWant::Join { ask: asked },
                    QueueWant::OrderAttempt { .. } if !input.spawned => QueueWant::OrderUnpublished,
                    QueueWant::OrderAttempt { .. } => QueueWant::OrderAttempt { ask: asked },
                    other => other,
                }
            };
            if let Some(Group {
                phase: Phase::Stop { signalled, .. },
                ..
            }) = self.s.sup.group.as_mut()
            {
                signalled.insert(h);
            }
            let reached = self.queue_head_signalled(h);
            self.obs.push(Obs::Queue {
                input,
                want,
                prod,
                reached,
            });
            return;
        }
    }

    /// What the stop queue reads of its head `h`, whose barrier entry names
    /// `aw`: the head's ledger entry counts only when it holds that
    /// incarnation.
    fn queue_input(
        &self,
        h: Key,
        aw: Option<Tok>,
        signalled: bool,
        verdict: bool,
        handled: bool,
    ) -> QueueInput {
        let life = self.s.life(h);
        let named = |l: Life| match l {
            Life::Running(n) | Life::Stopping { n, .. } | Life::Down { n, .. } => Some(n),
            Life::Restarting { next, .. } => Some(next),
        };
        let current = aw.filter(|t| life.and_then(named) == Some(*t));
        let infinity = self.cfg.kid(h).policy == Policy::Infinity;
        QueueInput {
            awaited: aw.is_some(),
            verdict,
            signalled,
            handled,
            life: current.and(life),
            has_order: current.is_some_and(|t| self.s.cell(t).order.is_some()),
            ask_possible: current.is_some_and(|t| {
                infinity && self.s.live(t) && vetoable(self.s.incs.get(&t).and_then(|i| i.lane))
            }),
            reasked: current.is_some_and(|t| self.s.incs.get(&t).is_some_and(|i| i.reasked)),
            past_init: matches!(life, Some(Life::Restarting { next, .. })
                if current == Some(next)
                    && self.s.live(next)
                    && self.s.cell(next).exec == Exec::Attempt(AttemptStage::Init)
                    && self.s.inc(next).acked),
            // The incarnation exists once the attempt spawned its actor, which
            // is when production's attempt publishes its lane.
            spawned: current.is_none_or(|t| self.s.spawned(t)),
        }
    }

    /// Whether the model's queue marks `h` as reached.
    fn queue_head_signalled(&self, h: Key) -> bool {
        matches!(
            self.s.sup.group.as_ref().map(|g| &g.phase),
            Some(Phase::Stop { signalled, .. }) if signalled.contains(&h)
        )
    }

    /// What production's queue reads of `h`, from the production image of the
    /// model's state now.
    fn prod_queue_head(&self, h: Key) -> QueueHead {
        let mut kept = Vec::new();
        let sup = img_supervision(self.cfg, &self.s, &mut kept);
        match sup.pending_group.as_ref() {
            Some(GroupPhase::Stopping(group)) => {
                crate::actor::runtime::queue_head(&sup, group, &ActorId::from(format!("child-{h}")))
            }
            _ => panic!("model invariant: a queue step has a stop phase"),
        }
    }

    /// A join by the stop queue of a member whose stop is already in progress
    /// owes one more ask when the member is a live `Infinity` child with a
    /// vetoable request on its lane that nobody asked once more yet.
    fn queue_join_owes_ask(&self, h: Key) -> Option<Tok> {
        let infinity = self.cfg.kid(h).policy == Policy::Infinity;
        let fresh = |n: Tok| {
            infinity && self.s.live(n) && vetoable(self.s.inc(n).lane) && !self.s.inc(n).reasked
        };
        match self.s.life(h) {
            Some(Life::Stopping { n, .. }) => fresh(n).then_some(n),
            Some(Life::Running(n)) if self.s.cell(n).order.is_some() => fresh(n).then_some(n),
            Some(Life::Restarting { next, .. }) => {
                let past_init = self.s.cell(next).order.is_some()
                    && self.s.live(next)
                    && self.s.cell(next).exec == Exec::Attempt(AttemptStage::Init)
                    && self.s.inc(next).acked;
                (past_init && fresh(next)).then_some(next)
            }
            _ => None,
        }
    }

    fn pop_queue_head(&mut self) {
        if let Some(Group {
            phase: Phase::Stop { queue, .. },
            ..
        }) = self.s.sup.group.as_mut()
        {
            queue.pop_front();
        }
    }

    fn start_chain(&mut self) {
        let g = self.s.sup.group.as_mut().expect("model invariant: group");
        let Phase::Stop { restart_order, .. } = &g.phase else {
            return;
        };
        let queue: VecDeque<Key> = restart_order
            .iter()
            .copied()
            .filter(|k| !g.overrides.contains(k))
            .collect();
        g.phase = Phase::Chain {
            queue,
            started: BTreeMap::new(),
        };
        self.advance_chain();
    }

    /// Restart members in start order, each after the previous one's ack.
    fn advance_chain(&mut self) {
        loop {
            let Some(g) = self.s.sup.group.as_ref() else {
                return;
            };
            let Phase::Chain { queue, started } = &g.phase else {
                return;
            };
            let Some(f) = queue.front().copied() else {
                self.complete_group();
                return;
            };
            let started_token = started.get(&f).copied();
            if let Some(life) = self.s.life(f) {
                let action = match life {
                    Life::Down { .. } => FrontKind::Initiate,
                    Life::Running(k) if started_token == Some(k) => FrontKind::Fresh,
                    Life::Running(_) => FrontKind::StopThenInitiate,
                    Life::Stopping { .. } | Life::Restarting { .. } => FrontKind::Hold,
                };
                self.obs.push(Obs::Front {
                    life,
                    started: started_token,
                    action,
                });
            }
            match self.s.life(f) {
                Some(Life::Down { .. }) => {
                    self.initiate(SupId::S, f, Origin::Chain);
                    let m = match self.s.life(f) {
                        Some(Life::Restarting { next, .. }) => next,
                        other => panic!("model invariant: initiated, got {other:?}"),
                    };
                    if let Some(Group {
                        phase: Phase::Chain { started, .. },
                        ..
                    }) = self.s.sup.group.as_mut()
                    {
                        started.insert(f, m);
                    }
                    return;
                }
                Some(Life::Running(k)) if started.get(&f) == Some(&k) => {
                    if let Some(g) = self.s.sup.group.as_mut() {
                        if let Phase::Chain { queue, .. } = &mut g.phase {
                            queue.pop_front();
                        }
                        g.restarted.insert(f);
                    }
                }
                Some(Life::Running(k)) => {
                    self.order(k, OrderFrom::ChainStop);
                    return;
                }
                Some(Life::Stopping { .. } | Life::Restarting { .. }) => return,
                None => {
                    // The spec is gone: nothing to restart.
                    if let Some(Group {
                        phase: Phase::Chain { queue, .. },
                        ..
                    }) = self.s.sup.group.as_mut()
                    {
                        queue.pop_front();
                    }
                }
            }
        }
    }

    /// The group is done: route the reports that waited behind it.
    fn complete_group(&mut self) {
        self.check_group_completion();
        self.s.sup.group = None;
        self.s.oracle.chain_log.clear();
        self.s.oracle.group_log.clear();
        self.s.oracle.group_outside.clear();
        self.s.oracle.removed.clear();
        let waiting: Vec<(Key, Tok, Reason)> = std::mem::take(&mut self.s.sup.queued);
        self.worklist.extend(waiting);
        if self.in_worklist {
            return;
        }
        self.in_worklist = true;
        while let Some((c, t, r)) = self.worklist.pop_front() {
            if self.s.s_phase != SPhase::Loop {
                break;
            }
            self.route_death(SupId::S, c, t, r);
        }
        self.in_worklist = false;
    }

    /// The front's own attempt failed during the chain: charge once, then lay
    /// the group out again from the registry as it is now.
    fn chain_start_failed(&mut self, f: Key, m: Tok, r: Reason) {
        self.down(SupId::S, f, m, Pend::Nothing);
        if !self.charge(SupId::S) {
            self.s.oracle.mark_exhausted(SupId::S);
            self.deliver(
                SupId::S,
                f,
                m,
                r,
                Action::BudgetExhausted,
                Delivery::Decision,
            );
            self.s.oracle.s_cause = Some(SCause::Budget);
            self.final_drain(SupId::S, Reason::ParentRequest);
            return;
        }
        self.deliver(
            SupId::S,
            f,
            m,
            r,
            Action::RestartInitiated,
            Delivery::Decision,
        );
        let kind = self
            .s
            .sup
            .group
            .as_ref()
            .expect("model invariant: group")
            .kind
            .clone();
        let members = self.group_members(&kind, f);
        // A child started since the plan was made, kept down by an override,
        // when the retry lays the group out again.
        let spawned_since_kept_down = {
            let g = self.s.sup.group.as_ref().expect("model invariant: group");
            g.overrides.iter().any(|k| !g.members.contains_key(k))
        };
        if spawned_since_kept_down {
            self.hit(Wit::SpawnedSinceOverrideLeftOutOfTheRetry);
        }
        self.s.oracle.chain_log.clear();
        self.s.oracle.removed.clear();
        self.check_layout_members(&kind, f, &members);
        let g = self.s.sup.group.as_mut().expect("model invariant: group");
        g.members = members;
        g.phase = Phase::Stop {
            awaiting: BTreeMap::new(),
            queue: VecDeque::new(),
            signalled: BTreeSet::new(),
            restart_order: Vec::new(),
        };
        self.lay_out(f, true);
    }
}

// ---------------------------------------------------------------------------
// The supervisor's loop: draining reports, leaving the loop, tearing down
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReportClass {
    Occupant,
    OwedEvaluation,
    Stale,
}

impl Ap<'_> {
    // ---- oracle accounting for the restart budget ---------------------------

    fn o_count_charge(&mut self, x: SupId) {
        let cap = self.cfg.budget_of(x) + 1;
        let v = (self.s.oracle.charges_of(x) + 1).min(cap);
        self.s.oracle.set_charges(x, v);
    }

    /// Whether supervisor incarnation `x`'s window covers the stop budgets of
    /// the keys that have an occupant as its teardown begins, computed from
    /// the configuration and the ledger.
    fn o_record_covers(&mut self, x: SupId) {
        let SupId::Inc(p) = x else {
            return;
        };
        let window = self
            .cfg
            .kid_of(self.s.cell(p).sup, self.s.cell(p).key)
            .window();
        if window == INF {
            return;
        }
        let mut budgets: Vec<u64> = Vec::new();
        for (c, life) in &self.s.sup_ref(x).ledger {
            let occupied = match life {
                Life::Running(_) | Life::Stopping { .. } | Life::Restarting { .. } => true,
                Life::Down { .. } => false,
            };
            if occupied {
                budgets.push(self.cfg.kid_of(x, *c).stop_budget());
            }
        }
        let need = if self.cfg.pool_of(x) {
            budgets.iter().copied().max().unwrap_or(0)
        } else {
            budgets.iter().fold(0u64, |a, b| a.saturating_add(*b))
        };
        if window >= need {
            self.s.oracle.p_covers.insert(p);
        }
    }

    /// Why supervisor `x` left its loop, recorded once.
    fn o_set_cause(&mut self, x: SupId, c: SCause) {
        match x {
            SupId::S => self.s.oracle.s_cause = Some(c),
            SupId::Inc(p) => {
                self.s.oracle.p_cause.insert(p, c);
            }
        }
    }

    fn o_cause(&self, x: SupId) -> Option<SCause> {
        match x {
            SupId::S => self.s.oracle.s_cause,
            SupId::Inc(p) => self.s.oracle.p_cause.get(&p).copied(),
        }
    }

    /// Whether the report of a death is a trigger the supervisor evaluates
    /// right now, judged from the facts of the incarnation's end.
    fn o_count_death(&mut self, x: SupId, c: Key, t: Tok, r: Reason) {
        let s = &self.s;
        if matches!(s.life_of(x, c), Some(Life::Restarting { next, .. }) if next == t) {
            return;
        }
        if s.oracle.decided.contains_key(&(c, t)) {
            return;
        }
        let owed_eval =
            matches!(s.life_of(x, c), Some(Life::Down { n, pend: Pend::Evaluate }) if n == t);
        match s.cells.get(&t).and_then(|x| x.order).map(|o| o.from) {
            Some(OrderFrom::Group | OrderFrom::ChainStop | OrderFrom::Teardown) => return,
            Some(OrderFrom::Manual) => {
                let bounce_crash = matches!(
                    s.life_of(x, c),
                    Some(Life::Stopping { n, kind: Kind::Bounce }) if n == t
                ) && matches!(r, Reason::Failure(_));
                if !(bounce_crash || owed_eval) {
                    return;
                }
            }
            None => {}
        }
        if !(s.occ_of(x, c) == Some(t) || owed_eval) {
            return;
        }
        if matches!(
            s.o_membership_of(self.cfg, x, c),
            Memb::Awaited | Memb::Queued
        ) {
            return;
        }
        if s.sup_ref(x).group.is_some() {
            return;
        }
        if restart_expected(self.cfg.kid_of(x, c).restart, r) {
            self.o_count_charge(x);
        }
    }

    fn o_count_attempt_failure(&mut self, x: SupId, c: Key, m: Tok, r: Reason) {
        let s = &self.s;
        let origin = s.cell(m).origin;
        if origin == Origin::Chain && s.sup_ref(x).group.is_some() {
            self.o_count_charge(x);
            return;
        }
        if matches!(
            s.o_membership_of(self.cfg, x, c),
            Memb::Awaited | Memb::Queued
        ) {
            return;
        }
        if origin == Origin::Manual || s.sup_ref(x).group.is_some() {
            return;
        }
        if restart_expected(self.cfg.kid_of(x, c).restart, r) {
            self.o_count_charge(x);
        }
    }

    // ---- recording what a drain saw ------------------------------------------

    /// A drain that only records: the supervisor is stopping, so nothing is
    /// delivered, charged or restarted.
    fn record_drained(&mut self, x: SupId, c: Key, t: Tok) {
        match self.s.life_of(x, c) {
            Some(Life::Running(n) | Life::Stopping { n, .. }) if n == t => {
                self.down(x, c, t, Pend::Nothing);
            }
            Some(Life::Restarting { next, .. }) if next == t => self.down(x, c, t, Pend::Nothing),
            Some(Life::Down { n, pend }) if n == t && pend != Pend::Nothing => {
                self.down(x, c, t, Pend::Nothing);
            }
            _ => {}
        }
        self.s.sup_mut(x).snapshots.remove(&(c, t));
        self.s.oracle.decided.remove(&(c, t));
        if let Some(Group {
            phase: Phase::Stop { awaiting, .. },
            ..
        }) = self.s.sup_mut(x).group.as_mut()
        {
            if awaiting.get(&c) == Some(&t) {
                awaiting.remove(&c);
            }
        }
    }

    fn remove_awaiting(&mut self, x: SupId, c: Key, t: Tok) {
        if let Some(Group {
            phase: Phase::Stop { awaiting, .. },
            ..
        }) = self.s.sup_mut(x).group.as_mut()
        {
            if awaiting.get(&c) == Some(&t) {
                awaiting.remove(&c);
            }
        }
    }

    /// An adopted restart attempt becomes the child's runner.
    fn adopt(&mut self, x: SupId, c: Key, m: Tok) {
        self.set_life(x, c, Some(Life::Running(m)));
        self.s
            .cells
            .get_mut(&m)
            .expect("model invariant: cell")
            .exec = Exec::Runner;
        if self.s.cell(m).order.is_some() && self.s.live(m) {
            self.arm(m);
        }
        if !self.s.live(m) {
            // It ended in transit: its end is a death like any other.
            let end = self.s.inc(m).end.expect("model invariant: end");
            self.s
                .cells
                .get_mut(&m)
                .expect("model invariant: cell")
                .fate = Some(end);
            self.put_death(x, c, m, end);
        }
    }

    fn drain_ack(&mut self, x: SupId, m: Tok) {
        let (c, o) = self
            .s
            .sup_mut(x)
            .outcomes
            .remove(&m)
            .expect("model invariant: an adopted outcome");
        assert_eq!(o, Outcome::Adopted, "model invariant: ack drain");
        match self.s.life_of(x, c) {
            Some(Life::Restarting { next, .. }) if next == m => {}
            other => {
                panic!("model invariant: adoption of an attempt the ledger holds, got {other:?}")
            }
        }
        if self.s.cell(m).origin == Origin::Chain && self.s.sup_ref(x).group.is_some() {
            self.s.oracle.chain_log.push(c);
            self.s.oracle.group_log.insert(c);
        }
        self.adopt(x, c, m);
        if self.s.sup_ref(x).teardown.is_some() {
            return;
        }
        let is_front = self
            .s
            .sup_ref(x)
            .group
            .as_ref()
            .is_some_and(|g| match &g.phase {
                Phase::Chain { queue, started } => {
                    queue.front() == Some(&c) && started.get(&c) == Some(&m)
                }
                Phase::Stop { .. } => false,
            });
        if is_front {
            self.advance_chain();
        }
    }

    fn drain_outcome(&mut self, x: SupId, m: Tok) {
        let (c, o) = self
            .s
            .sup_mut(x)
            .outcomes
            .remove(&m)
            .expect("model invariant: an outcome");
        if self.s.sup_ref(x).teardown.is_some() {
            self.record_drained(x, c, m);
            return;
        }
        match o {
            Outcome::Adopted => {
                panic!("model invariant: adopted outcomes are drained by the ack step")
            }
            Outcome::Cancelled => {
                self.down(x, c, m, Pend::Nothing);
                if self.awaited_token(x, c) == Some(m) {
                    self.remove_awaiting(x, c, m);
                    if x == SupId::S {
                        self.advance_stop_queue();
                    }
                }
            }
            Outcome::Stopped(r) => {
                self.down(x, c, m, Pend::Nothing);
                self.remove_awaiting(x, c, m);
                if self.member_end_prunes(x, c) {
                    self.remove_spec(x, c);
                    self.deliver(x, c, m, r, Action::Removed, Delivery::Decision);
                } else {
                    self.deliver(x, c, m, r, Action::RestartInitiated, Delivery::Decision);
                }
                if x == SupId::S {
                    self.advance_stop_queue();
                }
            }
            Outcome::Failed(r) => {
                if r == Reason::Failure(Cause::Shutdown) {
                    if x == SupId::S {
                        // S is never reached by the shutdown that was abandoned.
                        if self
                            .s
                            .shutdown
                            .is_some_and(|sh| sh.abandoned && !sh.reached)
                        {
                            self.hit(Wit::RestartRefusedWhileShuttingDown);
                        }
                    } else {
                        self.hit(Wit::RestartRefusedAtSupervisorLevel);
                    }
                }
                self.attempt_failed(x, c, m, r)
            }
            Outcome::PredecessorUnresponsive => {
                // The successor wait on the predecessor's subtree expired: the
                // supervisor leaves its loop with the verdict.
                self.down(x, c, m, Pend::Nothing);
                self.o_set_cause(x, SCause::Unresponsive);
                self.final_drain(x, Reason::Failure(Cause::Unresponsive(c)));
            }
        }
    }

    /// Whether a group removes member `c` at its end instead of restarting
    /// it: a `Temporary` member is pruned. Records the decision for the
    /// conformance check.
    fn member_end_prunes(&mut self, x: SupId, c: Key) -> bool {
        let restart = self.cfg.kid_of(x, c).restart;
        let prune = restart == Restart::Temporary;
        self.obs.push(Obs::MemberEnd { restart, prune });
        prune
    }

    fn awaited_token(&self, x: SupId, c: Key) -> Option<Tok> {
        match self.s.sup_ref(x).group.as_ref().map(|g| &g.phase) {
            Some(Phase::Stop { awaiting, .. }) => awaiting.get(&c).copied(),
            _ => None,
        }
    }

    /// The failed outcome of a restart attempt, routed by the first row that
    /// fits: the chain front's own attempt, a committed member, a manual
    /// restart, a non-member during a group, anything else.
    fn attempt_failed(&mut self, x: SupId, c: Key, m: Tok, r: Reason) {
        self.o_count_attempt_failure(x, c, m, r);
        let front_own = self
            .s
            .sup_ref(x)
            .group
            .as_ref()
            .is_some_and(|g| match &g.phase {
                Phase::Chain { queue, started } => {
                    queue.front() == Some(&c) && started.get(&c) == Some(&m)
                }
                Phase::Stop { .. } => false,
            });
        let memb = self.s.membership_of(x, c);
        let origin = self.s.cell(m).origin;
        let route = if front_own {
            RouteKind::ChainRetry
        } else if matches!(memb, Memb::Awaited | Memb::Queued) {
            RouteKind::Member
        } else if origin == Origin::Manual {
            RouteKind::Removed
        } else {
            RouteKind::Evaluate
        };
        self.obs.push(Obs::Route {
            chain_front_own: front_own,
            memb,
            origin,
            route,
        });
        if front_own {
            self.chain_start_failed(c, m, r);
        } else if matches!(memb, Memb::Awaited | Memb::Queued) {
            if origin == Origin::Manual {
                self.hit(Wit::ManualAttemptFailsAsMember);
            }
            self.down(x, c, m, Pend::Nothing);
            self.remove_awaiting(x, c, m);
            if self.member_end_prunes(x, c) {
                self.remove_spec(x, c);
                self.deliver(x, c, m, r, Action::Removed, Delivery::Decision);
            } else {
                self.deliver(x, c, m, r, Action::RestartInitiated, Delivery::Decision);
            }
            if x == SupId::S {
                self.advance_stop_queue();
            }
        } else if origin == Origin::Manual {
            self.hit(Wit::ManualRestartInitFails);
            self.down(x, c, m, Pend::Nothing);
            self.deliver(x, c, m, r, Action::Removed, Delivery::Decision);
            if x == SupId::S {
                self.o_stays_down_if_not_restarted(c);
            }
        } else if self.s.sup_ref(x).group.is_some() {
            self.down(x, c, m, Pend::Evaluate);
            self.s.sup_mut(x).queued.push((c, m, r));
        } else {
            self.down(x, c, m, Pend::Nothing);
            self.evaluate(x, c, m, r);
        }
    }

    // ---- death reports ----------------------------------------------------------

    fn report_class(&self, x: SupId, c: Key, t: Tok) -> ReportClass {
        if self.s.occ_of(x, c) == Some(t) {
            ReportClass::Occupant
        } else if matches!(self.s.life_of(x, c), Some(Life::Down { n, pend: Pend::Evaluate }) if n == t)
        {
            ReportClass::OwedEvaluation
        } else {
            ReportClass::Stale
        }
    }

    /// Route a drained death report.
    fn route_death(&mut self, x: SupId, c: Key, t: Tok, r: Reason) {
        if self.s.sup_ref(x).teardown.is_some() {
            self.record_drained(x, c, t);
            return;
        }
        self.o_count_death(x, c, t, r);
        self.route_death_inner(x, c, t, r);
    }

    fn route_death_inner(&mut self, x: SupId, c: Key, t: Tok, r: Reason) {
        let cls = self.report_class(x, c, t);
        self.obs.push(Obs::Report {
            life: self.s.life_of(x, c),
            t,
            class: cls,
        });
        match cls {
            ReportClass::Stale => {
                if let Some((sr, a)) = self.s.sup_mut(x).snapshots.remove(&(c, t)) {
                    if matches!(self.s.life_of(x, c), Some(Life::Down { n, pend: Pend::Snapshot }) if n == t)
                    {
                        self.down(x, c, t, Pend::Nothing);
                    }
                    self.remove_awaiting(x, c, t);
                    if x == SupId::S {
                        self.advance_stop_queue();
                    }
                    self.deliver(x, c, t, sr, a, Delivery::Snapshot);
                } else {
                    self.fail(EVERY_OWED_REPORT_IS_DELIVERED_ONCE);
                }
            }
            ReportClass::OwedEvaluation => {
                if matches!(self.s.membership_of(x, c), Memb::Awaited | Memb::Queued) {
                    self.down(x, c, t, Pend::Nothing);
                    self.remove_awaiting(x, c, t);
                    if self.member_end_prunes(x, c) {
                        self.remove_spec(x, c);
                        self.deliver(x, c, t, r, Action::Removed, Delivery::Decision);
                    } else {
                        self.deliver(x, c, t, r, Action::RestartInitiated, Delivery::Decision);
                    }
                    if x == SupId::S {
                        self.advance_stop_queue();
                    }
                } else if self.s.sup_ref(x).group.is_some() {
                    self.s.sup_mut(x).queued.push((c, t, r));
                } else {
                    self.down(x, c, t, Pend::Nothing);
                    self.evaluate(x, c, t, r);
                }
            }
            ReportClass::Occupant => match self
                .s
                .life_of(x, c)
                .expect("model invariant: occupant has a ledger entry")
            {
                Life::Stopping { kind, .. } => {
                    // The caller gave up: the loop completes the manual stop.
                    self.classify_manual(c, t, kind, r);
                    self.route_death_inner(x, c, t, r);
                }
                Life::Restarting { .. } => self.attempt_failed(x, c, t, r),
                Life::Running(_) => self.route_running_death(x, c, t, r),
                Life::Down { .. } => unreachable!("a down child has no occupant"),
            },
        }
    }

    fn route_running_death(&mut self, x: SupId, c: Key, t: Tok, r: Reason) {
        let awaited = self.awaited_token(x, c) == Some(t);
        let chain_stop_front = self
            .s
            .cell(t)
            .order
            .is_some_and(|o| o.from == OrderFrom::ChainStop)
            && self
                .s
                .sup_ref(x)
                .group
                .as_ref()
                .is_some_and(|g| match &g.phase {
                    Phase::Chain { queue, .. } => queue.front() == Some(&c),
                    Phase::Stop { .. } => false,
                });
        if awaited {
            self.remove_awaiting(x, c, t);
            self.down(x, c, t, Pend::Nothing);
            let overridden = self
                .s
                .sup_ref(x)
                .group
                .as_ref()
                .is_some_and(|g| g.overrides.contains(&c));
            if overridden {
                if self.cfg.prunable_of(x, c) {
                    self.remove_spec(x, c);
                }
                self.deliver(x, c, t, r, Action::Removed, Delivery::Decision);
            } else if self.member_end_prunes(x, c) {
                self.remove_spec(x, c);
                self.deliver(x, c, t, r, Action::Removed, Delivery::Decision);
            } else {
                self.deliver(x, c, t, r, Action::RestartInitiated, Delivery::Decision);
            }
            if x == SupId::S {
                self.advance_stop_queue();
            }
        } else if chain_stop_front {
            self.down(x, c, t, Pend::Nothing);
            self.deliver(x, c, t, r, Action::RestartInitiated, Delivery::Decision);
            self.advance_chain();
        } else if self.s.sup_ref(x).group.is_some() {
            self.down(x, c, t, Pend::Evaluate);
            self.s.sup_mut(x).queued.push((c, t, r));
        } else {
            self.down(x, c, t, Pend::Nothing);
            self.evaluate(x, c, t, r);
        }
    }

    // ---- leaving the loop -------------------------------------------------------

    /// The supervisor leaves its loop other than by `Kill`: every report still
    /// pending is collected for a delivery of its own, and the group ends.
    fn final_drain(&mut self, x: SupId, r: Reason) {
        let mut q: Vec<(Key, Tok, Reason)> = Vec::new();
        let rest: Vec<(Key, Tok, Reason)> = self.worklist.drain(..).collect();
        let queued: Vec<(Key, Tok, Reason)> = std::mem::take(&mut self.s.sup_mut(x).queued);
        let from_queues = !rest.is_empty() || !queued.is_empty();
        for e in rest.into_iter().chain(queued) {
            q.push(e);
            self.record_drained(x, e.0, e.1);
        }
        if from_queues {
            self.collected_from_queue = true;
        }
        let outcomes: Vec<(Tok, (Key, Outcome))> = std::mem::take(&mut self.s.sup_mut(x).outcomes)
            .into_iter()
            .collect();
        for (m, (c, o)) in outcomes {
            match o {
                Outcome::Adopted => self.adopt_collected(x, c, m),
                Outcome::Failed(rr) | Outcome::Stopped(rr) => {
                    q.push((c, m, rr));
                    self.record_drained(x, c, m);
                }
                Outcome::Cancelled | Outcome::PredecessorUnresponsive => {
                    self.record_drained(x, c, m);
                }
            }
        }
        let deaths: Vec<(Key, Tok, Reason)> = std::mem::take(&mut self.s.sup_mut(x).deaths)
            .into_iter()
            .collect();
        for e in deaths {
            q.push(e);
            self.record_drained(x, e.0, e.1);
        }
        // The group ends with the loop: no step of it can run now.
        self.s.sup_mut(x).group = None;
        if x == SupId::S {
            self.s.oracle.chain_log.clear();
            self.s.oracle.group_log.clear();
            self.s.oracle.group_outside.clear();
            self.s.oracle.removed.clear();
        }
        if !q.is_empty() {
            self.hit(Wit::ReportCollectedByFinalDrain);
            if self.s.oracle.exhausted(x) || self.o_cause(x) == Some(SCause::Unresponsive) {
                self.hit(Wit::ReportCollectedByEscalationDrain);
            }
            if self.collected_from_queue {
                self.hit(Wit::ReportQueuedCollectedByFinalDrain);
            }
        }
        self.s.sup_mut(x).final_q = q;
        match x {
            SupId::S => self.s.s_phase = SPhase::Final(r),
            SupId::Inc(p) => {
                let i = self.s.incs.get_mut(&p).expect("model invariant: inc");
                i.stop_reason = Some(r);
                i.status = Status::Stopping;
                i.stage = Stage::Final;
            }
        }
    }

    /// Adoption inside the final collection: no delivery, and an incarnation
    /// that already ended adds its death to the plane at once.
    fn adopt_collected(&mut self, x: SupId, c: Key, m: Tok) {
        self.adopt(x, c, m);
        if !self.s.live(m) {
            // `adopt` queued the death in the plane; the collection takes it next.
            self.s
                .cells
                .get_mut(&m)
                .expect("model invariant: cell")
                .owed = true;
        }
    }

    /// The supervisor's supervision state is dropped: its children go to their
    /// guards, and every report it still owed is excused.
    fn drop_supervision(&mut self, x: SupId) {
        self.check_budget_at_exit(x);
        let tokens: Vec<Tok> = self
            .s
            .cells
            .iter()
            .filter(|(_, c)| c.sup == x)
            .map(|(t, _)| *t)
            .collect();
        for t in tokens {
            let exec = self.s.cell(t).exec;
            let live = self.s.live(t);
            match exec {
                Exec::Runner | Exec::Attempt(AttemptStage::Init | AttemptStage::InTransit)
                    if live =>
                {
                    self.fire_guard(t, Exec::Orphan);
                }
                Exec::RunnerFailed if live => {
                    self.s
                        .cells
                        .get_mut(&t)
                        .expect("model invariant: cell")
                        .exec = Exec::Orphan;
                }
                Exec::Attempt(AttemptStage::Wait | AttemptStage::Factory) => {
                    self.s
                        .cells
                        .get_mut(&t)
                        .expect("model invariant: cell")
                        .exec = Exec::Returned;
                }
                _ => {}
            }
            let c = self.s.cells.get_mut(&t).expect("model invariant: cell");
            c.closed = true;
            c.ladder = Ladder::Off;
        }
        // Excuse every report still in a plane of the dropped state.
        let mut pending: Vec<Tok> = Vec::new();
        pending.extend(self.s.sup_ref(x).final_q.iter().map(|e| e.1));
        pending.extend(self.s.sup_ref(x).queued.iter().map(|e| e.1));
        pending.extend(self.s.sup_ref(x).deaths.iter().map(|e| e.1));
        for (m, (_, o)) in &self.s.sup_ref(x).outcomes {
            if matches!(o, Outcome::Failed(_) | Outcome::Stopped(_)) {
                pending.push(*m);
            }
        }
        if x == SupId::S {
            pending.extend(self.worklist.iter().map(|e| e.1));
        }
        for t in pending {
            if let Some(c) = self.s.cells.get_mut(&t) {
                if c.owed && !c.delivered {
                    c.excused = true;
                }
            }
        }
        match x {
            SupId::S => {
                self.worklist.clear();
                self.s.oracle.chain_log.clear();
                self.s.oracle.group_log.clear();
                self.s.oracle.group_outside.clear();
                self.s.oracle.removed.clear();
                self.s.oracle.pending_call = None;
                self.s.sup = Sup::default();
                self.s.caller = None;
                self.s.s_phase = SPhase::Done(Reason::Kill);
            }
            SupId::Inc(p) => {
                self.s.psups.remove(&p);
            }
        }
    }

    // ---- the child teardown ---------------------------------------------------------

    /// Signal one key for the teardown and report the token its stop waits on.
    fn teardown_signal(&mut self, x: SupId, c: Key) -> Option<Tok> {
        // Harness: a join of a stop in progress under `Infinity` with a
        // vetoable request must ask once more, whatever an earlier joiner did.
        let owes = self.teardown_join_owes_ask(x, c);
        let before = self.raised.len();
        // Harness: newest first, each after the one started later settled.
        if !self.cfg.pool_of(x) {
            let pos = self
                .s
                .sup_ref(x)
                .order
                .iter()
                .position(|k| *k == c)
                .unwrap_or(0);
            let later_unsettled = self.s.sup_ref(x).order[pos + 1..]
                .iter()
                .any(|k| self.s.occ_of(x, *k).is_some_and(|t| !self.s.settled(t)));
            if later_unsettled {
                self.fail(TEARDOWN_ORDER);
            }
        }
        let signalled = self.teardown_signal_logic(x, c);
        if let Some(t) = owes {
            if !self.raised[before..].contains(&(t, Reason::ParentRequest)) {
                self.fail(TEARDOWN_ORDER);
            }
        }
        signalled
    }

    fn teardown_join_owes_ask(&self, x: SupId, c: Key) -> Option<Tok> {
        let infinity = self.cfg.kid_of(x, c).policy == Policy::Infinity;
        match self.s.life_of(x, c) {
            Some(Life::Stopping { n, .. }) => {
                (infinity && self.s.live(n) && vetoable(self.s.inc(n).lane)).then_some(n)
            }
            Some(Life::Running(n)) if self.s.cell(n).order.is_some() => {
                (infinity && self.s.live(n) && vetoable(self.s.inc(n).lane)).then_some(n)
            }
            Some(Life::Restarting { next, .. }) => {
                let past_init = self.s.live(next)
                    && self.s.cell(next).order.is_some()
                    && self.s.cell(next).exec == Exec::Attempt(AttemptStage::Init)
                    && self.s.inc(next).acked;
                (infinity && past_init && vetoable(self.s.inc(next).lane)).then_some(next)
            }
            _ => None,
        }
    }

    /// What the teardown reads of key `c` of `x` when its turn comes.
    fn o_walk_input(&self, x: SupId, c: Key) -> WalkInput {
        let life = self.s.life_of(x, c);
        let occupant = self.s.occ_of(x, c);
        let infinity = self.cfg.kid_of(x, c).policy == Policy::Infinity;
        WalkInput {
            life,
            has_order: occupant.is_some_and(|t| self.s.cell(t).order.is_some()),
            ask_possible: occupant.is_some_and(|t| {
                infinity && self.s.live(t) && vetoable(self.s.incs.get(&t).and_then(|i| i.lane))
            }),
            reasked: occupant.is_some_and(|t| self.s.incs.get(&t).is_some_and(|i| i.reasked)),
            past_init: matches!(
                life,
                Some(Life::Restarting { next, .. })
                    if self.s.live(next)
                        && self.s.cell(next).exec == Exec::Attempt(AttemptStage::Init)
                        && self.s.inc(next).acked
            ),
        }
    }

    /// What the model's teardown does about key `c` of `x`, read from the
    /// state and not from the table.
    fn o_td_want(&self, x: SupId, c: Key) -> TdWant {
        match self.s.life_of(x, c) {
            Some(Life::Running(t)) if self.s.cell(t).order.is_none() => TdWant::Order,
            Some(Life::Running(t) | Life::Stopping { n: t, .. }) => TdWant::Join {
                ask: self.cfg.kid_of(x, c).policy == Policy::Infinity
                    && self.s.live(t)
                    && vetoable(self.s.inc(t).lane),
            },
            Some(Life::Restarting { next, .. }) => {
                let past_init = self.s.live(next)
                    && self.s.cell(next).exec == Exec::Attempt(AttemptStage::Init)
                    && self.s.inc(next).acked;
                TdWant::OrderAttempt {
                    ask: self.s.cell(next).order.is_some()
                        && past_init
                        && self.cfg.kid_of(x, c).policy == Policy::Infinity
                        && vetoable(self.s.inc(next).lane),
                }
            }
            Some(Life::Down { .. }) | None => TdWant::Skip,
        }
    }

    fn teardown_signal_logic(&mut self, x: SupId, c: Key) -> Option<Tok> {
        // Harness: what the turn of this key read, and what the model decided.
        let input = self.o_walk_input(x, c);
        let want = self.o_td_want(x, c);
        self.obs.push(Obs::Teardown {
            input,
            want,
            pool: self.cfg.pool_of(x),
        });
        match self.s.life_of(x, c) {
            Some(Life::Running(t)) if self.s.cell(t).order.is_none() => {
                self.order(t, OrderFrom::Teardown);
                Some(t)
            }
            Some(Life::Running(t) | Life::Stopping { n: t, .. }) => {
                // A stop already in progress is joined, with one more ask of
                // a vetoing `Infinity` child whatever an earlier joiner asked.
                if self.cfg.kid_of(x, c).policy == Policy::Infinity
                    && self.s.live(t)
                    && vetoable(self.s.inc(t).lane)
                {
                    self.raise(t, Reason::ParentRequest);
                    if matches!(self.s.life_of(x, c), Some(Life::Stopping { .. })) {
                        if self.s.inc(t).reasked {
                            self.hit(Wit::TeardownAskAfterGroupStopping);
                        }
                    } else if self
                        .s
                        .cell(t)
                        .order
                        .is_some_and(|o| o.from == OrderFrom::Group)
                    {
                        self.hit(Wit::TeardownAskAfterGroupRunning);
                    }
                }
                Some(t)
            }
            Some(Life::Restarting { next, .. }) => {
                let had_order = self.s.cell(next).order.is_some();
                self.order(next, OrderFrom::Teardown);
                let past_init = self.s.live(next)
                    && self.s.cell(next).exec == Exec::Attempt(AttemptStage::Init)
                    && self.s.inc(next).acked;
                if had_order
                    && past_init
                    && self.cfg.kid_of(x, c).policy == Policy::Infinity
                    && vetoable(self.s.inc(next).lane)
                {
                    self.raise(next, Reason::ParentRequest);
                    self.hit(Wit::TeardownAskToOrderedAttempt);
                }
                Some(next)
            }
            Some(Life::Down { .. }) | None => None,
        }
    }

    /// Whether `x`'s window covers the sum of the stop budgets of the keys
    /// that have an occupant now. A root has no window.
    fn covers(&self, x: SupId) -> bool {
        let SupId::Inc(p) = x else {
            return false;
        };
        let window = self
            .cfg
            .kid_of(self.s.cell(p).sup, self.s.cell(p).key)
            .window();
        if window == INF {
            return false;
        }
        let budgets: Vec<u64> = self
            .s
            .sup_ref(x)
            .order
            .iter()
            .filter(|c| self.s.occ_of(x, **c).is_some())
            .map(|c| self.cfg.kid_of(x, *c).stop_budget())
            .collect();
        let need = if self.cfg.pool_of(x) {
            budgets.iter().copied().max().unwrap_or(0)
        } else {
            budgets.iter().fold(0, |a, b| sat(a, *b))
        };
        window >= need
    }

    fn begin_teardown(&mut self, x: SupId, r: Reason) {
        self.s.sup_mut(x).group = None;
        self.s.sup_mut(x).queued.clear();
        self.s.sup_mut(x).snapshots.clear();
        if x == SupId::S {
            self.s.oracle.decided.clear();
            self.s.oracle.chain_log.clear();
            self.s.oracle.group_log.clear();
            self.s.oracle.group_outside.clear();
            self.s.oracle.removed.clear();
        }
        let occupants: Vec<Tok> = self
            .s
            .sup_ref(x)
            .order
            .iter()
            .filter_map(|c| self.s.occ_of(x, *c))
            .collect();
        let walk: VecDeque<Key> = self.s.sup_ref(x).order.iter().rev().copied().collect();
        self.o_record_covers(x);
        let mut driver = Driver {
            walk,
            current: None,
            pool_wait: BTreeSet::new(),
            reason: r,
            covers: self.covers(x),
        };
        if x == SupId::S {
            self.s.s_phase = SPhase::Teardown;
        }
        if self.cfg.pool_of(x) {
            let keys: Vec<Key> = driver.walk.drain(..).collect();
            // Harness: what the one step that stops the whole pool read of
            // every key, in start order, and what it decided for each.
            let in_start_order: Vec<Key> = keys.iter().rev().copied().collect();
            let inputs = in_start_order
                .iter()
                .map(|c| self.o_walk_input(x, *c))
                .collect();
            let wants = in_start_order
                .iter()
                .map(|c| self.o_td_want(x, *c))
                .collect();
            self.obs.push(Obs::PoolWalk { inputs, wants });
            for c in keys {
                if let Some(t) = self.teardown_signal(x, c) {
                    driver.pool_wait.insert((c, t));
                }
            }
        }
        self.s.sup_mut(x).teardown = Some(driver);
        // Harness: a pool stops every child with an occupant in this step.
        if self.cfg.pool_of(x)
            && occupants
                .iter()
                .any(|t| self.s.cells.get(t).is_none_or(|c| c.order.is_none()))
        {
            self.fail(TEARDOWN_ORDER);
        }
    }

    /// One step of the teardown: signal the next key that has an occupant, or
    /// finish when none is left.
    fn teardown_step(&mut self, x: SupId) {
        let mut driver = self
            .s
            .sup_mut(x)
            .teardown
            .take()
            .expect("model invariant: driver");
        driver.current = None;
        let mut signalled = false;
        while let Some(c) = driver.walk.pop_front() {
            if let Some(t) = self.teardown_signal(x, c) {
                driver.current = Some((c, t));
                signalled = true;
                break;
            }
        }
        if signalled {
            self.s.sup_mut(x).teardown = Some(driver);
            return;
        }
        // Finish: every child has a recorded fate or the verdict.
        let reason = driver.reason;
        if let SupId::Inc(p) = x {
            if self.cfg_of(p).kg() == 0 && self.s.inc(p).stop_reason == Some(Reason::ParentKilled) {
                self.hit(Wit::ZeroGraceOrderlyTeardownCompletes);
            }
            self.check_exit_reason(x, reason);
        }
        self.check_all_settled(x);
        self.check_budget_at_exit(x);
        let tokens: Vec<Tok> = self
            .s
            .cells
            .iter()
            .filter(|(_, c)| c.sup == x)
            .map(|(t, _)| *t)
            .collect();
        for t in tokens {
            if !self.s.live(t) {
                continue;
            }
            match self.s.cell(t).exec {
                Exec::RunnerFailed => {
                    self.s
                        .cells
                        .get_mut(&t)
                        .expect("model invariant: cell")
                        .exec = Exec::Orphan;
                }
                Exec::Orphan | Exec::Returned => {}
                _ => self.fire_guard(t, Exec::Orphan),
            }
        }
        match x {
            SupId::S => {
                self.s.sup = Sup::default();
                self.s.oracle.pending_call = None;
                self.s.s_phase = SPhase::Done(reason);
            }
            SupId::Inc(p) => {
                self.s.psups.remove(&p);
                self.end(p, reason);
            }
        }
    }

    /// The reason a supervisor incarnation ends with agrees with why it left
    /// its loop.
    fn check_exit_reason(&mut self, x: SupId, reason: Reason) {
        let ok = if self.s.oracle.failed_final(x) {
            matches!(reason, Reason::Failure(_))
        } else {
            match self.o_cause(x) {
                Some(SCause::Budget | SCause::Shutdown) => reason == Reason::ParentRequest,
                Some(SCause::ParentStop) => reason == Reason::ParentRequest,
                Some(SCause::Unresponsive) => {
                    matches!(reason, Reason::Failure(Cause::Unresponsive(_)))
                }
                Some(SCause::ParentKill) => false,
                None => strength(reason) == Strength::Forced,
            }
        };
        if !ok {
            self.fail(THE_SERIAL_OUTCOME_HOLDS);
        }
    }

    /// The supervisor stops only when every occupant and every token it made
    /// has a fate or the verdict.
    fn check_all_settled(&mut self, x: SupId) {
        let occupants: Vec<Tok> = self
            .s
            .sup_ref(x)
            .ledger
            .keys()
            .filter_map(|c| self.s.occ_of(x, *c))
            .collect();
        let all: Vec<Tok> = self
            .s
            .cells
            .iter()
            .filter(|(_, c)| c.sup == x)
            .map(|(t, _)| *t)
            .collect();
        if occupants
            .iter()
            .chain(all.iter())
            .any(|t| !self.s.settled(*t))
        {
            self.fail(STOPPED_ONLY_WHEN_EVERY_CHILD_IS_SETTLED);
        }
    }
}

// ---------------------------------------------------------------------------
// The manual calls: the handler calls of the supervisor
// ---------------------------------------------------------------------------

/// What the group does with a `stop_child` before the key's own state is read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StopPrecedence {
    Plain,
    Absorbed,
    Refused,
    AsTerminate,
}

impl State {
    /// The ledger state of a key as the manual table reads it.
    fn row(&self, c: Key) -> Row {
        match self.life(c) {
            None => Row::NoSpec,
            Some(Life::Running(n)) => {
                if self.ended(n) {
                    Row::RunningEnded
                } else {
                    Row::RunningLive
                }
            }
            Some(Life::Stopping { n, kind }) => match (kind, self.ended(n)) {
                (Kind::Bounce, false) => Row::BounceLive,
                (Kind::Bounce, true) => Row::BounceEnded,
                (Kind::Terminate, false) => Row::TerminateLive,
                (Kind::Terminate, true) => Row::TerminateEnded,
            },
            Some(Life::Restarting { .. }) => Row::Attempt,
            Some(Life::Down { pend, .. }) => match pend {
                Pend::Nothing => Row::DownPlain,
                Pend::Snapshot => Row::DownSnapshot,
                Pend::Evaluate => Row::DownOwed,
            },
        }
    }

    /// The incarnation a stop-capable row concerns.
    fn row_token(&self, c: Key) -> Option<Tok> {
        match self.life(c)? {
            Life::Running(n) | Life::Stopping { n, .. } => Some(n),
            _ => None,
        }
    }
}

impl Ap<'_> {
    fn in_keep_scope(&self, c: Key) -> bool {
        self.s.sup.group.as_ref().is_some_and(|g| match g.kind {
            GKind::All => true,
            GKind::Rest => !g.outside.contains(&c),
        })
    }

    /// The key leaves the pending group's restart set.
    fn release_from_restart_set(&mut self, c: Key) {
        if let Some(g) = self.s.sup.group.as_mut() {
            match &mut g.phase {
                Phase::Stop { restart_order, .. } => restart_order.retain(|k| *k != c),
                Phase::Chain { queue, .. } => queue.retain(|k| *k != c),
            }
        }
    }

    fn stop_precedence(&self, c: Key, row: Row) -> StopPrecedence {
        let Some(g) = self.s.sup.group.as_ref() else {
            return StopPrecedence::Plain;
        };
        let permanent = self.cfg.kid(c).restart == Restart::Permanent;
        let memb = self.s.membership(c);
        let overridden = g.overrides.contains(&c);
        let front_attempt = memb == Memb::Queued && row == Row::Attempt;
        let ahead = (memb == Memb::Awaited && !overridden)
            || (memb == Memb::Queued && !front_attempt && !overridden);
        if ahead {
            return if permanent {
                StopPrecedence::Absorbed
            } else {
                StopPrecedence::AsTerminate
            };
        }
        if front_attempt {
            return StopPrecedence::Refused;
        }
        if overridden || g.restarted.contains(&c) {
            return if permanent {
                StopPrecedence::Refused
            } else {
                StopPrecedence::AsTerminate
            };
        }
        // A child started while the group pends, inside the scope the group
        // keeps down a terminate's key in, follows its own policy: its stop
        // is its terminate with the record, so it stays down through every
        // retry.
        if !permanent && memb == Memb::Outside && self.in_keep_scope(c) {
            return StopPrecedence::AsTerminate;
        }
        StopPrecedence::Plain
    }

    // ---- shared effects ---------------------------------------------------------

    /// COMMIT: the child is `Stopping` and its executor owns the stop.
    fn commit(&mut self, kind: Kind, c: Key, n: Tok) {
        self.set_life(SupId::S, c, Some(Life::Stopping { n, kind }));
        self.order(n, OrderFrom::Manual);
        self.s.caller = Some(Caller {
            c,
            t: n,
            path: Path::Commit,
            kind: CKind::Stop,
        });
    }

    /// DIED-FIRST: the child ended before the call; classify at once.
    fn died_first(&mut self, kind: Kind, c: Key, n: Tok) {
        let r = self.s.seen_reason(n);
        self.hit(Wit::ManualCallOnEndedUndrained);
        self.classify_manual(c, n, kind, r);
        self.answer = Some(Answer::Ok);
    }

    /// JOIN: no second ladder; one more ask of a vetoing `Infinity` child.
    /// With `shared`, the ask is the group's single one.
    fn join(&mut self, c: Key, n: Tok, shared: bool) {
        if self.s.live(n)
            && self.cfg.kid(c).policy == Policy::Infinity
            && vetoable(self.s.inc(n).lane)
        {
            if shared {
                if !self.s.inc(n).reasked {
                    self.raise(n, Reason::ParentRequest);
                    self.s
                        .incs
                        .get_mut(&n)
                        .expect("model invariant: inc")
                        .reasked = true;
                    self.hit(Wit::KeepJoinSharedAsk);
                }
            } else {
                self.raise(n, Reason::ParentRequest);
                self.hit(Wit::ManualJoinOneMoreAsk);
            }
        }
        if self.s.ended(n) {
            let kind = match self.s.life(c) {
                Some(Life::Stopping { kind, .. }) => kind,
                other => panic!("model invariant: a join concerns a stopping child, got {other:?}"),
            };
            let r = self.s.seen_reason(n);
            self.classify_manual(c, n, kind, r);
            self.answer = Some(Answer::Ok);
        } else if self.s.cell(n).verdict {
            self.hit(Wit::ManualJoinAnsweredUnresponsive);
            self.answer = Some(Answer::ChildUnresponsive);
        } else {
            self.s.caller = Some(Caller {
                c,
                t: n,
                path: Path::Join,
                kind: CKind::Stop,
            });
        }
    }

    fn upgrade_to_terminate(&mut self, c: Key, n: Tok) {
        if matches!(
            self.s.life(c),
            Some(Life::Stopping {
                kind: Kind::Bounce,
                ..
            })
        ) {
            self.set_life(
                SupId::S,
                c,
                Some(Life::Stopping {
                    n,
                    kind: Kind::Terminate,
                }),
            );
        }
    }

    /// OVERRIDE: a terminate of a member of a pending group; the member stays
    /// in the barrier but the group does not restart it.
    fn override_member(&mut self, c: Key, n: Tok, row: Row) {
        if let Some(g) = self.s.sup.group.as_mut() {
            g.overrides.insert(c);
        }
        self.release_from_restart_set(c);
        match row {
            Row::RunningLive => {
                if self.s.cell(n).order.is_none() {
                    self.order(n, OrderFrom::Manual);
                    self.hit(Wit::TerminateOfUnsignalledMember);
                } else {
                    let from = self.s.cell(n).order.map(|o| o.from);
                    let before = self.raised.len();
                    self.ask_once_more_counted(c, n);
                    if from == Some(OrderFrom::Manual) && self.raised.len() > before {
                        self.hit(Wit::SecondOverrideAsk);
                    }
                }
            }
            Row::BounceLive => {
                self.upgrade_to_terminate(c, n);
                self.ask_once_more_counted(c, n);
            }
            Row::TerminateLive => self.ask_once_more_counted(c, n),
            Row::BounceEnded => self.upgrade_to_terminate(c, n),
            _ => {}
        }
        if self.s.ended(n) {
            let r = self.s.seen_reason(n);
            self.classify_manual(c, n, Kind::Terminate, r);
            self.answer = Some(Answer::Ok);
        } else if self.s.cell(n).verdict {
            self.hit(Wit::OverrideAnsweredUnresponsive);
            self.answer = Some(Answer::ChildUnresponsive);
        } else {
            self.s.caller = Some(Caller {
                c,
                t: n,
                path: Path::Override,
                kind: CKind::Stop,
            });
        }
    }

    fn ask_once_more_counted(&mut self, c: Key, n: Tok) {
        let before = self.s.inc(n).reasked;
        self.ask_once_more(c, n);
        if !before && self.s.inc(n).reasked {
            self.hit(Wit::OverrideOneMoreAsk);
            if self.cfg.mark_override_asks {
                self.s
                    .incs
                    .get_mut(&n)
                    .expect("model invariant: inc")
                    .ask_by_override = true;
            }
        }
    }

    /// Dismiss the owed evaluation of a down child: its report is delivered
    /// from a snapshot that says the child was not restarted.
    fn dismiss_owed_evaluation(&mut self, c: Key, n: Tok) {
        let r = self
            .s
            .sup
            .queued
            .iter()
            .find(|(k, t, _)| *k == c && *t == n)
            .map(|e| e.2)
            .or_else(|| {
                self.s
                    .sup
                    .deaths
                    .iter()
                    .find(|(k, t, _)| *k == c && *t == n)
                    .map(|e| e.2)
            })
            .expect("model invariant: an owed evaluation has its report in a plane");
        if self.cfg.prunable(c) {
            self.remove_spec(SupId::S, c);
        } else {
            self.down(SupId::S, c, n, Pend::Snapshot);
        }
        self.s.sup.snapshots.insert((c, n), (r, Action::Removed));
        self.s.oracle.decided.insert((c, n), Action::Removed);
    }

    // ---- the calls ----------------------------------------------------------------

    fn stop_child(&mut self, c: Key) {
        let row = self.s.row(c);
        if row == Row::NoSpec {
            self.answer = Some(Answer::ChildNotFound);
            return;
        }
        match self.stop_precedence(c, row) {
            StopPrecedence::Absorbed => {
                self.hit(Wit::StopChildAbsorbed);
                if self.s.oracle.group_log.contains(&c) {
                    self.hit(Wit::StopChildAbsorbedInRetry);
                }
                self.answer = Some(Answer::Absorbed);
            }
            StopPrecedence::Refused => {
                self.hit(Wit::StopChildRefused);
                let overridden = self
                    .s
                    .sup
                    .group
                    .as_ref()
                    .is_some_and(|g| g.overrides.contains(&c));
                if overridden && self.s.membership(c) == Memb::Awaited {
                    self.hit(Wit::StopChildRefusedOverriddenAwaited);
                }
                if self.s.oracle.group_log.contains(&c) && self.s.membership(c) == Memb::Outside {
                    self.hit(Wit::StopOrRestartRefusedEarlierChain);
                }
                self.answer = Some(Answer::ChildRestarting);
            }
            StopPrecedence::AsTerminate => {
                match self.cfg.kid(c).restart {
                    Restart::Transient => self.hit(Wit::AsTerminateTransient),
                    _ => self.hit(Wit::AsTerminateTemporary),
                }
                let counted = self.s.membership(c) == Memb::Outside
                    && !self
                        .s
                        .sup
                        .group
                        .as_ref()
                        .is_some_and(|g| g.overrides.contains(&c) || g.restarted.contains(&c));
                if counted {
                    self.hit(Wit::StopChildKeptDownSpawnedSince);
                }
                self.terminate_child(c);
            }
            StopPrecedence::Plain => match row {
                Row::NoSpec => unreachable!("handled above"),
                Row::RunningLive => {
                    let n = self.s.row_token(c).expect("model invariant: token");
                    self.commit(Kind::Bounce, c, n);
                }
                Row::RunningEnded => {
                    let n = self.s.row_token(c).expect("model invariant: token");
                    self.died_first(Kind::Bounce, c, n);
                }
                Row::BounceLive | Row::TerminateLive | Row::BounceEnded | Row::TerminateEnded => {
                    let n = self.s.row_token(c).expect("model invariant: token");
                    self.join(c, n, false);
                }
                Row::Attempt | Row::DownOwed => self.answer = Some(Answer::ChildRestarting),
                Row::DownPlain | Row::DownSnapshot => self.answer = Some(Answer::Ok),
            },
        }
    }

    fn terminate_child(&mut self, c: Key) {
        let row = self.s.row(c);
        let memb = self.s.membership(c);
        let group_pending = self.s.sup.group.is_some();
        let keep_ctx = group_pending && memb == Memb::Outside && self.in_keep_scope(c);
        // The incarnation whose subtree the call waits for once it answers:
        // the one it stops, or the last one of a child already down.
        let subtree_of = self.s.row_token(c).or_else(|| {
            matches!(row, Row::DownPlain | Row::DownSnapshot | Row::DownOwed)
                .then(|| self.s.sup.pred.get(&c).copied())
                .flatten()
        });
        match (memb, row) {
            (_, Row::NoSpec) => {
                self.answer = Some(Answer::ChildNotFound);
                return;
            }
            (_, Row::Attempt) => {
                self.answer = Some(Answer::ChildRestarting);
                return;
            }
            (Memb::Outside, Row::RunningLive) => {
                let n = self.s.row_token(c).expect("model invariant: token");
                self.commit(Kind::Terminate, c, n);
            }
            (Memb::Outside, Row::RunningEnded) => {
                let n = self.s.row_token(c).expect("model invariant: token");
                self.died_first(Kind::Terminate, c, n);
            }
            (Memb::Outside, Row::BounceLive | Row::BounceEnded) => {
                let n = self.s.row_token(c).expect("model invariant: token");
                self.upgrade_to_terminate(c, n);
                self.join(c, n, keep_ctx);
            }
            (Memb::Outside, Row::TerminateLive | Row::TerminateEnded) => {
                let n = self.s.row_token(c).expect("model invariant: token");
                self.join(c, n, keep_ctx);
            }
            (Memb::Released, Row::TerminateLive | Row::TerminateEnded) => {
                let n = self.s.row_token(c).expect("model invariant: token");
                self.join(c, n, true);
            }
            (Memb::Outside | Memb::Released, Row::DownPlain | Row::DownSnapshot) => {
                self.answer = Some(Answer::Ok);
            }
            (Memb::Outside, Row::DownOwed) => {
                let Some(Life::Down { n, .. }) = self.s.life(c) else {
                    unreachable!()
                };
                self.dismiss_owed_evaluation(c, n);
                self.answer = Some(Answer::Ok);
            }
            (
                Memb::Awaited,
                Row::RunningLive
                | Row::RunningEnded
                | Row::BounceLive
                | Row::BounceEnded
                | Row::TerminateLive
                | Row::TerminateEnded,
            ) => {
                let n = self.s.row_token(c).expect("model invariant: token");
                self.override_member(c, n, row);
            }
            (Memb::Queued, Row::DownPlain | Row::DownSnapshot | Row::DownOwed) => {
                let Some(Life::Down { n, .. }) = self.s.life(c) else {
                    unreachable!()
                };
                if let Some(g) = self.s.sup.group.as_mut() {
                    g.overrides.insert(c);
                }
                self.release_from_restart_set(c);
                if row == Row::DownOwed {
                    self.dismiss_owed_evaluation(c, n);
                } else if row == Row::DownSnapshot {
                    if let Some((r, Action::RestartInitiated)) =
                        self.s.sup.snapshots.get(&(c, n)).copied()
                    {
                        self.s.sup.snapshots.insert((c, n), (r, Action::Removed));
                        self.s.oracle.decided.insert((c, n), Action::Removed);
                    }
                }
                self.answer = Some(Answer::Ok);
            }
            (m, r) => panic!("model invariant: terminate cell unreachable: {m:?} {r:?}"),
        }
        if self.answer == Some(Answer::Ok) {
            if let Some(t) = subtree_of.filter(|t| !self.s.gone(*t)) {
                if matches!(row, Row::DownPlain | Row::DownSnapshot | Row::DownOwed) {
                    self.hit(Wit::TerminateOfDownChildWaitsOnSubtree);
                }
                self.answer = None;
                self.s.caller = Some(Caller {
                    c,
                    t,
                    path: Path::Commit,
                    kind: CKind::TermSubtree,
                });
            }
        }
        let answered_or_waiting = self.answer == Some(Answer::Ok) || self.s.caller.is_some();
        if keep_ctx && answered_or_waiting && self.s.life(c).is_some() {
            let g = self.s.sup.group.as_mut().expect("model invariant: group");
            let new_key = g.overrides.insert(c);
            let outside_plan =
                new_key && matches!(g.kind, GKind::Rest) && !g.members.contains_key(&c);
            if outside_plan {
                self.hit(Wit::KeepRecordsKeyOutsideRetryPlan);
            }
        }
        if answered_or_waiting {
            self.o_stays_down_if_not_restarted(c);
        }
    }

    fn restart_child(&mut self, c: Key) {
        let row = self.s.row(c);
        let memb = self.s.membership(c);
        if row == Row::NoSpec {
            self.answer = Some(Answer::ChildNotFound);
            return;
        }
        if memb != Memb::Outside {
            self.answer = Some(Answer::ChildRestarting);
            return;
        }
        let restarted = self
            .s
            .sup
            .group
            .as_ref()
            .is_some_and(|g| g.restarted.contains(&c));
        self.answer = Some(match row {
            Row::RunningLive => {
                if restarted {
                    if self.s.oracle.group_log.contains(&c) {
                        self.hit(Wit::StopOrRestartRefusedEarlierChain);
                    }
                    Answer::ChildRestarting
                } else {
                    Answer::ChildRunning
                }
            }
            Row::DownPlain | Row::DownSnapshot => {
                self.initiate(SupId::S, c, Origin::Manual);
                self.s.oracle.stays_down.remove(&c);
                Answer::Ok
            }
            _ => Answer::ChildRestarting,
        });
    }

    fn delete_child(&mut self, c: Key) {
        let row = self.s.row(c);
        let memb = self.s.membership(c);
        self.answer = Some(match (memb, row) {
            (_, Row::NoSpec) => Answer::ChildNotFound,
            (Memb::Awaited | Memb::Queued, _) => Answer::ChildRestarting,
            (_, Row::DownPlain | Row::DownSnapshot) => {
                self.remove_spec(SupId::S, c);
                Answer::Ok
            }
            (_, Row::RunningLive) => Answer::ChildRunning,
            _ => Answer::ChildRestarting,
        });
    }

    fn spawn_child(&mut self, c: Key) {
        if self.s.life(c).is_some() {
            self.answer = Some(Answer::DuplicateChild);
            return;
        }
        // A retired record whose subtree still runs: the handler waits.
        if let Some(t) = self
            .s
            .sup
            .retired
            .get(&c)
            .copied()
            .filter(|t| !self.s.gone(*t))
        {
            self.s.caller = Some(Caller {
                c,
                t,
                path: Path::Commit,
                kind: CKind::SpawnWait,
            });
            return;
        }
        self.spawn_proceed(c);
    }

    /// The registration and creation of a spawn under a key with no spec.
    fn spawn_proceed(&mut self, c: Key) {
        if self.s.sys != SysPhase::Active {
            self.answer = Some(Answer::SystemShuttingDown);
            return;
        }
        if self.s.held.contains_key(&IdKey::S(c)) || self.s.foreign.contains(&IdKey::S(c)) {
            self.answer = Some(if self.cfg.kid(c).named {
                Answer::NameTaken
            } else {
                Answer::DuplicateChild
            });
            return;
        }
        let t = self.s.next_tok;
        self.s.next_tok += 1;
        self.s.sup.order.push(c);
        self.set_life(SupId::S, c, Some(Life::Running(t)));
        self.s
            .cells
            .insert(t, new_cell(SupId::S, c, Origin::Initial, Exec::Runner));
        self.s
            .incs
            .insert(t, new_inc(SupId::S, c, None, IdKey::S(c)));
        self.s.held.insert(IdKey::S(c), t);
        self.s.oracle.set_last(SupId::S, c, Some(t));
        self.s.oracle.stays_down.remove(&c);
        self.answer = Some(Answer::Ok);
    }

    /// Run a manual call and record, for the check of its later answer, the
    /// call a waiting caller belongs to.
    fn manual_call(&mut self, op: CallOp, c: Key) {
        let row = self.s.row(c);
        let omemb = self.s.o_membership(self.cfg, c);
        if omemb == Memb::Released {
            self.hit(Wit::ManualCallOnReleased);
        }
        match op {
            CallOp::Stop => self.stop_child(c),
            CallOp::Terminate => self.terminate_child(c),
            CallOp::Restart => self.restart_child(c),
            CallOp::Delete => self.delete_child(c),
            CallOp::Spawn => self.spawn_child(c),
        }
        if let Some(caller) = self.s.caller {
            self.s.oracle.pending_call = Some(CallRecord {
                op,
                c,
                t: caller.t,
                row,
                memb: omemb,
            });
        }
    }

    /// The oracle's `stays_down`: a Permanent child a manual operation or a
    /// failed manual restart leaves down.
    fn o_stays_down_if_not_restarted(&mut self, c: Key) {
        if !self.s.o_restarts(self.cfg, c) {
            self.s.oracle.stays_down.insert(c);
        }
    }
}

fn new_cell(sup: SupId, key: Key, origin: Origin, exec: Exec) -> Cell {
    Cell {
        sup,
        key,
        origin,
        exec,
        order: None,
        ladder: Ladder::Off,
        fate: None,
        verdict: false,
        cancelled: false,
        closed: false,
        st_fired: false,
        st_named: false,
        waits_on: None,
        refusal: None,
        owed: false,
        delivered: false,
        excused: false,
    }
}

fn new_inc(sup: SupId, key: Key, parent: Option<Tok>, id: IdKey) -> Inc {
    Inc {
        sup,
        key,
        id,
        parent,
        below: 0,
        stage: Stage::Pending,
        lane: None,
        unseen: false,
        lane_mode: LaneMode::Open,
        vetoed: false,
        reasked: false,
        ask_by_override: false,
        status: Status::Initializing,
        stop_reason: None,
        end: None,
        acked: false,
        aborted: false,
    }
}

// ---------------------------------------------------------------------------
// Steps: what can happen next, and what each step does
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[allow(clippy::enum_variant_names)]
enum Step {
    // Child tasks.
    PreStartBegins(Tok),
    PreStartDone(Tok),
    OnStartedDone(Tok),
    InitFail(Tok),
    Observe(Tok),
    GateReturns(Tok),
    BeginTurn(Tok),
    EndTurn(Tok),
    Crash(Tok),
    GoStuck(Tok),
    HookDone(Tok),
    // Restart attempts.
    AttemptCancels(Tok),
    SuccessorProceeds(Tok),
    SuccessorWaitExpires(Tok),
    Spawn(Tok),
    InitAck(Tok),
    StartTimeout(Tok),
    // Ladders, the reaper and runner failures.
    WindowEnds(Tok),
    GraceEnds(Tok),
    AbortGraceEnds(Tok),
    ReaperFires(Tok),
    ReaperKillStage(Tok),
    RunnerFails(Tok),
    LostRunnerVerdict(Tok),
    // The supervisor's loop.
    DrainAck(Tok),
    DrainOutcome(Tok),
    DrainDeath(Key, Tok, Reason),
    SVerdict,
    SObserve,
    FinalDeliver,
    FinalDeliverFails,
    FinalKill,
    FinalDone,
    SHookDone,
    SAbort,
    TeardownStep,
    TeardownKill,
    // The same, for a supervisor incarnation.
    PFinalDeliver(Tok),
    PFinalDeliverFails(Tok),
    PFinalKill(Tok),
    PFinalDone(Tok),
    PTeardownStep(Tok),
    PTeardownKill(Tok),
    CallerSees,
    CallerAbandons,
    SubtreeGone,
    SubtreeWaitExpires,
    // The system's shutdown driver.
    ShutdownReachesS,
    ShutdownKillStep,
    GlobalDeadline,
    SweepStarts,
    SweepAbort(Tok),
    DeadlineKill(Tok),
    DeadlineAbort(Tok),
    ShutdownCompletes,
    // External stimuli.
    ShutdownBegins,
    ShutdownAbandoned,
    LaterShutdown,
    Reactivate,
    StopChild(Key),
    TerminateChild(Key),
    RestartChild(Key),
    DeleteChild(Key),
    SpawnChild(Key),
    ParentStop,
    ParentKill,
    ThirdPartyRaise(Tok, Reason),
    BareKill(Tok),
    ForeignRegister(Key),
}

impl Step {
    fn is_internal(self) -> bool {
        !matches!(
            self,
            Step::InitFail(_)
                | Step::BeginTurn(_)
                | Step::Crash(_)
                | Step::GoStuck(_)
                | Step::RunnerFails(_)
                | Step::FinalDeliverFails
                | Step::PFinalDeliverFails(_)
                | Step::ShutdownBegins
                | Step::ShutdownAbandoned
                | Step::LaterShutdown
                | Step::Reactivate
                | Step::StopChild(_)
                | Step::TerminateChild(_)
                | Step::RestartChild(_)
                | Step::DeleteChild(_)
                | Step::SpawnChild(_)
                | Step::ParentStop
                | Step::ParentKill
                | Step::ThirdPartyRaise(..)
                | Step::BareKill(_)
                | Step::ForeignRegister(_)
        )
    }
}

impl State {
    /// The initial state of a configuration.
    fn initial(cfg: &Cfg) -> State {
        let mut s = State {
            sys: SysPhase::Active,
            shutdown: None,
            next_tok: 1,
            s_phase: SPhase::Loop,
            s_lane: None,
            s_unseen: false,
            s_abort: false,
            sup: Sup::default(),
            psups: BTreeMap::new(),
            cells: BTreeMap::new(),
            incs: BTreeMap::new(),
            held: BTreeMap::new(),
            foreign: BTreeSet::new(),
            reaper: BTreeMap::new(),
            sweep: BTreeMap::new(),
            caller: None,
            left: cfg.budgets,
            oracle: Oracle::default(),
        };
        for c in 0..cfg.initial as Key {
            let t = s.next_tok;
            s.next_tok += 1;
            s.sup.order.push(c);
            s.sup.ledger.insert(c, Life::Running(t));
            s.cells
                .insert(t, new_cell(SupId::S, c, Origin::Initial, Exec::Runner));
            s.incs.insert(t, new_inc(SupId::S, c, None, IdKey::S(c)));
            s.held.insert(IdKey::S(c), t);
            s.sup.pred.insert(c, t);
            s.oracle.set_last(SupId::S, c, Some(t));
        }
        s
    }

    /// Every step enabled in this state.
    fn enabled(&self, cfg: &Cfg) -> Vec<Step> {
        let mut v = Vec::new();
        self.enabled_children(cfg, &mut v);
        self.enabled_attempts_and_ladders(cfg, &mut v);
        self.enabled_supervisor(cfg, &mut v);
        self.enabled_shutdown(&mut v);
        self.enabled_sweep_entries(&mut v);
        self.enabled_external(cfg, &mut v);
        v
    }

    fn enabled_children(&self, cfg: &Cfg, v: &mut Vec<Step>) {
        for (&t, inc) in &self.incs {
            if inc.end.is_some() {
                continue;
            }
            let cell = self.cell(t);
            let kid = cfg.kid_of(inc.sup, inc.key);
            let crash_left = if inc.sup == SupId::S {
                self.left.crash_s
            } else {
                self.left.crash_w
            };
            match inc.stage {
                Stage::Pending => v.push(Step::PreStartBegins(t)),
                Stage::PreStart => v.push(Step::PreStartDone(t)),
                Stage::OnStarted => v.push(Step::OnStartedDone(t)),
                Stage::Idle => {
                    if inc.unseen {
                        v.push(Step::Observe(t));
                    } else {
                        if kid.turns {
                            v.push(Step::BeginTurn(t));
                        }
                        if crash_left > 0 {
                            v.push(Step::Crash(t));
                        }
                    }
                }
                Stage::Turn => {
                    v.push(Step::EndTurn(t));
                    if crash_left > 0 {
                        v.push(Step::Crash(t));
                    }
                }
                Stage::Gate(_) => v.push(Step::GateReturns(t)),
                Stage::Hook => v.push(Step::HookDone(t)),
                Stage::Stuck(_) | Stage::Done | Stage::Final | Stage::Teardown => {}
            }
            if kid.may_stick
                && self.left.stuck > 0
                && matches!(inc.stage, Stage::Turn | Stage::PreStart | Stage::OnStarted)
            {
                v.push(Step::GoStuck(t));
            }
            if cell.exec == Exec::Attempt(AttemptStage::Init)
                && cell.sup == SupId::S
                && matches!(inc.stage, Stage::PreStart | Stage::OnStarted)
                && self.left.init_fail > 0
            {
                v.push(Step::InitFail(t));
            }
        }
    }

    fn enabled_attempts_and_ladders(&self, cfg: &Cfg, v: &mut Vec<Step>) {
        for (&t, cell) in &self.cells {
            let live = self.live(t);
            match cell.exec {
                Exec::Attempt(AttemptStage::Wait) => {
                    let w = cell.waits_on.expect("model invariant: a waiting attempt");
                    if cell.order.is_some() {
                        v.push(Step::AttemptCancels(t));
                    } else {
                        if self.gone(w) {
                            v.push(Step::SuccessorProceeds(t));
                        }
                        if self.expirable(cfg, w) {
                            v.push(Step::SuccessorWaitExpires(t));
                        }
                    }
                    if self.left.runner_fail > 0 {
                        v.push(Step::RunnerFails(t));
                    }
                }
                Exec::Attempt(AttemptStage::Factory) => {
                    if cell.order.is_some() {
                        v.push(Step::AttemptCancels(t));
                    } else {
                        v.push(Step::Spawn(t));
                    }
                    if self.left.runner_fail > 0 {
                        v.push(Step::RunnerFails(t));
                    }
                }
                Exec::Attempt(AttemptStage::Init) if live => {
                    let acked = self.inc(t).acked;
                    if cell.order.is_none() && !cell.st_fired && cell.ladder == Ladder::Off && acked
                    {
                        v.push(Step::InitAck(t));
                    }
                    if cfg.kid_of(cell.sup, cell.key).start_timeout
                        && !acked
                        && matches!(
                            cell.ladder,
                            Ladder::Off | Ladder::Window | Ladder::WindowInf
                        )
                    {
                        v.push(Step::StartTimeout(t));
                    }
                    if self.left.runner_fail > 0 {
                        v.push(Step::RunnerFails(t));
                    }
                }
                Exec::Runner if live && self.left.runner_fail > 0 => v.push(Step::RunnerFails(t)),
                _ => {}
            }
            let ladder_runs =
                live && matches!(cell.exec, Exec::Runner | Exec::Attempt(AttemptStage::Init));
            if ladder_runs {
                match cell.ladder {
                    Ladder::Window if self.gate_open_for(t) => v.push(Step::WindowEnds(t)),
                    Ladder::KillGrace => v.push(Step::GraceEnds(t)),
                    Ladder::AbortGrace => v.push(Step::AbortGraceEnds(t)),
                    _ => {}
                }
            }
            match self.reaper.get(&t) {
                Some(Reap::AbortDue) if live => v.push(Step::ReaperFires(t)),
                Some(Reap::KillThenAbort) if live && self.gate_open_for(t) => {
                    v.push(Step::ReaperKillStage(t));
                }
                _ => {}
            }
            if cell.exec == Exec::RunnerFailed
                && live
                && self.inc(t).aborted
                && !cell.verdict
                && self.has_sup(cell.sup)
            {
                v.push(Step::LostRunnerVerdict(t));
            }
        }
    }

    fn enabled_supervisor(&self, cfg: &Cfg, v: &mut Vec<Step>) {
        if self.drain_enabled() {
            for (&m, (_, o)) in &self.sup.outcomes {
                if *o == Outcome::Adopted {
                    v.push(Step::DrainAck(m));
                } else {
                    v.push(Step::DrainOutcome(m));
                }
            }
            for &(c, t, r) in &self.sup.deaths {
                v.push(Step::DrainDeath(c, t, r));
            }
        }
        if self.loop_free() && self.verdict_exit().is_some() {
            v.push(Step::SVerdict);
        }
        match self.s_phase {
            SPhase::Loop => {
                if self.s_unseen && self.caller.is_none() {
                    v.push(Step::SObserve);
                }
            }
            SPhase::Final(_) => {
                if self.s_lane == Some(Reason::Kill) {
                    v.push(Step::FinalKill);
                } else if self.sup.final_q.is_empty() {
                    v.push(Step::FinalDone);
                } else {
                    v.push(Step::FinalDeliver);
                    if self.left.final_fail > 0 {
                        v.push(Step::FinalDeliverFails);
                    }
                }
            }
            SPhase::Hook(_) => v.push(Step::SHookDone),
            SPhase::Teardown => {
                if self.s_lane == Some(Reason::Kill) && self.s_unseen {
                    v.push(Step::TeardownKill);
                } else if let Some(d) = &self.sup.teardown {
                    let ready = if cfg.is_pool() {
                        d.pool_wait.iter().all(|(_, t)| self.settled_for_wait(*t))
                    } else {
                        d.current.is_none_or(|(_, t)| self.settled_for_wait(t))
                    };
                    if ready {
                        v.push(Step::TeardownStep);
                    }
                }
            }
            SPhase::Done(_) => {}
        }
        if self.s_abort && !matches!(self.s_phase, SPhase::Done(_)) {
            v.push(Step::SAbort);
        }
        for (&p, sup) in &self.psups {
            let x = SupId::Inc(p);
            let inc = self.inc(p);
            if self.drain_enabled_of(x) {
                for (&m, (_, o)) in &sup.outcomes {
                    if *o == Outcome::Adopted {
                        v.push(Step::DrainAck(m));
                    } else {
                        v.push(Step::DrainOutcome(m));
                    }
                }
                for &(c, t, r) in &sup.deaths {
                    v.push(Step::DrainDeath(c, t, r));
                }
            }
            match inc.stage {
                Stage::Final => {
                    if inc.lane == Some(Reason::Kill) {
                        v.push(Step::PFinalKill(p));
                    } else if sup.final_q.is_empty() {
                        v.push(Step::PFinalDone(p));
                    } else {
                        v.push(Step::PFinalDeliver(p));
                        if self.left.final_fail > 0 {
                            v.push(Step::PFinalDeliverFails(p));
                        }
                    }
                }
                Stage::Teardown => {
                    if inc.lane == Some(Reason::Kill) && inc.unseen {
                        v.push(Step::PTeardownKill(p));
                    } else if let Some(d) = &sup.teardown {
                        let ready = if cfg.pool_of(x) {
                            d.pool_wait.iter().all(|(_, t)| self.settled_for_wait(*t))
                        } else {
                            d.current.is_none_or(|(_, t)| self.settled_for_wait(t))
                        };
                        if ready {
                            v.push(Step::PTeardownStep(p));
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(caller) = self.caller {
            match caller.kind {
                CKind::Stop => {
                    if self.settled_for_wait(caller.t) {
                        v.push(Step::CallerSees);
                    }
                }
                CKind::TermSubtree | CKind::SpawnWait => {
                    if self.gone(caller.t) {
                        v.push(Step::SubtreeGone);
                    }
                    if self.expirable(cfg, caller.t) {
                        v.push(Step::SubtreeWaitExpires);
                    }
                }
            }
            v.push(Step::CallerAbandons);
        }
    }

    /// The verdict exit: the escalation that leaves the loop when a barrier or
    /// a chain waits only on incarnations that carry the verdict. Gives the
    /// key the failure names.
    fn verdict_exit(&self) -> Option<Key> {
        let g = self.sup.group.as_ref()?;
        match &g.phase {
            Phase::Stop {
                awaiting, queue, ..
            } => {
                let head_verdict = queue
                    .front()
                    .and_then(|h| awaiting.get(h))
                    .is_some_and(|t| self.cell(*t).verdict);
                let all_verdict = queue.is_empty()
                    && !awaiting.is_empty()
                    && awaiting.values().all(|t| self.cell(*t).verdict);
                if head_verdict || all_verdict {
                    // The first such member in reverse start order.
                    self.sup
                        .order
                        .iter()
                        .rev()
                        .find(|k| awaiting.get(k).is_some_and(|t| self.cell(*t).verdict))
                        .copied()
                        .or(Some(0))
                } else {
                    None
                }
            }
            Phase::Chain { queue, started } => {
                let f = *queue.front()?;
                match self.life(f) {
                    Some(Life::Restarting { next, .. })
                        if started.get(&f) == Some(&next) && self.cell(next).verdict =>
                    {
                        Some(f)
                    }
                    _ => None,
                }
            }
        }
    }

    fn enabled_external(&self, cfg: &Cfg, v: &mut Vec<Step>) {
        let free = self.loop_free();
        if free {
            for c in 0..cfg.kids.len() as Key {
                if self.left.manual > 0 {
                    v.push(Step::StopChild(c));
                    v.push(Step::TerminateChild(c));
                    v.push(Step::RestartChild(c));
                    v.push(Step::DeleteChild(c));
                }
                let spare_unused =
                    (c as usize) >= cfg.initial && !self.oracle.spawned_before(SupId::S, c);
                if spare_unused {
                    if self.left.new_spawn > 0 {
                        v.push(Step::SpawnChild(c));
                    }
                } else if self.left.manual > 0 {
                    v.push(Step::SpawnChild(c));
                }
            }
        }
        if self.s_phase == SPhase::Loop
            && self.s_lane.is_none()
            && (self.left.parent_stop > 0 || self.left.parent > 0)
        {
            v.push(Step::ParentStop);
        }
        if matches!(
            self.s_phase,
            SPhase::Loop | SPhase::Final(_) | SPhase::Hook(_) | SPhase::Teardown
        ) && self.s_lane != Some(Reason::Kill)
            && (self.left.parent_kill > 0 || self.left.parent > 0)
        {
            v.push(Step::ParentKill);
        }
        if self.left.third > 0 {
            for (&t, inc) in &self.incs {
                if inc.end.is_none() && inc.sup == SupId::S {
                    for r in &cfg.third_reasons {
                        v.push(Step::ThirdPartyRaise(t, *r));
                    }
                }
            }
        }
        if self.left.bare_kill > 0 {
            for (&t, inc) in &self.incs {
                if inc.end.is_none() && inc.sup == SupId::S && cfg.bare_kill_targets(inc.key) {
                    v.push(Step::BareKill(t));
                }
            }
        }
        if self.left.foreign > 0 {
            for c in 0..cfg.kids.len() as Key {
                v.push(Step::ForeignRegister(c));
            }
        }
        if self.sys == SysPhase::Active && self.shutdown.is_none() && self.left.shut_begin > 0 {
            v.push(Step::ShutdownBegins);
        }
        if let Some(sh) = &self.shutdown {
            let driven = self.sup_alive() || (sh.sweep_started && sh.step != ShutStep::Swept);
            if !sh.abandoned && driven && self.left.shut_abandon > 0 {
                v.push(Step::ShutdownAbandoned);
            }
            if self.sys == SysPhase::ShuttingDown && sh.abandoned && self.left.shut_later > 0 {
                v.push(Step::LaterShutdown);
            }
        }
        if self.sys == SysPhase::Defunct && self.left.shut_reactivate > 0 {
            v.push(Step::Reactivate);
        }
    }

    /// Whether a deadline entry on a live incarnation waits for its global
    /// deadline.
    fn sweep_pending(&self) -> bool {
        self.sweep
            .iter()
            .any(|(t, stage)| *stage == SweepStage::Armed && self.live(*t))
    }

    /// The steps of the deadline entries: they belong to the system's reaper,
    /// so they run whether or not the shutdown call is still driven.
    fn enabled_sweep_entries(&self, v: &mut Vec<Step>) {
        if self.sweep_pending() {
            v.push(Step::GlobalDeadline);
        }
        for (&t, stage) in &self.sweep {
            if !self.live(t) {
                continue;
            }
            match stage {
                SweepStage::Armed => {}
                SweepStage::KillDue => v.push(Step::DeadlineKill(t)),
                SweepStage::AbortDue if !self.inc(t).aborted => v.push(Step::DeadlineAbort(t)),
                SweepStage::AbortDue => {}
            }
        }
    }

    /// The shutdown driver's own steps.
    fn enabled_shutdown(&self, v: &mut Vec<Step>) {
        let Some(sh) = &self.shutdown else {
            return;
        };
        if sh.abandoned {
            return;
        }
        if self.sup_alive() {
            if !sh.reached {
                v.push(Step::ShutdownReachesS);
            }
            if sh.step == ShutStep::Requested && sh.reached {
                v.push(Step::ShutdownKillStep);
            }
            return;
        }
        if !sh.sweep_started {
            if sh.step != ShutStep::Swept {
                v.push(Step::SweepStarts);
            }
            return;
        }
        let orphans: Vec<Tok> = self
            .cells
            .iter()
            .filter(|(t, c)| c.exec == Exec::Orphan && self.live(**t))
            .map(|(t, _)| *t)
            .collect();
        if !orphans.is_empty() && !sh.deadline && !self.sweep_pending() {
            v.push(Step::GlobalDeadline);
        }
        if sh.deadline {
            for &t in &orphans {
                if !self.inc(t).aborted {
                    v.push(Step::SweepAbort(t));
                }
            }
        }
        if sh.step != ShutStep::Swept
            && orphans.iter().all(|t| self.inc(*t).aborted)
            && self.sys == SysPhase::ShuttingDown
        {
            v.push(Step::ShutdownCompletes);
        }
    }
}

/// The result of applying a step to a state: the post-state (not canonical),
/// the failures the step's own checks recorded, and what it witnessed.
struct Applied {
    post: State,
    fails: Vec<&'static str>,
    wit: Vec<Wit>,
    answer: Option<Answer>,
    raised: Vec<(Tok, Reason)>,
    obs: Vec<Obs>,
}

fn apply(cfg: &Cfg, pre: &State, step: Step) -> Applied {
    let mut ap = Ap::new(cfg, pre);
    ap.run(step);
    Applied {
        post: ap.s,
        fails: ap.fails,
        wit: ap.wit,
        answer: ap.answer,
        raised: ap.raised,
        obs: ap.obs,
    }
}

impl Ap<'_> {
    fn run(&mut self, step: Step) {
        // Harness: a supervisor incarnation whose gate accepts the request its
        // lane holds leaves its loop because its parent asked it to stop.
        let gate_exit = match step {
            Step::GateReturns(t) if self.s.x_in_loop(SupId::Inc(t)) => Some(t),
            _ => None,
        };
        self.run_logic(step);
        if let Some(t) = gate_exit {
            if self.s.has_sup(SupId::Inc(t))
                && !self.s.x_in_loop(SupId::Inc(t))
                && !self.s.oracle.p_cause.contains_key(&t)
                && self.s.oracle.p_req.contains_key(&t)
            {
                self.s.oracle.p_cause.insert(t, SCause::ParentStop);
            }
        }
    }

    fn run_logic(&mut self, step: Step) {
        match step {
            Step::PreStartBegins(t) => {
                let lane = self.s.inc(t).lane;
                self.obs.push(Obs::InitRead {
                    lane,
                    kill_now: lane == Some(Reason::Kill),
                });
                if self.s.inc(t).lane == Some(Reason::Kill) {
                    self.s
                        .incs
                        .get_mut(&t)
                        .expect("model invariant: inc")
                        .stop_reason = Some(Reason::Kill);
                    self.close(t);
                } else {
                    self.s.incs.get_mut(&t).expect("model invariant: inc").stage = Stage::PreStart;
                }
            }
            Step::PreStartDone(t) => {
                let lane = self.s.inc(t).lane;
                self.obs.push(Obs::InitRead {
                    lane,
                    kill_now: lane == Some(Reason::Kill),
                });
                if self.s.inc(t).lane == Some(Reason::Kill) {
                    self.s
                        .incs
                        .get_mut(&t)
                        .expect("model invariant: inc")
                        .stop_reason = Some(Reason::Kill);
                    self.close(t);
                } else {
                    self.s.incs.get_mut(&t).expect("model invariant: inc").stage = Stage::OnStarted;
                    if self.cfg_of(t).is_supervisor {
                        self.s.psups.insert(t, Sup::default());
                        for w in 0..self.cfg.workers.len() as Key {
                            if let Err(cause) = self.spawn_worker(t, w) {
                                self.s
                                    .incs
                                    .get_mut(&t)
                                    .expect("model invariant: inc")
                                    .stop_reason = Some(Reason::Failure(cause));
                                self.close(t);
                                break;
                            }
                        }
                    }
                }
            }
            Step::OnStartedDone(t) => {
                {
                    let i = self.s.incs.get_mut(&t).expect("model invariant: inc");
                    i.status = Status::Running;
                    i.acked = true;
                    i.stage = Stage::Idle;
                }
                // Harness: a supervisor's loop begins here; every end report
                // already in its planes is owed from now on.
                if self.s.has_sup(SupId::Inc(t)) {
                    let sup = self.s.sup_ref(SupId::Inc(t));
                    let mut owe: Vec<Tok> = sup.deaths.iter().map(|d| d.1).collect();
                    owe.extend(
                        sup.outcomes
                            .iter()
                            .filter(|(_, (_, o))| {
                                matches!(o, Outcome::Failed(_) | Outcome::Stopped(_))
                            })
                            .map(|(m, _)| *m),
                    );
                    for m in owe {
                        if let Some(c) = self.s.cells.get_mut(&m) {
                            c.owed = true;
                        }
                    }
                }
            }
            Step::InitFail(t) => {
                self.s.left.init_fail -= 1;
                self.s
                    .incs
                    .get_mut(&t)
                    .expect("model invariant: inc")
                    .stop_reason = Some(Reason::Failure(Cause::InitErr));
                self.close(t);
            }
            Step::Observe(t) => self.observe(t),
            Step::GateReturns(t) => self.gate_returns(t),
            Step::BeginTurn(t) => {
                self.s.incs.get_mut(&t).expect("model invariant: inc").stage = Stage::Turn
            }
            Step::EndTurn(t) => {
                self.s.incs.get_mut(&t).expect("model invariant: inc").stage = Stage::Idle
            }
            Step::Crash(t) => {
                if self.s.sys == SysPhase::ShuttingDown
                    && self.s.shutdown.is_some_and(|sh| sh.abandoned)
                {
                    self.hit(Wit::ChildCrashAfterAbandonedShutdown);
                }
                if self.s.inc(t).sup == SupId::S {
                    self.s.left.crash_s -= 1;
                } else {
                    self.s.left.crash_w -= 1;
                }
                if self.s.inc(t).lane == Some(Reason::Kill) {
                    self.s
                        .incs
                        .get_mut(&t)
                        .expect("model invariant: inc")
                        .stop_reason = Some(Reason::Kill);
                    self.close(t);
                } else {
                    self.leave_loop(t, Reason::Failure(Cause::Crash));
                }
            }
            Step::GoStuck(t) => {
                self.s.left.stuck -= 1;
                let i = self.s.incs.get_mut(&t).expect("model invariant: inc");
                let at = match i.stage {
                    Stage::Turn => At::Turn,
                    Stage::PreStart => At::PreStart,
                    _ => At::OnStarted,
                };
                i.stage = Stage::Stuck(at);
            }
            Step::HookDone(t) => {
                // A failed final delivery makes the exit reason a failure once
                // `on_stopped` has returned.
                if self.s.has_sup(SupId::Inc(t)) && self.s.sup_ref(SupId::Inc(t)).final_err {
                    self.s
                        .incs
                        .get_mut(&t)
                        .expect("model invariant: inc")
                        .stop_reason = Some(Reason::Failure(Cause::Crash));
                }
                self.close(t);
            }
            Step::AttemptCancels(m) => {
                if self.pre.cell(m).exec == Exec::Attempt(AttemptStage::Wait) {
                    if let Some(w) = self.pre.cell(m).waits_on {
                        if !self.pre.gone(w) && self.pre.expirable(self.cfg, w) {
                            self.hit(Wit::OrderBesideAnExpirableWait);
                        }
                    }
                }
                if self.pre.cell(m).exec == Exec::Attempt(AttemptStage::Factory) {
                    self.obs.push(Obs::Attempt {
                        stage: AtStage::Factory,
                        event: AtEvent::Begin { ordered: true },
                        ladder: Ladder::Off,
                        kill_stored: false,
                        acked: false,
                        want: AtWant::Cancel,
                    });
                }
                let key = self.s.cell(m).key;
                let x = self.s.cell(m).sup;
                {
                    let c = self.s.cells.get_mut(&m).expect("model invariant: cell");
                    c.cancelled = true;
                    c.exec = Exec::Returned;
                }
                self.put_outcome(x, m, key, Outcome::Cancelled);
            }
            Step::SuccessorProceeds(m) => {
                self.s
                    .cells
                    .get_mut(&m)
                    .expect("model invariant: cell")
                    .exec = Exec::Attempt(AttemptStage::Factory);
            }
            Step::SuccessorWaitExpires(m) => {
                let key = self.s.cell(m).key;
                let x = self.s.cell(m).sup;
                {
                    let c = self.s.cells.get_mut(&m).expect("model invariant: cell");
                    c.closed = true;
                    c.exec = Exec::Returned;
                }
                self.hit(Wit::SuccessorWaitExpired);
                self.put_outcome(x, m, key, Outcome::PredecessorUnresponsive);
            }
            Step::Spawn(m) => self.attempt_spawns(m),
            Step::InitAck(m) => {
                self.obs.push(Obs::Attempt {
                    stage: AtStage::Init,
                    event: AtEvent::Ack {
                        ordered: false,
                        ended: false,
                    },
                    ladder: self.s.cell(m).ladder,
                    kill_stored: self.s.inc(m).lane == Some(Reason::Kill),
                    acked: true,
                    want: AtWant::Adopt,
                });
                let key = self.s.cell(m).key;
                let x = self.s.cell(m).sup;
                self.put_outcome(x, m, key, Outcome::Adopted);
                self.s
                    .cells
                    .get_mut(&m)
                    .expect("model invariant: cell")
                    .exec = Exec::Attempt(AttemptStage::InTransit);
            }
            Step::StartTimeout(m) => {
                self.obs.push(Obs::Attempt {
                    stage: AtStage::Init,
                    event: AtEvent::StartTimeout,
                    ladder: self.pre.cell(m).ladder,
                    kill_stored: self.pre.inc(m).lane == Some(Reason::Kill),
                    acked: self.pre.inc(m).acked,
                    want: AtWant::KillPath {
                        raise: self.pre.inc(m).lane != Some(Reason::Kill),
                    },
                });
                if self.pre.inc(m).lane != Some(Reason::Kill) {
                    self.s
                        .cells
                        .get_mut(&m)
                        .expect("model invariant: cell")
                        .st_named = true;
                }
                if self.s.inc(m).lane != Some(Reason::Kill) {
                    self.raise(m, Reason::Kill);
                    self.s
                        .cells
                        .get_mut(&m)
                        .expect("model invariant: cell")
                        .st_fired = true;
                }
                self.s
                    .cells
                    .get_mut(&m)
                    .expect("model invariant: cell")
                    .ladder = Ladder::KillGrace;
            }
            Step::WindowEnds(t) => {
                self.raise(t, Reason::Kill);
                self.s
                    .cells
                    .get_mut(&t)
                    .expect("model invariant: cell")
                    .ladder = Ladder::KillGrace;
            }
            Step::GraceEnds(t) => {
                self.abort(t);
                if self.s.live(t) {
                    self.s
                        .cells
                        .get_mut(&t)
                        .expect("model invariant: cell")
                        .ladder = Ladder::AbortGrace;
                }
            }
            Step::AbortGraceEnds(t) => {
                if self.s.cell(t).exec == Exec::Attempt(AttemptStage::Init) {
                    self.obs.push(Obs::Attempt {
                        stage: AtStage::Init,
                        event: AtEvent::LadderSpent,
                        ladder: Ladder::AbortGrace,
                        kill_stored: self.s.inc(t).lane == Some(Reason::Kill),
                        acked: self.s.inc(t).acked,
                        want: AtWant::WriteVerdict,
                    });
                }
                if matches!(self.s.inc(t).stage, Stage::Stuck(_))
                    && self.s.cell(t).exec == Exec::Attempt(AttemptStage::Init)
                {
                    self.hit(Wit::InitOutlivesStartTimeoutAndLadder);
                }
                let c = self.s.cells.get_mut(&t).expect("model invariant: cell");
                c.verdict = true;
                c.ladder = Ladder::Spent;
            }
            Step::ReaperFires(t) => {
                self.abort(t);
                self.s.reaper.remove(&t);
            }
            Step::ReaperKillStage(t) => {
                self.raise(t, Reason::Kill);
                self.s.reaper.insert(t, Reap::AbortDue);
            }
            Step::RunnerFails(t) => self.runner_fails(t),
            Step::LostRunnerVerdict(t) => {
                self.hit(Wit::StuckMemberRunnerFailedVerdict);
                if self.s.cell(t).sup != SupId::S
                    && self.s.s_lane == Some(Reason::Kill)
                    && !self.s.sup_alive()
                {
                    self.hit(Wit::LostRunnerVerdictAfterGrandparentKill);
                }
                self.s
                    .cells
                    .get_mut(&t)
                    .expect("model invariant: cell")
                    .verdict = true;
            }
            Step::DrainAck(m) => {
                let x = self.s.cell(m).sup;
                self.drain_ack(x, m)
            }
            Step::DrainOutcome(m) => {
                let x = self.s.cell(m).sup;
                self.drain_outcome(x, m)
            }
            Step::DrainDeath(c, t, r) => {
                let x = self.s.cell(t).sup;
                self.s.sup_mut(x).deaths.remove(&(c, t, r));
                self.route_death(x, c, t, r);
            }
            Step::SVerdict => self.s_verdict(),
            Step::SObserve => self.s_observe(),
            Step::FinalDeliver => self.final_deliver(SupId::S, false),
            Step::FinalDeliverFails => self.final_deliver(SupId::S, true),
            Step::PFinalDeliver(p) => self.final_deliver(SupId::Inc(p), false),
            Step::PFinalDeliverFails(p) => self.final_deliver(SupId::Inc(p), true),
            Step::PFinalKill(p) => {
                if !self.s.sup_ref(SupId::Inc(p)).final_q.is_empty() {
                    self.hit(Wit::KillStopsFinalDrain);
                }
                self.close(p);
            }
            Step::PFinalDone(p) => {
                self.s.incs.get_mut(&p).expect("model invariant: inc").stage = Stage::Hook;
            }
            Step::PTeardownStep(p) => self.teardown_step(SupId::Inc(p)),
            Step::PTeardownKill(p) => {
                self.drop_supervision(SupId::Inc(p));
                self.end(p, Reason::Kill);
            }
            Step::FinalKill => {
                if !self.s.sup.final_q.is_empty() {
                    self.hit(Wit::KillStopsFinalDrain);
                }
                self.drop_supervision(SupId::S);
            }
            Step::FinalDone => {
                if let SPhase::Final(r) = self.s.s_phase {
                    self.s.s_phase = SPhase::Hook(r);
                }
            }
            Step::SHookDone => self.s_hook_done(),
            Step::SAbort => self.drop_supervision(SupId::S),
            Step::TeardownStep => self.teardown_step(SupId::S),
            Step::TeardownKill => self.drop_supervision(SupId::S),
            Step::CallerSees => self.caller_sees(),
            Step::SubtreeGone => self.subtree_gone(),
            Step::SubtreeWaitExpires => self.subtree_wait_expires(),
            Step::CallerAbandons => {
                if self
                    .s
                    .caller
                    .and_then(|k| self.s.cells.get(&k.t))
                    .is_some_and(|c| {
                        matches!(
                            c.ladder,
                            Ladder::Window | Ladder::KillGrace | Ladder::AbortGrace
                        )
                    })
                {
                    self.hit(Wit::CallerAbandonsMidLadder);
                }
                self.s.caller = None;
                self.s.oracle.pending_call = None;
            }
            Step::StopChild(c) => {
                self.spend_manual(c);
                self.manual_call(CallOp::Stop, c);
            }
            Step::TerminateChild(c) => {
                self.spend_manual(c);
                self.manual_call(CallOp::Terminate, c);
            }
            Step::RestartChild(c) => {
                self.spend_manual(c);
                self.manual_call(CallOp::Restart, c);
            }
            Step::DeleteChild(c) => {
                self.spend_manual(c);
                self.manual_call(CallOp::Delete, c);
            }
            Step::SpawnChild(c) => {
                let spare_unused =
                    (c as usize) >= self.cfg.initial && !self.s.oracle.spawned_before(SupId::S, c);
                if spare_unused {
                    self.s.left.new_spawn -= 1;
                } else {
                    self.spend_manual(c);
                }
                self.manual_call(CallOp::Spawn, c);
            }
            Step::ParentStop => {
                if self.s.left.parent_stop > 0 {
                    self.s.left.parent_stop -= 1;
                } else {
                    self.s.left.parent -= 1;
                }
                self.s_raise(Reason::ParentRequest);
            }
            Step::ParentKill => {
                if self.s.left.parent_kill > 0 {
                    self.s.left.parent_kill -= 1;
                } else {
                    self.s.left.parent -= 1;
                }
                if self.s.s_phase == SPhase::Teardown {
                    self.hit(Wit::KillDuringTeardown);
                }
                if matches!(self.s.s_phase, SPhase::Hook(_)) {
                    self.hit(Wit::KillDuringSupervisorHook);
                }
                self.s_raise(Reason::Kill);
                self.s.s_abort = true;
                if self.s.s_phase == SPhase::Loop && self.s.oracle.s_cause.is_none() {
                    self.s.oracle.s_cause = Some(SCause::ParentKill);
                }
            }
            Step::ThirdPartyRaise(t, r) => {
                self.s.left.third -= 1;
                if r == Reason::Graceful && self.s.inc(t).lane == Some(Reason::ParentRequest) {
                    self.hit(Wit::ThirdGracefulOverParentRequest);
                }
                self.raise(t, r);
            }
            Step::BareKill(t) => {
                self.s.left.bare_kill -= 1;
                if matches!(self.s.inc(t).stage, Stage::Pending | Stage::PreStart) {
                    self.hit(Wit::KillDuringPreStart);
                }
                if self.s.inc(t).stage == Stage::Hook {
                    self.hit(Wit::KillDuringHook);
                }
                self.raise(t, Reason::Kill);
                if self.s.inc(t).lane == Some(Reason::Kill) {
                    self.s.reaper.insert(t, Reap::AbortDue);
                }
            }
            Step::ForeignRegister(c) => {
                self.s.left.foreign -= 1;
                if self.s.sys == SysPhase::Active
                    && !self.s.held.contains_key(&IdKey::S(c))
                    && !self.s.foreign.contains(&IdKey::S(c))
                {
                    self.s.foreign.insert(IdKey::S(c));
                }
            }
            Step::ShutdownBegins => {
                self.s.left.shut_begin -= 1;
                self.s.sys = SysPhase::ShuttingDown;
                self.s.shutdown = Some(Shut {
                    step: ShutStep::Requested,
                    reached: false,
                    abandoned: false,
                    deadline: false,
                    sweep_started: false,
                });
            }
            Step::ShutdownReachesS => {
                self.s_raise(Reason::ParentRequest);
                if let Some(sh) = self.s.shutdown.as_mut() {
                    sh.reached = true;
                }
            }
            Step::ShutdownKillStep => {
                self.s_raise(Reason::Kill);
                self.s.s_abort = true;
                if let Some(sh) = self.s.shutdown.as_mut() {
                    sh.step = ShutStep::KillRaised;
                }
                if self.s.oracle.s_cause.is_none() {
                    self.s.oracle.s_cause = Some(SCause::Shutdown);
                }
            }
            Step::ShutdownAbandoned => {
                self.s.left.shut_abandon -= 1;
                if let Some(sh) = self.s.shutdown.as_mut() {
                    sh.abandoned = true;
                }
            }
            Step::LaterShutdown => {
                self.s.left.shut_later -= 1;
                // Harness: the call is recorded before the logic runs.
                self.s.oracle.later_shutdown = true;
                if let Some(sh) = self.s.shutdown.as_mut() {
                    sh.abandoned = false;
                }
            }
            Step::Reactivate => {
                self.s.left.shut_reactivate -= 1;
                if self.s.sys == SysPhase::Defunct {
                    self.s.sys = SysPhase::Active;
                    self.s.shutdown = None;
                }
            }
            Step::GlobalDeadline => {
                if let Some(sh) = self.s.shutdown.as_mut() {
                    if sh.sweep_started {
                        sh.deadline = true;
                    }
                }
                // Every entry armed so far shares the deadline that passed.
                for stage in self.s.sweep.values_mut() {
                    if *stage == SweepStage::Armed {
                        *stage = SweepStage::KillDue;
                    }
                }
            }
            Step::SweepStarts => {
                // Harness: every live incarnation is a leftover, whatever the
                // logic arms for it.
                let live: Vec<Tok> = self
                    .s
                    .incs
                    .iter()
                    .filter(|(_, i)| i.end.is_none())
                    .map(|(t, _)| *t)
                    .collect();
                self.s.oracle.sweep_leftovers = live.iter().copied().collect();
                // Each leftover gets the system's deadline entry: its `Kill` at
                // the global deadline, then its abort.
                for t in live {
                    self.s.sweep.insert(t, SweepStage::Armed);
                }
                if let Some(sh) = self.s.shutdown.as_mut() {
                    sh.sweep_started = true;
                }
            }
            Step::SweepAbort(t) => self.abort(t),
            Step::DeadlineKill(t) => {
                if self.s.shutdown.is_some_and(|sh| sh.abandoned) {
                    self.hit(Wit::DeadlineEntryFiresAfterAbandon);
                }
                self.raise(t, Reason::Kill);
                self.s.sweep.insert(t, SweepStage::AbortDue);
            }
            Step::DeadlineAbort(t) => {
                self.abort(t);
            }
            Step::ShutdownCompletes => {
                self.s.sys = SysPhase::Defunct;
                if let Some(sh) = self.s.shutdown.as_mut() {
                    sh.step = ShutStep::Swept;
                }
            }
        }
    }

    fn spend_manual(&mut self, _c: Key) {
        self.s.left.manual -= 1;
    }

    /// The incarnation leaves its loop with `r` to run `on_stopped`; a
    /// supervisor first delivers the reports its final drain collects.
    fn leave_loop(&mut self, t: Tok, r: Reason) {
        if self.s.has_sup(SupId::Inc(t)) {
            self.final_drain(SupId::Inc(t), r);
        } else {
            let i = self.s.incs.get_mut(&t).expect("model invariant: inc");
            i.stop_reason = Some(r);
            i.status = Status::Stopping;
            i.stage = Stage::Hook;
        }
    }

    /// Register a worker of supervisor incarnation `p` from its `on_started`:
    /// a refused registration fails the initialization with the cause.
    fn spawn_worker(&mut self, p: Tok, w: Key) -> Result<(), Cause> {
        let x = SupId::Inc(p);
        if self.s.sys != SysPhase::Active {
            return Err(Cause::Shutdown);
        }
        let id = IdKey::W(w);
        if self.s.held.contains_key(&id) || self.s.foreign.contains(&id) {
            return Err(if self.cfg.kid_of(x, w).named {
                Cause::NameTaken
            } else {
                Cause::Duplicate
            });
        }
        let k = self.s.next_tok;
        self.s.next_tok += 1;
        self.s.incs.insert(k, new_inc(x, w, Some(p), id));
        self.s.held.insert(id, k);
        self.s
            .cells
            .insert(k, new_cell(x, w, Origin::Initial, Exec::Runner));
        self.s.sup_mut(x).order.push(w);
        self.set_life(x, w, Some(Life::Running(k)));
        self.raise_below(p, w);
        self.s.oracle.set_last(x, w, Some(k));
        Ok(())
    }

    /// A child `w` of supervisor incarnation `p` was created: the subtree
    /// budget of `p` rises to cover it, and never falls.
    fn raise_below(&mut self, p: Tok, w: Key) {
        let kid = self.cfg.kid_of(SupId::Inc(p), w);
        let cover = sat(kid.own_fin(), 0);
        self.obs.push(Obs::Budget {
            is_supervisor: kid.is_supervisor,
            policy: kid.policy,
            window_ns: kid.window_ns,
            kill_grace_ns: kid.kg(),
            own_fin: kid.own_fin(),
            own_budget: kid.own_budget(),
        });
        let i = self.s.incs.get_mut(&p).expect("model invariant: inc");
        i.below = i.below.max(cover);
    }

    fn observe(&mut self, t: Tok) {
        let lane = self.s.inc(t).lane;
        self.s
            .incs
            .get_mut(&t)
            .expect("model invariant: inc")
            .unseen = false;
        match lane {
            Some(Reason::Kill) => {
                self.s
                    .incs
                    .get_mut(&t)
                    .expect("model invariant: inc")
                    .stop_reason = Some(Reason::Kill);
                self.close(t);
            }
            Some(r) if strength(r) == Strength::Requested => {
                self.s.incs.get_mut(&t).expect("model invariant: inc").stage = Stage::Gate(r);
            }
            Some(r) => self.leave_loop(t, r),
            None => panic!("model invariant: an unseen lane holds a reason"),
        }
    }

    fn gate_returns(&mut self, t: Tok) {
        let Stage::Gate(r) = self.s.inc(t).stage else {
            panic!("model invariant: gate stage");
        };
        let behavior = self.cfg_of(t).behavior;
        let accepted = match behavior {
            Behavior::Cooperative => true,
            Behavior::Vetoing => false,
            Behavior::VetoOnce => {
                let was = self.s.inc(t).vetoed;
                self.s
                    .incs
                    .get_mut(&t)
                    .expect("model invariant: inc")
                    .vetoed = true;
                was
            }
            Behavior::ParentOnly => r == Reason::ParentRequest,
        };
        if accepted
            && self.cfg_of(t).policy == Policy::Infinity
            && self.s.inc(t).reasked
            && (!self.cfg.mark_override_asks || self.s.inc(t).ask_by_override)
        {
            self.hit(Wit::AcceptsAfterTheOneMoreAsk);
        }
        let lane = self.s.inc(t).lane;
        if lane == Some(Reason::Kill) {
            self.s
                .incs
                .get_mut(&t)
                .expect("model invariant: inc")
                .stop_reason = Some(Reason::Kill);
            self.close(t);
            return;
        }
        match lane {
            Some(f) if strength(f) == Strength::Forced => self.leave_loop(t, f),
            _ if accepted => self.leave_loop(t, r),
            _ => {
                self.s.incs.get_mut(&t).expect("model invariant: inc").stage = Stage::Idle;
            }
        }
    }

    /// A restart attempt registers its child and starts its incarnation.
    fn attempt_spawns(&mut self, m: Tok) {
        self.obs.push(Obs::Attempt {
            stage: AtStage::Factory,
            event: AtEvent::Begin { ordered: false },
            ladder: Ladder::Off,
            kill_stored: false,
            acked: false,
            want: AtWant::Spawn,
        });
        let x = self.s.cell(m).sup;
        let c = self.s.cell(m).key;
        let id = match x {
            SupId::S => IdKey::S(c),
            SupId::Inc(_) => IdKey::W(c),
        };
        let parent = match x {
            SupId::S => None,
            SupId::Inc(p) => Some(p),
        };
        let held = self.s.held.contains_key(&id) || self.s.foreign.contains(&id);
        // Harness: why a refused registration is refused, from the state the
        // step started in.
        let refusal = if self.pre.sys != SysPhase::Active {
            Some(Cause::Shutdown)
        } else if self.pre.held.contains_key(&id) || self.pre.foreign.contains(&id) {
            Some(if self.cfg.kid_of(x, c).named {
                Cause::NameTaken
            } else {
                Cause::Duplicate
            })
        } else {
            None
        };
        self.s
            .cells
            .get_mut(&m)
            .expect("model invariant: cell")
            .refusal = refusal;
        // Harness: an attempt that spawns for a member the layout recorded.
        if x == SupId::S
            && self
                .s
                .sup
                .group
                .as_ref()
                .is_some_and(|g| g.members.values().any(|o| *o == Some(m)))
        {
            self.fail(NO_GROUP_COMPLETES_ON_AN_INCARNATION_ITS_CHAIN_DID_NOT_START);
        }
        let refused = if self.s.sys != SysPhase::Active {
            Some(Cause::Shutdown)
        } else if held {
            Some(if self.cfg.kid_of(x, c).named {
                Cause::NameTaken
            } else {
                Cause::Duplicate
            })
        } else {
            None
        };
        if let Some(cause) = refused {
            {
                let cell = self.s.cells.get_mut(&m).expect("model invariant: cell");
                cell.closed = true;
                cell.exec = Exec::Returned;
            }
            self.put_outcome(x, m, c, Outcome::Failed(Reason::Failure(cause)));
            return;
        }
        self.s.incs.insert(m, new_inc(x, c, parent, id));
        self.s.held.insert(id, m);
        self.s.sup_mut(x).pred.insert(c, m);
        self.s.oracle.set_last(x, c, Some(m));
        if let SupId::Inc(p) = x {
            self.raise_below(p, c);
        }
        self.s
            .cells
            .get_mut(&m)
            .expect("model invariant: cell")
            .exec = Exec::Attempt(AttemptStage::Init);
    }

    fn runner_fails(&mut self, t: Tok) {
        self.s.left.runner_fail -= 1;
        let key = self.s.cell(t).key;
        let exec = self.s.cell(t).exec;
        {
            let c = self.s.cells.get_mut(&t).expect("model invariant: cell");
            c.ladder = Ladder::Off;
            c.closed = true;
            c.exec = Exec::RunnerFailed;
        }
        if self.s.live(t) {
            self.hit(Wit::RunnerFailureAfterSpawn);
            self.fire_guard(t, Exec::RunnerFailed);
        } else {
            debug_assert!(matches!(
                exec,
                Exec::Attempt(AttemptStage::Wait | AttemptStage::Factory)
            ));
            self.hit(Wit::RunnerFailureBeforeSpawn);
            let x = self.s.cell(t).sup;
            self.put_outcome(x, t, key, Outcome::Failed(Reason::Failure(Cause::Panic)));
        }
    }

    fn s_verdict(&mut self) {
        let exit = self
            .s
            .verdict_exit()
            .expect("model invariant: verdict exit enabled");
        let chain = matches!(
            self.s.sup.group.as_ref().map(|g| &g.phase),
            Some(Phase::Chain { .. })
        );
        if !chain {
            self.advance_stop_queue();
            let blocked = match self.s.sup.group.as_ref().map(|g| &g.phase) {
                Some(Phase::Stop {
                    awaiting, queue, ..
                }) => {
                    queue.is_empty()
                        && !awaiting.is_empty()
                        && awaiting.values().all(|t| self.s.cell(*t).verdict)
                }
                _ => false,
            };
            // The barrier the loop's check reads, only while the stop queue
            // is empty: production stops every committed member at once.
            if let Some(Phase::Stop {
                awaiting, queue, ..
            }) = self.s.sup.group.as_ref().map(|g| &g.phase)
            {
                if queue.is_empty() {
                    let entries: Vec<(Key, bool)> = self
                        .s
                        .sup
                        .order
                        .iter()
                        .rev()
                        .filter_map(|k| awaiting.get(k).map(|t| (*k, self.s.cell(*t).verdict)))
                        .collect();
                    let exit_key = blocked
                        .then(|| entries.iter().find(|(_, v)| *v).map(|(k, _)| *k))
                        .flatten();
                    self.obs.push(Obs::Verdict {
                        entries,
                        exit: exit_key,
                    });
                }
            }
            if !blocked {
                return;
            }
        } else if let Some(Phase::Chain { queue, .. }) = self.s.sup.group.as_ref().map(|g| &g.phase)
        {
            if let Some(front) = queue.front() {
                self.obs.push(Obs::Verdict {
                    entries: vec![(*front, true)],
                    exit: Some(*front),
                });
            }
        }
        self.s.oracle.s_cause = Some(SCause::Unresponsive);
        self.final_drain(SupId::S, Reason::Failure(Cause::Unresponsive(exit)));
    }

    fn s_observe(&mut self) {
        self.s.s_unseen = false;
        match self.s.s_lane {
            Some(Reason::Kill) => self.drop_supervision(SupId::S),
            Some(Reason::ParentRequest) => {
                self.s.oracle.s_cause = Some(if self.s.shutdown.is_some() {
                    SCause::Shutdown
                } else {
                    SCause::ParentStop
                });
                self.final_drain(SupId::S, Reason::ParentRequest);
            }
            other => {
                panic!("model invariant: S lane holds a parent request or a kill, got {other:?}")
            }
        }
    }

    fn final_deliver(&mut self, x: SupId, fails: bool) {
        let (c, t, r) = self.s.sup_mut(x).final_q.remove(0);
        if fails {
            self.s.left.final_fail -= 1;
            self.s.oracle.mark_failed_final(x);
            self.s.sup_mut(x).final_err = true;
            if !self.s.sup_ref(x).final_q.is_empty() {
                self.hit(Wit::FailedFinalDeliveryRestStillDelivered);
            }
        }
        self.hit(Wit::FinalDelivery);
        self.deliver(x, c, t, r, Action::SupervisorStopping, Delivery::Final);
    }

    fn s_hook_done(&mut self) {
        let SPhase::Hook(mut r) = self.s.s_phase else {
            panic!("model invariant: hook phase");
        };
        if self.s.sup.final_err {
            r = Reason::Failure(Cause::Crash);
        }
        if self.s.s_lane == Some(Reason::Kill) {
            self.drop_supervision(SupId::S);
        } else {
            self.begin_teardown(SupId::S, r);
        }
    }

    fn caller_sees(&mut self) {
        let caller = self.s.caller.expect("model invariant: caller");
        let t = caller.t;
        if self.s.ended(t) {
            if self.s.cell(t).exec == Exec::RunnerFailed {
                self.hit(Wit::CallerWaitedForRunnerFailedEnd);
            }
            let r = self.s.seen_reason(t);
            let mut terminate = false;
            match self.s.life(caller.c) {
                Some(Life::Stopping { kind, .. }) => {
                    terminate = kind == Kind::Terminate;
                    self.classify_manual(caller.c, t, kind, r);
                }
                Some(Life::Running(n)) if caller.path == Path::Override && n == t => {
                    terminate = true;
                    self.classify_manual(caller.c, t, Kind::Terminate, r);
                }
                _ => {}
            }
            if terminate && !self.s.gone(t) {
                // The terminate returns only once the subtree has ended.
                self.s.caller = Some(Caller {
                    kind: CKind::TermSubtree,
                    path: Path::Commit,
                    ..caller
                });
                return;
            }
            self.answer = Some(Answer::Ok);
        } else {
            self.answer = Some(Answer::ChildUnresponsive);
        }
        self.s.caller = None;
        self.s.oracle.pending_call = None;
    }

    /// The subtree a handler waited for ended.
    fn subtree_gone(&mut self) {
        let caller = self.s.caller.expect("model invariant: caller");
        self.s.caller = None;
        self.s.oracle.pending_call = None;
        match caller.kind {
            CKind::SpawnWait => self.spawn_proceed(caller.c),
            _ => self.answer = Some(Answer::Ok),
        }
    }

    /// A bounded subtree wait of a handler expired: the verdict, and the
    /// supervisor keeps running.
    fn subtree_wait_expires(&mut self) {
        let caller = self.s.caller.expect("model invariant: caller");
        self.s.caller = None;
        self.s.oracle.pending_call = None;
        self.answer = Some(match caller.kind {
            CKind::SpawnWait => Answer::SpawnUnresponsive,
            _ => Answer::ChildUnresponsive,
        });
    }
}

// ---------------------------------------------------------------------------
// Check names and witnesses
// ---------------------------------------------------------------------------

const NO_OBLIGATION_DEPENDS_ON_THE_CALLER: &str = "no_obligation_depends_on_the_caller";
const NO_HANDLER_WAITS_ON_THE_LOOP: &str = "no_handler_waits_on_the_loop";
const FIRST_SIGNAL_WAITS_AT_MOST_FOR_AN_ADOPTION_IN_TRANSIT: &str =
    "first_signal_waits_at_most_for_an_adoption_in_transit";
const EVERY_WAIT_IS_BOUNDED: &str = "every_wait_is_bounded";
const ORDERS_FOLLOW_THE_CONFIGURATION: &str = "orders_follow_the_configuration";
const BOUNDED_SEARCH_DEPTH_CAP: &str = "bounded_search_depth_cap";
const REPORTS_CHANGE_ONLY_THEIR_OCCUPANT: &str = "reports_change_only_their_occupant";
const EVERY_TRANSITION_IS_IN_THE_TABLE: &str = "every_transition_is_in_the_table";
const AT_MOST_ONE_LIVE_INCARNATION_PER_CHILD: &str = "at_most_one_live_incarnation_per_child";
const AT_MOST_ONE_LIVE_REGISTRATION_PER_ID: &str = "at_most_one_live_registration_per_id";
const EVERY_LIVE_INCARNATION_IS_THE_LEDGER_OCCUPANT: &str =
    "every_live_incarnation_is_the_ledger_occupant";
const DOWN_ONLY_AFTER_THE_TASK_ENDED: &str = "down_only_after_the_task_ended";
const NO_GROUP_COMPLETES_ON_AN_INCARNATION_ITS_CHAIN_DID_NOT_START: &str =
    "no_group_completes_on_an_incarnation_its_chain_did_not_start";
const STOP_REQUESTS_NEVER_WEAKEN: &str = "stop_requests_never_weaken";
const KILL_IS_FINAL: &str = "kill_is_final";
const ONE_VERDICT_AND_NO_SILENT_WEDGE: &str = "one_verdict_and_no_silent_wedge";
const STOPPED_ONLY_WHEN_EVERY_CHILD_IS_SETTLED: &str = "stopped_only_when_every_child_is_settled";
const NO_RESTART_STRATEGY_OR_DELIVERY_DURING_TEARDOWN: &str =
    "no_restart_strategy_or_delivery_during_teardown";
const TEARDOWN_ORDER: &str = "teardown_order";
const GROUP_BARRIER_HOLDS: &str = "group_barrier_holds";
const CHAIN_ORDER_AND_RETRY_SCOPE: &str = "chain_order_and_retry_scope";
const BUDGET_MATCHES_THE_SERIAL_COUNT: &str = "budget_matches_the_serial_count";
const MANUAL_RESULTS_MATCH_THE_TABLE: &str = "manual_results_match_the_table";
const NO_INCARNATION_DELIVERED_TWICE: &str = "no_incarnation_delivered_twice";
const EVERY_OWED_REPORT_IS_DELIVERED_ONCE: &str = "every_owed_report_is_delivered_once";
const REPORTS_CARRY_THE_REAL_REASON: &str = "reports_carry_the_real_reason";
const STATUS_NEVER_STOPPED_WHILE_LIVE: &str = "status_never_stopped_while_live";
const CHILDREN_VIEW_IS_TRUTHFUL: &str = "children_view_is_truthful";
const NO_SUCCESSOR_STARTS_BESIDE_ITS_PREDECESSOR_SUBTREE: &str =
    "no_successor_starts_beside_its_predecessor_subtree";
const NO_NAME_TAKEN_CHARGE_FROM_A_PREDECESSOR_SUBTREE: &str =
    "no_name_taken_charge_from_a_predecessor_subtree";
const KILLED_SUPERVISOR_CHILDREN_SETTLE_IN_ORDER_WITHIN_THEIR_WINDOWS: &str =
    "killed_supervisor_children_settle_in_order_within_their_windows";
const WORKERS_UNDER_A_KILLED_SUPERVISOR_SETTLE_WITHIN_THEIR_KILL_GRACE: &str =
    "workers_under_a_killed_supervisor_settle_within_their_kill_grace";
const PAIRING_RULE: &str = "pairing_rule";
const ABANDONED_SHUTDOWN_REFUSES_REGISTRATIONS_UNTIL_A_LATER_ONE_COMPLETES: &str =
    "abandoned_shutdown_refuses_registrations_until_a_later_one_completes";
const A_RAISED_KILL_SURVIVES_AN_ABANDONED_SHUTDOWN: &str =
    "a_raised_kill_survives_an_abandoned_shutdown";
const LEFTOVER_SWEEP_NEVER_CUTS_A_STILL_STOPPING_SUBTREE_BEFORE_THE_DEADLINE: &str =
    "leftover_sweep_never_cuts_a_still_stopping_subtree_before_the_deadline";
const NO_LEFTOVER_OUTLIVES_THE_DEADLINE_ONCE_THE_SWEEP_STARTED: &str =
    "no_leftover_outlives_the_deadline_once_the_sweep_started";
const AT_MOST_ONE_HOLDER_PER_NAME: &str = "at_most_one_holder_per_name";
const THE_SERIAL_OUTCOME_HOLDS: &str = "the_serial_outcome_holds";

/// What a step witnessed, counted per exploration so that no check passes
/// vacuously.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
enum Wit {
    AttemptOrderedFactory,
    AttemptOrderedInit,
    AttemptOrderedInTransit,
    MemberStoppingAtEvaluation,
    ReportForRetiredTokenPending,
    QueuedDeathAtRetry,
    SpawnedDuringGroupJoinsRetry,
    KillDuringHook,
    KillDuringPreStart,
    KillDuringTeardown,
    KillDuringSupervisorHook,
    ExemptThroughHandlerOnEndedAttempt,
    ThirdGracefulOverParentRequest,
    StuckMemberInBarrier,
    InitOutlivesStartTimeoutAndLadder,
    RunnerFailureBeforeSpawn,
    RunnerFailureAfterSpawn,
    CallerAbandonsMidLadder,
    TerminateCommittedBeforeEvaluation,
    ManualRestartInitFails,
    ManualCallOnReleased,
    ManualCallOnEndedUndrained,
    TerminateOfUnsignalledMember,
    ManualAttemptFailsAsMember,
    ReportQueuedCollectedByFinalDrain,
    FinalDelivery,
    KillStopsFinalDrain,
    StuckMemberRunnerFailedVerdict,
    OwedEvaluationAbsorbedIntoSnapshot,
    StartAckedThenDied,
    ManualCallOnRunnerFailedEnded,
    CallerWaitedForRunnerFailedEnd,
    TeardownPassedRunnerFailedEnd,
    OverrideOneMoreAsk,
    AcceptsAfterTheOneMoreAsk,
    FailedFinalDeliveryRestStillDelivered,
    UnspawnedAttemptOrderedAtEvaluation,
    WaitingAttemptOrderedAtEvaluation,
    UnspawnedAttemptOrderedAtRetry,
    WaitingAttemptOrderedAtRetry,
    AttemptCommittedAtRetry,
    TerminateDuringChainKeptDownThroughRetry,
    TeardownAskAfterGroupStopping,
    TeardownAskAfterGroupRunning,
    StopChildAbsorbed,
    StopChildRefused,
    ManualJoinOneMoreAsk,
    ManualJoinAnsweredUnresponsive,
    OverrideAnsweredUnresponsive,
    StopChildAbsorbedInRetry,
    StopChildKeptDownSpawnedSince,
    SpawnedSinceOverrideLeftOutOfTheRetry,
    ReportCollectedByEscalationDrain,
    StopOrRestartRefusedEarlierChain,
    TeardownAskToOrderedAttempt,
    SecondOverrideAsk,
    StopChildRefusedOverriddenAwaited,
    KeepRecordsKeyOutsideRetryPlan,
    KeepJoinSharedAsk,
    AsTerminateTransient,
    AsTerminateTemporary,
    ReportCollectedByFinalDrain,
    AttemptOrderedWait,
    BusyNamedWorkerAtSupervisorKill,
    SuccessorWaitingOnLiveSubtree,
    SuccessorWaitExpired,
    OrderBesideAnExpirableWait,
    SupervisorChildUnderInheritedStop,
    ZeroGraceOrderlyTeardownCompletes,
    FailedAttemptSubtreeWaitedFor,
    SupervisorIncarnationExhaustedItsBudget,
    SpawnWaitAfterAbandonedTerminate,
    SpawnWaitAfterPruning,
    SpawnWaitAfterPoolBounce,
    ChildCrashAfterAbandonedShutdown,
    RestartRefusedWhileShuttingDown,
    TerminateOfDownChildWaitsOnSubtree,
    RestartRefusedAtSupervisorLevel,
    ShutdownKillReachesSupervisorChild,
    LostRunnerVerdictAfterGrandparentKill,
    ParentStopWhileHandlerWaits,
    TeardownOrdersSupervisorChildUnderKill,
    GuardKeepsBareKillAbortUnderBoundedPolicy,
    GuardKeepsBareKillAbortUnderInfinity,
    GuardKeepsLadderKillAbortUnderBoundedPolicy,
    GuardKeepsLadderKillAbortUnderInfinity,
    DeadlineEntryFiresAfterAbandon,
}

impl Wit {
    fn label(self) -> &'static str {
        match self {
            Wit::AttemptOrderedFactory => "an in-flight attempt ordered before its factory ran",
            Wit::AttemptOrderedWait => "an in-flight attempt ordered during its successor wait",
            Wit::BusyNamedWorkerAtSupervisorKill => "a busy named worker at its supervisor's Kill",
            Wit::SuccessorWaitingOnLiveSubtree => "a successor waiting on a live subtree",
            Wit::SuccessorWaitExpired => "an expired successor wait",
            Wit::OrderBesideAnExpirableWait => "an order beside an expirable wait",
            Wit::SupervisorChildUnderInheritedStop => "a supervisor child under an inherited orderly stop",
            Wit::ZeroGraceOrderlyTeardownCompletes => "the same with a zero kill grace completing its orderly teardown",
            Wit::FailedAttemptSubtreeWaitedFor => "a failed attempt's subtree waited for",
            Wit::SupervisorIncarnationExhaustedItsBudget => {
                "a supervisor incarnation stopping itself for an exhausted restart budget"
            }
            Wit::SpawnWaitAfterAbandonedTerminate => "a same-id spawn waiting on a retired subtree after an abandoned terminate",
            Wit::SpawnWaitAfterPruning => "a same-id spawn waiting on a retired subtree after a pruning",
            Wit::SpawnWaitAfterPoolBounce => "a same-id spawn waiting on a retired subtree after a pool bounce",
            Wit::ChildCrashAfterAbandonedShutdown => "an abandoned shutdown followed by a child crash",
            Wit::RestartRefusedWhileShuttingDown => "a supervised restart refused while an abandoned shutdown leaves the system shutting down",
            Wit::RestartRefusedAtSupervisorLevel => "a worker restart refused at its supervisor incarnation's level while the system shuts down",
            Wit::ShutdownKillReachesSupervisorChild => "the shutdown's Kill step reaching a supervisor child",
            Wit::LostRunnerVerdictAfterGrandparentKill => "a stuck worker whose runner failed given its verdict after its supervisor's supervisor was killed",
            Wit::ParentStopWhileHandlerWaits => "a parent stop or Kill while a handler call waits",
            Wit::TeardownOrdersSupervisorChildUnderKill => "S's orderly teardown ordering a supervisor child under Kill",
            Wit::GuardKeepsBareKillAbortUnderBoundedPolicy => {
                "a guard keeping a stored Kill's abort: a bare Kill on a supervisor child under Timeout or Kill"
            }
            Wit::GuardKeepsBareKillAbortUnderInfinity => {
                "a guard keeping a stored Kill's abort: a bare Kill on a supervisor child under Infinity"
            }
            Wit::GuardKeepsLadderKillAbortUnderBoundedPolicy => {
                "a guard keeping a stored Kill's abort: a ladder or attempt Kill on a supervisor child under Timeout or Kill"
            }
            Wit::GuardKeepsLadderKillAbortUnderInfinity => {
                "a guard keeping a stored Kill's abort: a ladder or attempt Kill on a supervisor child under Infinity"
            }
            Wit::DeadlineEntryFiresAfterAbandon => {
                "a deadline entry of the final sweep firing after its shutdown was abandoned"
            }
            Wit::TerminateOfDownChildWaitsOnSubtree => "a terminate of a child already down waiting on its old subtree",
            Wit::AttemptOrderedInit => "an in-flight attempt ordered during its init",
            Wit::AttemptOrderedInTransit => "an order written while an ack is in transit",
            Wit::MemberStoppingAtEvaluation => "a member already stopping at group evaluation",
            Wit::ReportForRetiredTokenPending => "a report for a retired token pending",
            Wit::QueuedDeathAtRetry => "a queued member death at a retry",
            Wit::SpawnedDuringGroupJoinsRetry => "a child spawned during a pending group joining a retry",
            Wit::KillDuringHook => "a Kill raised during the hook",
            Wit::KillDuringPreStart => "a Kill raised during pre_start",
            Wit::KillDuringTeardown => "a Kill raised during the teardown",
            Wit::ExemptThroughHandlerOnEndedAttempt => {
                "a wait on an ended attempt exempted through the handler wait it is delayed behind"
            }
            Wit::KillDuringSupervisorHook => "a Kill raised during the supervisor's hook",
            Wit::ThirdGracefulOverParentRequest => "a third-party Graceful over a pending ParentRequest",
            Wit::StuckMemberInBarrier => "a stuck member in a barrier",
            Wit::InitOutlivesStartTimeoutAndLadder => "a stuck start attempt given the verdict after its ladder",
            Wit::RunnerFailureBeforeSpawn => "a runner failure before the spawn",
            Wit::RunnerFailureAfterSpawn => "a runner failure after the spawn",
            Wit::CallerAbandonsMidLadder => "a caller abandoning mid-ladder",
            Wit::TerminateCommittedBeforeEvaluation => "a terminate committed before a group evaluation",
            Wit::ManualRestartInitFails => "a manual restart whose init fails",
            Wit::ManualCallOnReleased => "a manual call on a released member",
            Wit::ManualCallOnEndedUndrained => {
                "a manual call on a running child whose task ended before its death was drained"
            }
            Wit::TerminateOfUnsignalledMember => "a terminate of a member not yet signalled",
            Wit::ManualAttemptFailsAsMember => "a manual restart's attempt failing as a committed member",
            Wit::ReportQueuedCollectedByFinalDrain => {
                "a report queued behind a group when the loop ends, collected by the final drain"
            }
            Wit::FinalDelivery => "a final delivery",
            Wit::KillStopsFinalDrain => "a Kill that stops the final drain with reports left",
            Wit::StuckMemberRunnerFailedVerdict => "a stuck member whose runner failed given its verdict",
            Wit::OwedEvaluationAbsorbedIntoSnapshot => "an owed evaluation absorbed into a snapshot at a layout",
            Wit::StartAckedThenDied => "a start that acked and then died, adopted before its death",
            Wit::ManualCallOnRunnerFailedEnded => {
                "a manual call on a child whose runner failed and whose task ended"
            }
            Wit::CallerWaitedForRunnerFailedEnd => "a caller that waited for the end of an incarnation whose runner failed",
            Wit::TeardownPassedRunnerFailedEnd => "a teardown that waited for the end of an incarnation whose runner failed",
            Wit::OverrideOneMoreAsk => "a terminate override giving the one more ask under Infinity",
            Wit::AcceptsAfterTheOneMoreAsk => "a member under Infinity accepting after the one more ask",
            Wit::FailedFinalDeliveryRestStillDelivered => "a failed final delivery, the remaining reports still delivered",
            Wit::UnspawnedAttemptOrderedAtEvaluation => "an unspawned attempt ordered at a first group evaluation",
            Wit::WaitingAttemptOrderedAtEvaluation => "an attempt waiting for its predecessor's subtree ordered at a first group evaluation",
            Wit::UnspawnedAttemptOrderedAtRetry => "an unspawned attempt ordered at a group retry",
            Wit::WaitingAttemptOrderedAtRetry => "an attempt waiting for its predecessor's subtree ordered at a group retry",
            Wit::AttemptCommittedAtRetry => "an attempt in flight committed at a group retry",
            Wit::TerminateDuringChainKeptDownThroughRetry => "a terminate committed during the chain kept down through the retry",
            Wit::TeardownAskAfterGroupStopping => "a teardown's own one more ask after a group's (stopping member)",
            Wit::TeardownAskAfterGroupRunning => "a teardown's own one more ask after a group's (ordered member)",
            Wit::StopChildAbsorbed => "a stop_child absorbed by a pending group restart",
            Wit::StopChildRefused => "a stop_child refused during a pending group restart",
            Wit::ManualJoinOneMoreAsk => "a manual join giving the one more ask under Infinity",
            Wit::ManualJoinAnsweredUnresponsive => "a manual join answered Unresponsive at once",
            Wit::OverrideAnsweredUnresponsive => "an override terminate answered Unresponsive at once",
            Wit::StopChildAbsorbedInRetry => "a stop_child absorbed in a retry's stop phase",
            Wit::StopChildKeptDownSpawnedSince => {
                "a stop_child of a Transient or Temporary child started during a pending group, kept down like its terminate"
            }
            Wit::SpawnedSinceOverrideLeftOutOfTheRetry => {
                "a retry laid out while a child started since the group restart began is kept down by an override"
            }
            Wit::ReportCollectedByEscalationDrain => "a report collected by an escalation's final drain",
            Wit::StopOrRestartRefusedEarlierChain => {
                "a stop_child or restart_child refused for a key an earlier chain of the group restarted"
            }
            Wit::TeardownAskToOrderedAttempt => "a teardown's one more ask to an ordered restart attempt that finished its init",
            Wit::SecondOverrideAsk => "a second terminate override's one more ask to an earlier override's member",
            Wit::StopChildRefusedOverriddenAwaited => "a stop_child refused for an awaited member a terminate released",
            Wit::KeepRecordsKeyOutsideRetryPlan => "a terminate during a RestForOne retry keeping down a member the retry does not include",
            Wit::KeepJoinSharedAsk => "a terminate join that keeps a child down, giving the shared one more ask",
            Wit::AsTerminateTransient => "a stop_child of a Transient member of a pending group handled as its terminate_child",
            Wit::AsTerminateTemporary => "a stop_child of a Temporary member of a pending group handled as its terminate_child",
            Wit::ReportCollectedByFinalDrain => "a report collected by the final drain",
        }
    }
}

// ---------------------------------------------------------------------------
// Oracle functions: expectations computed from facts, never from the logic
// ---------------------------------------------------------------------------

impl State {
    /// The reason a report about `t` must carry.
    fn expected_reason(&self, _cfg: &Cfg, t: Tok) -> Option<Reason> {
        let cell = self.cells.get(&t)?;
        if let Some(inc) = self.incs.get(&t) {
            let end = inc.end?;
            if cell.exec == Exec::RunnerFailed {
                return Some(end);
            }
            if cell.st_named {
                return Some(Reason::Failure(Cause::StartTimeout));
            }
            return Some(end);
        }
        match cell.exec {
            Exec::Returned if cell.closed => cell.refusal.map(Reason::Failure),
            Exec::RunnerFailed => Some(Reason::Failure(Cause::Panic)),
            _ => None,
        }
    }

    /// The group membership of a key, recomputed from the layout record, the
    /// overrides and the log, never from the barrier or the restart set.
    fn o_membership(&self, cfg: &Cfg, c: Key) -> Memb {
        let Some(g) = &self.sup.group else {
            return Memb::Outside;
        };
        if self.life(c).is_none() || self.oracle.removed.contains(&c) {
            return Memb::Outside;
        }
        let stop = matches!(g.phase, Phase::Stop { .. });
        if stop {
            if let (Some(Some(t)), Some(o)) = (g.members.get(&c), self.occ(c)) {
                if *t == o {
                    return Memb::Awaited;
                }
            }
        }
        let temporary = cfg.kid(c).restart == Restart::Temporary;
        let member = g.members.contains_key(&c);
        if member
            && !temporary
            && !g.overrides.contains(&c)
            && (stop || !self.oracle.chain_log.contains(&c))
        {
            return Memb::Queued;
        }
        if g.overrides.contains(&c) {
            return Memb::Released;
        }
        Memb::Outside
    }

    /// The pending group will restart `c`.
    fn o_membership_of(&self, cfg: &Cfg, x: SupId, c: Key) -> Memb {
        if x == SupId::S {
            self.o_membership(cfg, c)
        } else {
            Memb::Outside
        }
    }

    fn o_restarts_of(&self, cfg: &Cfg, x: SupId, c: Key) -> bool {
        x == SupId::S && self.o_restarts(cfg, c)
    }

    fn o_restarts(&self, cfg: &Cfg, c: Key) -> bool {
        let Some(g) = &self.sup.group else {
            return false;
        };
        matches!(self.o_membership(cfg, c), Memb::Awaited | Memb::Queued)
            && cfg.kid(c).restart != Restart::Temporary
            && !g.overrides.contains(&c)
    }

    /// The action a manual completion's snapshot must carry.
    fn o_decide(&self, cfg: &Cfg, c: Key, kind: Kind) -> Action {
        if self.o_restarts(cfg, c) {
            return Action::RestartInitiated;
        }
        match kind {
            Kind::Terminate => Action::Removed,
            Kind::Bounce => {
                if cfg.is_pool() || cfg.kid(c).restart != Restart::Permanent {
                    Action::Removed
                } else {
                    Action::RestartInitiated
                }
            }
        }
    }

    /// The members a group of `kind` covers when it starts at `c0`, from the
    /// ledger and the start order as they stand.
    fn o_members(&self, kind: &GKind, c0: Key) -> BTreeMap<Key, Option<Tok>> {
        let mut out = BTreeMap::new();
        let mut seen_c0 = false;
        for k in &self.sup.order {
            if *k == c0 {
                seen_c0 = true;
            }
            let in_scope = match kind {
                GKind::All => true,
                GKind::Rest => seen_c0,
            };
            if in_scope {
                out.insert(*k, self.occ(*k));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Inline checks of the procedures
// ---------------------------------------------------------------------------

impl Ap<'_> {
    fn check_layout_members(
        &mut self,
        kind: &GKind,
        c0: Key,
        members: &BTreeMap<Key, Option<Tok>>,
    ) {
        if self.s.o_members(kind, c0) != *members {
            self.fail(CHAIN_ORDER_AND_RETRY_SCOPE);
        }
    }

    /// Checks made when a chain starts a member.
    fn check_chain_initiate(&mut self, f: Key) {
        let Some(g) = self.s.sup.group.as_ref() else {
            self.fail(CHAIN_ORDER_AND_RETRY_SCOPE);
            return;
        };
        let order = &self.s.sup.order;
        let pos = order.iter().position(|k| *k == f).unwrap_or(0);
        let expected: Vec<Key> = order[..pos]
            .iter()
            .copied()
            .filter(|k| g.members.contains_key(k))
            .filter(|k| self.cfg.kid(*k).restart != Restart::Temporary)
            .filter(|k| !g.overrides.contains(k))
            .filter(|k| !self.s.oracle.removed.contains(k))
            .collect();
        let in_log: Vec<Key> = self
            .s
            .oracle
            .chain_log
            .iter()
            .copied()
            .filter(|k| expected.contains(k))
            .collect();
        let recorded_live = g
            .members
            .values()
            .any(|o| o.is_some_and(|t| self.s.live(t)));
        let temporary = self.cfg.kid(f).restart == Restart::Temporary;
        if in_log != expected {
            self.fail(CHAIN_ORDER_AND_RETRY_SCOPE);
        }
        if recorded_live || temporary {
            self.fail(GROUP_BARRIER_HOLDS);
        }
    }

    /// Checks made at the step that completes a group.
    fn check_group_completion(&mut self) {
        let Some(g) = self.s.sup.group.as_ref() else {
            return;
        };
        let Phase::Chain { started, .. } = &g.phase else {
            self.fail(NO_GROUP_COMPLETES_ON_AN_INCARNATION_ITS_CHAIN_DID_NOT_START);
            return;
        };
        let mut bad = false;
        for c in &self.s.oracle.chain_log {
            match started.get(c) {
                Some(k) => {
                    if self.s.occ(*c) != Some(*k) && self.s.live(*k) {
                        bad = true;
                    }
                }
                None => bad = true,
            }
        }
        for c in g.members.keys() {
            if self.cfg.kid(*c).restart == Restart::Temporary
                || g.overrides.contains(c)
                || self.s.oracle.removed.contains(c)
            {
                continue;
            }
            if !self.s.oracle.chain_log.contains(c) {
                bad = true;
            }
        }
        if bad {
            self.fail(NO_GROUP_COMPLETES_ON_AN_INCARNATION_ITS_CHAIN_DID_NOT_START);
        }
    }

    /// The restart budget against the serial count, at a supervisor's exit.
    fn check_budget_at_exit(&mut self, x: SupId) {
        let expected = self.s.oracle.charges_of(x);
        let budget = self.cfg.budget_of(x);
        if self.s.sup_ref(x).charges != expected.min(budget)
            || self.s.oracle.exhausted(x) != (expected > budget)
        {
            self.fail(BUDGET_MATCHES_THE_SERIAL_COUNT);
        }
    }
}

// ---------------------------------------------------------------------------
// Waits, boundedness and progress
// ---------------------------------------------------------------------------

/// A wait a supervisor, or the guard of a dropped supervisor, makes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Wait {
    /// A stop order not yet settled.
    Occ(Tok),
    /// A restart's init, unordered and not acked.
    Init(Tok),
    /// An incarnation whose attempt task failed, with no order reaching it.
    Lost(Tok),
    /// The attempt `m` waiting for the subtree of the awaited `t` to end
    /// before its factory runs.
    SubAttempt(Tok, Tok),
    /// A handler waiting for the subtree of `t` to end.
    SubCaller(Tok),
}

impl Wait {
    /// Whether the wait is the open wait of the attempt `m`: an attempt in its
    /// successor wait is the waiter, not the predecessor it waits on.
    fn concerns(self, m: Tok) -> bool {
        match self {
            Wait::SubAttempt(_, a) => a == m,
            other => other.token() == m,
        }
    }

    /// The incarnation waited on.
    fn token(self) -> Tok {
        match self {
            Wait::Occ(t)
            | Wait::Init(t)
            | Wait::Lost(t)
            | Wait::SubAttempt(t, _)
            | Wait::SubCaller(t) => t,
        }
    }
}

impl State {
    /// Every wait of the supervisor in this state. A wait ends with its
    /// waiter: once the supervision state is gone, only the obligation of its
    /// guards remains, on every token whose incarnation is still live.
    fn waits(&self) -> Vec<Wait> {
        let mut v = Vec::new();
        for (&t, cell) in &self.cells {
            let x = cell.sup;
            if !self.has_sup(x) {
                // The waiter is gone: only the guard's obligation on a live
                // token stays open.
                if self.live(t) && !self.settled(t) {
                    v.push(Wait::Occ(t));
                }
                continue;
            }
            if cell.order.is_some() && !self.settled(t) {
                v.push(Wait::Occ(t));
            }
            if cell.exec == Exec::Attempt(AttemptStage::Init)
                && cell.order.is_none()
                && self.live(t)
                && !self.inc(t).acked
                && !self.settled(t)
            {
                v.push(Wait::Init(t));
            }
            if cell.exec == Exec::RunnerFailed
                && self.live(t)
                && !cell.verdict
                && cell.order.is_none()
                && matches!(self.life_of(x, cell.key), Some(Life::Restarting { next, .. }) if next == t)
            {
                v.push(Wait::Lost(t));
            }
            if cell.exec == Exec::Attempt(AttemptStage::Wait) && cell.order.is_none() {
                if let Some(w) = cell.waits_on {
                    if !self.gone(w) {
                        v.push(Wait::SubAttempt(w, t));
                    }
                }
            }
        }
        if let Some(c) = self.caller {
            if c.kind != CKind::Stop && !self.gone(c.t) {
                v.push(Wait::SubCaller(c.t));
            }
        }
        v
    }

    fn wait_resolved(&self, w: Wait) -> bool {
        let owner_gone = |t: Tok| self.cells.get(&t).is_none_or(|c| !self.has_sup(c.sup));
        match w {
            Wait::Occ(t) => {
                if owner_gone(t) {
                    // The waiter is gone: only a guard's obligation on a live
                    // token stays open. every_live_incarnation_is_the_ledger_occupant
                    // is the backstop for a live token the drop leaves non-Orphan.
                    !self.live(t) || self.settled(t)
                } else {
                    self.settled(t)
                }
            }
            Wait::Init(m) => {
                owner_gone(m)
                    || self
                        .cells
                        .get(&m)
                        .is_none_or(|c| c.exec != Exec::Attempt(AttemptStage::Init))
                    || self.settled(m)
            }
            Wait::Lost(t) => owner_gone(t) || !self.live(t) || self.cell(t).verdict,
            Wait::SubAttempt(t, m) => {
                self.gone(t)
                    || owner_gone(m)
                    || self
                        .cells
                        .get(&m)
                        .is_none_or(|c| c.exec != Exec::Attempt(AttemptStage::Wait))
            }
            Wait::SubCaller(t) => {
                self.gone(t)
                    || !self
                        .caller
                        .is_some_and(|c| c.t == t && c.kind != CKind::Stop)
            }
        }
    }

    /// The policy of the stop that bounds `u`.
    fn governs(&self, cfg: &Cfg, u: Tok) -> Option<Policy> {
        let cell = self.cell(u);
        let kid = cfg.kid_of(cell.sup, cell.key);
        if matches!(cell.exec, Exec::Orphan | Exec::RunnerFailed) {
            // The inherited stop of its guard: a Kill for a worker, the
            // child's own policy for a supervisor child.
            return Some(if kid.is_supervisor {
                kid.policy
            } else {
                Policy::Kill
            });
        }
        if cell.order.is_some() {
            return Some(kid.policy);
        }
        if self.inc(u).lane == Some(Reason::Kill) {
            return Some(Policy::Kill);
        }
        None
    }

    /// The incarnations a wait reaches: the ones it waits on and, through the
    /// orderly teardown of a supervisor child under `Infinity`, the children
    /// that teardown orders or joins.
    fn reach(&self, cfg: &Cfg, w: Wait) -> Vec<Tok> {
        let mut set: Vec<Tok> = match w {
            Wait::Occ(t) | Wait::Init(t) | Wait::Lost(t) => vec![t],
            Wait::SubAttempt(t, _) | Wait::SubCaller(t) => self.subtree_live(t),
        };
        let mut i = 0;
        while i < set.len() {
            let v = set[i];
            i += 1;
            if self.has_sup(SupId::Inc(v)) && self.governs(cfg, v) == Some(Policy::Infinity) {
                for (&u, c) in &self.cells {
                    if c.sup == SupId::Inc(v)
                        && self.live(u)
                        && c.order.is_some_and(|o| o.from == OrderFrom::Teardown)
                        && !set.contains(&u)
                    {
                        set.push(u);
                    }
                }
            }
        }
        set
    }

    /// The wait of the handler a caller leaves open.
    fn caller_wait(c: Caller) -> Wait {
        match c.kind {
            CKind::Stop => Wait::Occ(c.t),
            CKind::TermSubtree | CKind::SpawnWait => Wait::SubCaller(c.t),
        }
    }

    /// `u` cannot reach its end by its own steps.
    fn blocked(&self, u: Tok) -> bool {
        let inc = self.inc(u);
        matches!(inc.stage, Stage::Stuck(_))
            || (inc.stage == Stage::Idle
                && !inc.unseen
                && matches!(inc.lane, Some(Reason::Graceful | Reason::ParentRequest)))
    }

    /// The exemption of a wait: it waits on an incarnation under `Infinity`
    /// that does not reach its fate by its own steps, or on an init no stop
    /// order reached and no start timeout bounds.
    fn exempt(&self, cfg: &Cfg, w: Wait) -> bool {
        // (d) a subtree wait whose carried bound an `Infinity` supervisor
        // child makes unbounded.
        if let Wait::SubAttempt(t, _) | Wait::SubCaller(t) = w {
            if self.inf_live(cfg, t) {
                return true;
            }
        }
        for u in self.reach(cfg, w) {
            if !self.live(u) {
                continue;
            }
            let cell = self.cell(u);
            let kid = cfg.kid_of(cell.sup, cell.key);
            let stage = self.inc(u).stage;
            // (a) blocked under `Infinity`.
            if self.blocked(u) && self.governs(cfg, u) == Some(Policy::Infinity) {
                let hung_init = matches!(stage, Stage::Stuck(At::PreStart | At::OnStarted));
                let attempt_bounded =
                    cell.exec == Exec::Attempt(AttemptStage::Init) && kid.start_timeout;
                if !(hung_init && attempt_bounded) {
                    return true;
                }
            }
            // (b) an unordered init, hung, with no start timeout.
            if matches!(w, Wait::Init(_))
                && u == w.token()
                && matches!(stage, Stage::Stuck(At::PreStart | At::OnStarted))
                && !kid.start_timeout
                && cell.order.is_none()
            {
                return true;
            }
        }
        self.exempt_through_handler(cfg, w)
    }

    /// (c) an order in transit while the supervisor's handler wait is exempt.
    /// The attempt's incarnation may already have ended: its order still waits
    /// for the adoption the handler delays.
    fn exempt_through_handler(&self, cfg: &Cfg, w: Wait) -> bool {
        let u = w.token();
        if !(matches!(w, Wait::Occ(_))
            && self.cell(u).exec == Exec::Attempt(AttemptStage::InTransit)
            && self.cell(u).sup == SupId::S)
        {
            return false;
        }
        match self.caller {
            Some(caller) => {
                let wc = State::caller_wait(caller);
                !self.wait_resolved(wc) && self.exempt(cfg, wc)
            }
            None => false,
        }
    }

    /// Whether `step` is one of the steps of incarnation `u` itself: its task,
    /// its ladder, its reaper entry and its attempt.
    fn step_owner(&self, step: Step) -> Option<Tok> {
        match step {
            Step::PreStartBegins(t)
            | Step::PreStartDone(t)
            | Step::OnStartedDone(t)
            | Step::Observe(t)
            | Step::GateReturns(t)
            | Step::EndTurn(t)
            | Step::HookDone(t)
            | Step::WindowEnds(t)
            | Step::GraceEnds(t)
            | Step::AbortGraceEnds(t)
            | Step::ReaperFires(t)
            | Step::ReaperKillStage(t)
            | Step::DeadlineKill(t)
            | Step::DeadlineAbort(t)
            | Step::LostRunnerVerdict(t)
            | Step::AttemptCancels(t)
            | Step::SuccessorProceeds(t)
            | Step::SuccessorWaitExpires(t)
            | Step::Spawn(t)
            | Step::StartTimeout(t)
            | Step::InitAck(t)
            | Step::PFinalDeliver(t)
            | Step::PFinalKill(t)
            | Step::PFinalDone(t)
            | Step::PTeardownStep(t)
            | Step::PTeardownKill(t) => Some(t),
            Step::DrainAck(m) | Step::DrainOutcome(m) => match self.cells.get(&m)?.sup {
                SupId::Inc(p) => Some(p),
                SupId::S => None,
            },
            Step::DrainDeath(_, t, _) => match self.cells.get(&t)?.sup {
                SupId::Inc(p) => Some(p),
                SupId::S => None,
            },
            _ => None,
        }
    }

    /// The incarnation and every incarnation below it, unspawned attempts
    /// included.
    fn tree_of(&self, t: Tok) -> BTreeSet<Tok> {
        let mut set = BTreeSet::from([t]);
        loop {
            let before = set.len();
            for (&v, c) in &self.cells {
                if let SupId::Inc(p) = c.sup {
                    if set.contains(&p) {
                        set.insert(v);
                    }
                }
            }
            if set.len() == before {
                return set;
            }
        }
    }

    /// Whether `step` belongs to the progress of the wait `w`.
    fn in_prog(&self, step: Step, w: Wait, without_loop: bool) -> bool {
        let mut toks = self.tree_of(w.token());
        if let Wait::SubAttempt(_, m) = w {
            toks.insert(m);
        }
        if let Wait::Occ(m) = w {
            // An attempt in transit is adopted by the loop of its
            // supervisor, which a supervisor incarnation runs itself.
            if let (Some(c), false) = (self.cells.get(&m), without_loop) {
                if let (Exec::Attempt(AttemptStage::InTransit), SupId::Inc(p)) = (c.exec, c.sup) {
                    toks.extend(self.tree_of(p));
                }
            }
        }
        if self.step_owner(step).is_some_and(|o| toks.contains(&o)) {
            return true;
        }
        if matches!((w, step), (Wait::SubCaller(_), Step::SubtreeWaitExpires)) {
            return true;
        }
        if without_loop {
            return false;
        }
        let in_transit_s = matches!(w, Wait::Occ(_))
            && self.cells.get(&w.token()).is_some_and(|c| {
                c.exec == Exec::Attempt(AttemptStage::InTransit) && c.sup == SupId::S
            });
        if !in_transit_s {
            return false;
        }
        let loop_step = matches!(
            step,
            Step::SObserve
                | Step::FinalDeliver
                | Step::FinalKill
                | Step::FinalDone
                | Step::SHookDone
                | Step::SAbort
                | Step::TeardownStep
                | Step::TeardownKill
                | Step::SVerdict
                | Step::DrainAck(_)
                | Step::DrainOutcome(_)
                | Step::DrainDeath(..)
        );
        if loop_step {
            return true;
        }
        if let Some(caller) = self.caller {
            return matches!(
                step,
                Step::CallerSees | Step::SubtreeGone | Step::SubtreeWaitExpires
            ) || self
                .step_owner(step)
                .is_some_and(|o| self.tree_of(caller.t).contains(&o));
        }
        false
    }
}

/// The depth at which a search over the progress steps of a wait gives up on
/// a branch. It is far above the deepest search any configuration needs
/// (reported with every run). The search is recursive and a frame takes about
/// 2 KB, so the cap holds only because every search runs on the 256 MB thread
/// `run_configuration` spawns, its only entry.
const BOUNDED_DEPTH_CAP: usize = 4096;

/// Existential search over the progress steps of a wait: whether some path of
/// them reaches a state where the wait is resolved or exempt.
struct BoundedSearch<'a> {
    cfg: &'a Cfg,
    seen: Visited,
    cut: bool,
    without_loop: bool,
    /// The deepest recursion any path of the search reached.
    deepest: usize,
}

impl BoundedSearch<'_> {
    fn go(&mut self, s: &State, w: Wait, depth: usize) -> bool {
        if s.wait_resolved(w) || s.exempt(self.cfg, w) {
            return true;
        }
        if depth >= BOUNDED_DEPTH_CAP {
            self.cut = true;
            return false;
        }
        self.deepest = self.deepest.max(depth);
        let key = encode(&(s, w));
        if !self.seen.insert(key.clone()) {
            return false;
        }
        let cut_outside = std::mem::replace(&mut self.cut, false);
        let steps: Vec<Step> = s
            .enabled(self.cfg)
            .into_iter()
            .filter(|st| st.is_internal() && s.in_prog(*st, w, self.without_loop))
            .collect();
        for st in steps {
            let next = apply(self.cfg, s, st).post;
            if self.go(&next, w, depth + 1) {
                self.cut |= cut_outside;
                return true;
            }
        }
        // A state whose subtree was cut at the depth cap is not settled as a
        // failure: a shorter path to it may still succeed.
        if self.cut {
            self.seen.remove(&key);
        }
        self.cut |= cut_outside;
        false
    }
}

impl State {
    /// Whether the wait reaches a resolved or exempt state, and whether the
    /// search hit the depth cap on the way.
    fn bounded(&self, cfg: &Cfg, w: Wait, without_loop: bool, deepest: &mut usize) -> (bool, bool) {
        let mut search = BoundedSearch {
            cfg,
            seen: Visited::default(),
            cut: false,
            without_loop,
            deepest: 0,
        };
        let found = search.go(self, w, 0);
        *deepest = (*deepest).max(search.deepest);
        (found, search.cut)
    }

    /// The open wait of a pending group or chain, if the group is blocked on
    /// something not yet settled: the wait the group depends on.
    fn group_open_waits(&self) -> Vec<Wait> {
        let mut v = Vec::new();
        let Some(g) = &self.sup.group else {
            return v;
        };
        match &g.phase {
            Phase::Stop {
                awaiting, queue, ..
            } => {
                if let Some(h) = queue.front() {
                    if let Some(t) = awaiting.get(h) {
                        if !self.settled(*t) {
                            v.push(Wait::Occ(*t));
                        }
                    }
                } else {
                    for t in awaiting.values() {
                        if !self.settled(*t) {
                            v.push(Wait::Occ(*t));
                        }
                    }
                }
            }
            Phase::Chain { queue, started } => {
                if let Some(f) = queue.front() {
                    match self.life(*f) {
                        Some(Life::Restarting { next, .. }) if started.get(f) == Some(&next) => {
                            if self.cell(next).order.is_some() {
                                v.push(Wait::Occ(next));
                            } else if self.cell(next).exec == Exec::RunnerFailed {
                                v.push(Wait::Lost(next));
                            } else {
                                v.push(Wait::Init(next));
                            }
                        }
                        Some(Life::Stopping { n, .. }) => v.push(Wait::Occ(n)),
                        Some(Life::Running(k)) => v.push(Wait::Occ(k)),
                        _ => {}
                    }
                }
            }
        }
        v
    }

    /// Every wait of the group is exempt, and none is a barrier that only
    /// verdicts block.
    fn group_wait_is_exempt(&self, cfg: &Cfg) -> bool {
        let ws = self.group_open_waits();
        !ws.is_empty() && ws.iter().all(|w| self.exempt(cfg, *w))
    }
}

// ---------------------------------------------------------------------------
// State checks and quiescent checks
// ---------------------------------------------------------------------------

impl State {
    /// Checks of a state, made on every first arrival.
    fn state_checks(&self, cfg: &Cfg, out: &mut Vec<&'static str>, deepest: &mut usize) {
        // Obligations never depend on the caller.
        for (&t, cell) in &self.cells {
            let raised = cell.order.is_some_and(|o| o.raised);
            if self.live(t)
                && raised
                && matches!(cell.exec, Exec::Runner | Exec::Attempt(AttemptStage::Init))
                && cell.ladder == Ladder::Off
            {
                out.push(NO_OBLIGATION_DEPENDS_ON_THE_CALLER);
            }
        }
        for x in self.all_sups() {
            for life in self.sup_ref(x).ledger.values() {
                if let Life::Stopping { n, .. } = life {
                    if self.live(*n) && self.cell(*n).order.is_none() {
                        out.push(NO_OBLIGATION_DEPENDS_ON_THE_CALLER);
                    }
                }
            }
        }
        // Every wait is bounded; a handler's wait does not need the loop.
        for w in self.waits() {
            match self.bounded(cfg, w, false, deepest) {
                (true, _) => {}
                (false, true) => out.push(BOUNDED_SEARCH_DEPTH_CAP),
                (false, false) => out.push(EVERY_WAIT_IS_BOUNDED),
            }
            if self.caller.is_some_and(|k| State::caller_wait(k) == w) {
                match self.bounded(cfg, w, true, deepest) {
                    (true, _) => {}
                    (false, true) => out.push(BOUNDED_SEARCH_DEPTH_CAP),
                    (false, false) => out.push(NO_HANDLER_WAITS_ON_THE_LOOP),
                }
            }
        }
        // One live incarnation and one live registration per child.
        let mut per_key: BTreeMap<(SupId, Key), u8> = BTreeMap::new();
        let mut per_id: BTreeMap<IdKey, u8> = BTreeMap::new();
        for (&t, inc) in &self.incs {
            if inc.end.is_none() {
                *per_key.entry((inc.sup, inc.key)).or_insert(0) += 1;
                *per_id.entry(inc.id).or_insert(0) += 1;
                if self.held.get(&inc.id) != Some(&t) {
                    out.push(AT_MOST_ONE_LIVE_REGISTRATION_PER_ID);
                }
                let occupant = if self.has_sup(inc.sup) {
                    self.occ_of(inc.sup, inc.key)
                } else {
                    None
                };
                if self.cell(t).exec != Exec::Orphan && occupant != Some(t) {
                    out.push(EVERY_LIVE_INCARNATION_IS_THE_LEDGER_OCCUPANT);
                }
            }
        }
        for n in per_key.values() {
            if *n > 1 {
                out.push(AT_MOST_ONE_LIVE_INCARNATION_PER_CHILD);
            }
        }
        for (k, n) in &per_id {
            let foreign = u8::from(self.foreign.contains(k));
            if n + foreign > 1 {
                out.push(AT_MOST_ONE_LIVE_REGISTRATION_PER_ID);
            }
        }
        for k in &self.foreign {
            if per_id.get(k).copied().unwrap_or(0) + 1 > 1 {
                out.push(AT_MOST_ONE_LIVE_REGISTRATION_PER_ID);
            }
        }
        // A down child's incarnation is over.
        for x in self.all_sups() {
            for life in self.sup_ref(x).ledger.values() {
                match life {
                    Life::Down { n, .. } if self.live(*n) => {
                        out.push(DOWN_ONLY_AFTER_THE_TASK_ENDED);
                    }
                    Life::Restarting { prev, .. } if self.live(*prev) => {
                        out.push(DOWN_ONLY_AFTER_THE_TASK_ENDED);
                    }
                    _ => {}
                }
            }
        }
        // The verdict belongs to a live stuck incarnation.
        for (&t, cell) in &self.cells {
            if cell.verdict {
                if let Some(inc) = self.incs.get(&t) {
                    if inc.end.is_none() && !matches!(inc.stage, Stage::Stuck(_)) {
                        out.push(ONE_VERDICT_AND_NO_SILENT_WEDGE);
                    }
                }
            }
        }
        // A supervisor sees an end it can see: the report is on its way.
        for x in self.all_sups() {
            if self.has_sup(x) && !self.stuck_supervisor(x) {
                for (&c, life) in &self.sup_ref(x).ledger {
                    self.check_ended_occupant_reported(x, c, *life, out);
                }
            }
        }
        // At most one live holder per name, foreign roots included.
        {
            let mut holders: BTreeMap<IdKey, u8> = BTreeMap::new();
            for inc in self.incs.values() {
                if inc.end.is_none() && cfg.kid_of(inc.sup, inc.key).named {
                    *holders.entry(inc.id).or_insert(0) += 1;
                }
            }
            for k in &self.foreign {
                *holders.entry(*k).or_insert(0) += 1;
            }
            if holders.values().any(|n| *n > 1) {
                out.push(AT_MOST_ONE_HOLDER_PER_NAME);
            }
        }
        // A supervisor child in an inherited orderly stop keeps a backstop
        // until it ends or its Kill stage fired.
        for (&t, cell) in &self.cells {
            let kid = cfg.kid_of(cell.sup, cell.key);
            if let (true, Some(inc)) = (
                cell.exec == Exec::Orphan && kid.is_supervisor,
                self.incs.get(&t),
            ) {
                if inc.end.is_none()
                    && kid.policy != Policy::Infinity
                    && inc.lane != Some(Reason::Kill)
                    && !inc.aborted
                    && !self.reaper.contains_key(&t)
                {
                    out.push(KILLED_SUPERVISOR_CHILDREN_SETTLE_IN_ORDER_WITHIN_THEIR_WINDOWS);
                }
            }
        }
        // The status plane never claims a stop while the task lives.
        for inc in self.incs.values() {
            if inc.status == Status::Stopped && inc.end.is_none() {
                out.push(STATUS_NEVER_STOPPED_WHILE_LIVE);
            }
        }
        // At most one stop the group itself signalled is unsettled.
        if let Some(Group {
            phase: Phase::Stop { awaiting, .. },
            ..
        }) = &self.sup.group
        {
            let unsettled_group_stops = awaiting
                .values()
                .filter(|t| {
                    self.spawned(**t)
                        && !self.settled(**t)
                        && self
                            .cell(**t)
                            .order
                            .is_some_and(|o| o.from == OrderFrom::Group)
                })
                .count();
            if unsettled_group_stops > 1 {
                out.push(GROUP_BARRIER_HOLDS);
            }
        }
        // The children view tells the truth.
        for &c in self.sup.ledger.keys() {
            let (alive, pending) = self.child_info_flags(c);
            let (o_alive, o_pending) = self.o_child_flags(cfg, c);
            if alive != o_alive || pending != o_pending {
                out.push(CHILDREN_VIEW_IS_TRUTHFUL);
            }
        }
    }

    /// The oracle's `children()` flags, from the incarnations.
    fn o_child_flags(&self, cfg: &Cfg, c: Key) -> (bool, bool) {
        match self.life(c) {
            None => (false, false),
            Some(Life::Running(n)) => {
                let ended = self.incs.get(&n).is_some_and(|i| i.end.is_some());
                (!ended, ended)
            }
            Some(Life::Stopping { n, .. }) => (self.live(n), false),
            Some(Life::Restarting { .. }) => (false, true),
            Some(Life::Down { pend, .. }) => (
                false,
                pend == Pend::Evaluate || self.o_membership(cfg, c) == Memb::Queued,
            ),
        }
    }

    /// An occupant whose task ended has its end on its way to the supervisor.
    fn check_ended_occupant_reported(
        &self,
        x: SupId,
        c: Key,
        life: Life,
        out: &mut Vec<&'static str>,
    ) {
        let s = self.sup_ref(x);
        let on_its_way = |n: Tok| -> bool {
            s.deaths.iter().any(|d| d.0 == c && d.1 == n)
                || s.final_q.iter().any(|d| d.0 == c && d.1 == n)
                || s.queued.iter().any(|d| d.0 == c && d.1 == n)
                || s.outcomes.contains_key(&n)
                || s.snapshots.contains_key(&(c, n))
                || (x == SupId::S && self.caller.is_some_and(|k| k.t == n))
        };
        match life {
            Life::Running(n) | Life::Stopping { n, .. } => {
                let ended = self.incs.get(&n).is_some_and(|i| i.end.is_some());
                if ended && !on_its_way(n) {
                    out.push(ONE_VERDICT_AND_NO_SILENT_WEDGE);
                }
            }
            Life::Restarting { next, .. } => {
                let cell = self.cell(next);
                let gone_executor = matches!(cell.exec, Exec::Returned | Exec::RunnerFailed);
                let task_over = !self.live(next);
                if gone_executor && task_over && !on_its_way(next) {
                    out.push(ONE_VERDICT_AND_NO_SILENT_WEDGE);
                }
            }
            Life::Down { .. } => {}
        }
    }

    /// A supervisor incarnation that never yields: its loop does not run
    /// again.
    fn stuck_supervisor(&self, x: SupId) -> bool {
        match x {
            SupId::S => false,
            SupId::Inc(p) => self
                .incs
                .get(&p)
                .is_some_and(|i| matches!(i.stage, Stage::Stuck(_))),
        }
    }

    /// Checks of a state with no internal step enabled.
    fn quiescent_checks(&self, cfg: &Cfg, out: &mut Vec<&'static str>) {
        // Once the final sweep took its snapshot, no leftover outlives the
        // global deadline and its kill grace: each has ended, or its task
        // was aborted (a task that never yields keeps running, with the
        // abort taken as its verdict), whether or not the shutdown call is
        // still driven.
        for &t in &self.oracle.sweep_leftovers {
            if self.live(t) && !self.inc(t).aborted {
                out.push(NO_LEFTOVER_OUTLIVES_THE_DEADLINE_ONCE_THE_SWEEP_STARTED);
            }
        }
        // Progress: every open change belongs to an exempt wait; the waits of
        // a supervisor that never yields are exempt with it.
        for w in self.waits() {
            let owner = match w {
                Wait::SubCaller(_) => Some(SupId::S),
                Wait::SubAttempt(_, m) => self.cells.get(&m).map(|c| c.sup),
                other => self.cells.get(&other.token()).map(|c| c.sup),
            };
            if owner.is_some_and(|x| self.stuck_supervisor(x)) {
                continue;
            }
            if !self.exempt(cfg, w) {
                out.push(ONE_VERDICT_AND_NO_SILENT_WEDGE);
            }
        }
        if self.sup_alive() {
            if let Some(g) = &self.sup.group {
                let open = self.group_open_waits();
                // A pending group with no open wait belongs to no wait.
                if open.is_empty() {
                    out.push(ONE_VERDICT_AND_NO_SILENT_WEDGE);
                }
                for w in open {
                    if !self.exempt(cfg, w) {
                        out.push(ONE_VERDICT_AND_NO_SILENT_WEDGE);
                    }
                }
                if let Phase::Stop { awaiting, .. } = &g.phase {
                    if awaiting
                        .values()
                        .any(|t| self.cell(*t).verdict && !self.ended(*t))
                    {
                        // A barrier blocked only by verdicts escalates.
                        let all_verdict = awaiting.values().all(|t| self.cell(*t).verdict);
                        if all_verdict {
                            out.push(ONE_VERDICT_AND_NO_SILENT_WEDGE);
                        }
                    }
                }
            }
        }
        // Every owed report is delivered once, or excused by a Kill, or waits
        // behind a group whose wait is exempt.
        for (&t, cell) in &self.cells {
            if cell.owed && !cell.delivered && !cell.excused {
                if self.stuck_supervisor(cell.sup) {
                    continue;
                }
                let queued = cell.sup == SupId::S && self.sup.queued.iter().any(|e| e.1 == t);
                if !(queued && self.group_wait_is_exempt(cfg)) {
                    out.push(EVERY_OWED_REPORT_IS_DELIVERED_ONCE);
                }
            }
        }
        // The serial outcome.
        match self.s_phase {
            SPhase::Loop => {
                for (&c, life) in &self.sup.ledger {
                    if cfg.kid(c).restart != Restart::Permanent
                        || self.oracle.stays_down.contains(&c)
                    {
                        continue;
                    }
                    let ok = match life {
                        Life::Running(n) | Life::Stopping { n, .. } => {
                            self.live(*n) || self.cell(*n).verdict
                        }
                        Life::Restarting { next, .. } => {
                            self.cell(*next).verdict
                                || self
                                    .waits()
                                    .iter()
                                    .any(|w| w.concerns(*next) && self.exempt(cfg, *w))
                                || (self.o_membership(cfg, c) != Memb::Outside
                                    && self.group_wait_is_exempt(cfg))
                        }
                        Life::Down { pend, .. } => {
                            self.sup.group.is_some()
                                && self.group_wait_is_exempt(cfg)
                                && (self.o_membership(cfg, c) != Memb::Outside
                                    || *pend == Pend::Evaluate)
                        }
                    };
                    if !ok {
                        out.push(THE_SERIAL_OUTCOME_HOLDS);
                    }
                }
            }
            SPhase::Teardown => {
                let ok = self.sup.teardown.as_ref().is_some_and(|d| {
                    let pending: Vec<Tok> = d
                        .current
                        .iter()
                        .map(|(_, t)| *t)
                        .chain(d.pool_wait.iter().map(|(_, t)| *t))
                        .filter(|t| !self.settled(*t))
                        .collect();
                    !pending.is_empty() && pending.iter().all(|t| self.exempt(cfg, Wait::Occ(*t)))
                });
                if !ok {
                    out.push(THE_SERIAL_OUTCOME_HOLDS);
                }
            }
            SPhase::Done(reason) => {
                let o = &self.oracle;
                let ok = if self.s_lane == Some(Reason::Kill) {
                    reason == Reason::Kill
                } else if o.failed_final(SupId::S) {
                    matches!(reason, Reason::Failure(_))
                } else {
                    match o.s_cause {
                        Some(SCause::Budget | SCause::ParentStop | SCause::Shutdown) => {
                            reason == Reason::ParentRequest
                        }
                        Some(SCause::Unresponsive) => {
                            matches!(reason, Reason::Failure(Cause::Unresponsive(_)))
                        }
                        Some(SCause::ParentKill) | None => false,
                    }
                };
                if !ok {
                    out.push(THE_SERIAL_OUTCOME_HOLDS);
                }
            }
            SPhase::Final(_) | SPhase::Hook(_) => out.push(THE_SERIAL_OUTCOME_HOLDS),
        }
        // After a later shutdown call the system is defunct or active again.
        if self.oracle.later_shutdown && self.sys == SysPhase::ShuttingDown {
            out.push(ABANDONED_SHUTDOWN_REFUSES_REGISTRATIONS_UNTIL_A_LATER_ONE_COMPLETES);
        }
        // The same for every live supervisor incarnation over its workers.
        for &p in self.psups.keys() {
            if !self.p_serial_ok(cfg, p) {
                out.push(THE_SERIAL_OUTCOME_HOLDS);
            }
        }
        // The budget against the serial count.
        for x in self.all_sups() {
            if !self.has_sup(x) {
                continue;
            }
            let expected = self.oracle.charges_of(x);
            let budget = cfg.budget_of(x);
            if self.sup_ref(x).charges != expected.min(budget)
                || self.oracle.exhausted(x) != (expected > budget)
            {
                out.push(BUDGET_MATCHES_THE_SERIAL_COUNT);
            }
        }
    }

    /// The serial outcome of a live supervisor incarnation at a quiescent
    /// state: in its loop every restartable worker lives or waits on an
    /// exempt wait, and in its teardown the driver waits on an exempt wait.
    fn p_serial_ok(&self, cfg: &Cfg, p: Tok) -> bool {
        let x = SupId::Inc(p);
        let inc = self.inc(p);
        match inc.stage {
            Stage::Idle | Stage::Turn | Stage::Gate(_) => {
                self.sup_ref(x).ledger.iter().all(|(&c, life)| {
                    if cfg.kid_of(x, c).restart != Restart::Permanent {
                        return true;
                    }
                    match life {
                        Life::Running(n) | Life::Stopping { n, .. } => {
                            self.live(*n) || self.cell(*n).verdict
                        }
                        Life::Restarting { next, .. } => {
                            self.cell(*next).verdict
                                || self
                                    .waits()
                                    .iter()
                                    .any(|w| w.concerns(*next) && self.exempt(cfg, *w))
                        }
                        Life::Down { .. } => false,
                    }
                })
            }
            Stage::Teardown => self.sup_ref(x).teardown.as_ref().is_some_and(|d| {
                let pending: Vec<Tok> = d
                    .current
                    .iter()
                    .map(|(_, t)| *t)
                    .chain(d.pool_wait.iter().map(|(_, t)| *t))
                    .filter(|t| !self.settled(*t))
                    .collect();
                !pending.is_empty() && pending.iter().all(|t| self.exempt(cfg, Wait::Occ(*t)))
            }),
            _ => true,
        }
    }
}

// ---------------------------------------------------------------------------
// Step checks: the pre-state, the step and the post-state
// ---------------------------------------------------------------------------

fn lane_weakens(old: Option<Reason>, new: Option<Reason>) -> bool {
    match (old, new) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(o), Some(n)) => {
            strength(n) < strength(o) || (strength(n) == strength(o) && rank(n) < rank(o))
        }
    }
}

fn step_checks(cfg: &Cfg, pre: &State, step: Step, ap: &Applied, out: &mut Vec<&'static str>) {
    let post = &ap.post;
    // A caller giving up changes nothing but the caller.
    if step == Step::CallerAbandons {
        let mut p = pre.clone();
        p.caller = None;
        p.oracle.pending_call = None;
        if p != *post {
            out.push(NO_OBLIGATION_DEPENDS_ON_THE_CALLER);
        }
    }
    // An order not yet raised waits only for an adoption in transit.
    for (&t, cell) in &post.cells {
        if cell.order.is_some_and(|o| !o.raised) {
            let ok = matches!(
                cell.exec,
                Exec::Attempt(AttemptStage::Wait | AttemptStage::Factory | AttemptStage::InTransit)
            ) || cell.exec.executor_gone()
                || post.incs.get(&t).is_some_and(|i| i.end.is_some());
            if !ok {
                out.push(FIRST_SIGNAL_WAITS_AT_MOST_FOR_AN_ADOPTION_IN_TRANSIT);
            }
        }
    }
    check_reports_change_only_their_occupant(pre, step, ap, out);
    check_orders_follow_the_configuration(cfg, pre, ap, out);
    // Stop requests only strengthen, and are consumed only by the loop's read.
    for (&t, inc) in &pre.incs {
        let Some(after) = post.incs.get(&t) else {
            continue;
        };
        if lane_weakens(inc.lane, after.lane) {
            out.push(STOP_REQUESTS_NEVER_WEAKEN);
        }
        if inc.unseen && !after.unseen && step != Step::Observe(t) {
            out.push(STOP_REQUESTS_NEVER_WEAKEN);
        }
    }
    if lane_weakens(pre.s_lane, post.s_lane)
        || (pre.s_unseen && !post.s_unseen && step != Step::SObserve)
    {
        out.push(STOP_REQUESTS_NEVER_WEAKEN);
    }
    check_kill_is_final(pre, step, post, out);
    // A verdict is written once, on a live stuck incarnation, and kept.
    for (&t, cell) in &pre.cells {
        if let Some(after) = post.cells.get(&t) {
            if cell.verdict && !after.verdict {
                out.push(ONE_VERDICT_AND_NO_SILENT_WEDGE);
            }
        }
    }
    for (&t, after) in &post.cells {
        if after.verdict && !pre.cells.get(&t).is_some_and(|c| c.verdict) {
            let stuck = post
                .incs
                .get(&t)
                .is_some_and(|i| i.end.is_none() && matches!(i.stage, Stage::Stuck(_)));
            if !stuck {
                out.push(ONE_VERDICT_AND_NO_SILENT_WEDGE);
            }
        }
    }
    check_chain_retry_scope(cfg, pre, step, post, out);
    // Reports carry the reason the status plane carries.
    if let Step::GateReturns(t) = step {
        match pre.inc(t).lane {
            Some(Reason::Kill) if post.inc(t).end != Some(Reason::Kill) => {
                out.push(REPORTS_CARRY_THE_REAL_REASON);
            }
            Some(l) if strength(l) == Strength::Forced && post.inc(t).stop_reason != Some(l) => {
                out.push(REPORTS_CARRY_THE_REAL_REASON);
            }
            _ => {}
        }
    }
    for (&t, inc) in &pre.incs {
        if inc.end.is_some() && post.incs.get(&t).is_some_and(|a| a.end != inc.end) {
            out.push(REPORTS_CARRY_THE_REAL_REASON);
        }
    }
    check_manual(cfg, pre, step, ap, out);
    check_tree_steps(cfg, pre, step, post, out);
}

/// The incarnations whose guard fired in a step: live before and after, taken
/// over by the system, not merely left to it by a failed runner.
fn guards_fired(pre: &State, post: &State) -> Vec<Tok> {
    post.cells
        .iter()
        .filter(|(t, after)| {
            after.exec == Exec::Orphan
                && post.live(**t)
                && pre.live(**t)
                && pre
                    .cells
                    .get(*t)
                    .is_some_and(|b| !matches!(b.exec, Exec::Orphan | Exec::RunnerFailed))
        })
        .map(|(t, _)| *t)
        .collect()
}

/// The lane a request leaves behind: the lane's acceptance rule, recomputed
/// from the old lane and the lane's mode.
fn lane_after_raise(old: Option<Reason>, mode: LaneMode, r: Reason) -> Option<Reason> {
    if mode == LaneMode::Sealed || (mode == LaneMode::KillOnly && r != Reason::Kill) {
        return old;
    }
    match old {
        None => Some(r),
        Some(cur) if strength(r) < strength(cur) => Some(cur),
        Some(cur) if strength(r) > strength(cur) || rank(r) >= rank(cur) => Some(r),
        Some(cur) => Some(cur),
    }
}

/// The step checks of the tree, the names and the system phase.
fn check_tree_steps(cfg: &Cfg, pre: &State, step: Step, post: &State, out: &mut Vec<&'static str>) {
    // A successor starts only beside a gone predecessor subtree.
    if let Step::Spawn(m) = step {
        let cell = pre.cell(m);
        if let Some(v) = pre.oracle.last_of(cell.sup, cell.key) {
            if !pre.gone(v) {
                out.push(NO_SUCCESSOR_STARTS_BESIDE_ITS_PREDECESSOR_SUBTREE);
            }
        }
        // Every registration while the system is not active fails.
        if pre.sys != SysPhase::Active && post.incs.contains_key(&m) {
            out.push(ABANDONED_SHUTDOWN_REFUSES_REGISTRATIONS_UNTIL_A_LATER_ONE_COMPLETES);
        }
    }
    if let Step::SpawnChild(c) = step {
        if post.life(c).is_some() && pre.life(c).is_none() {
            if let Some(v) = pre.oracle.last_of(SupId::S, c) {
                if !pre.gone(v) {
                    out.push(NO_SUCCESSOR_STARTS_BESIDE_ITS_PREDECESSOR_SUBTREE);
                }
            }
            if pre.sys != SysPhase::Active {
                out.push(ABANDONED_SHUTDOWN_REFUSES_REGISTRATIONS_UNTIL_A_LATER_ONE_COMPLETES);
            }
        }
    }
    // A worker spawn that meets a name an earlier incarnation's worker still
    // holds is a charge a successor wait would have avoided.
    if let Step::PreStartDone(p) = step {
        if post.incs.get(&p).is_some_and(|i| {
            i.stop_reason == Some(Reason::Failure(Cause::NameTaken)) && i.end.is_none()
                || i.end == Some(Reason::Failure(Cause::NameTaken))
        }) {
            let key = pre.cell(p).key;
            for w in 0..cfg.workers.len() as Key {
                if let Some(&h) = pre.held.get(&IdKey::W(w)) {
                    let earlier = pre.incs.get(&h).and_then(|i| i.parent).is_some_and(|q| {
                        q != p
                            && pre
                                .incs
                                .get(&q)
                                .is_some_and(|qi| qi.key == key && qi.sup == pre.cell(p).sup)
                    });
                    if earlier {
                        out.push(NO_NAME_TAKEN_CHARGE_FROM_A_PREDECESSOR_SUBTREE);
                    }
                }
            }
        }
        if pre.sys != SysPhase::Active && post.psups.get(&p).is_some_and(|s| !s.ledger.is_empty()) {
            out.push(ABANDONED_SHUTDOWN_REFUSES_REGISTRATIONS_UNTIL_A_LATER_ONE_COMPLETES);
        }
    }
    // The guards that fired in this step.
    for t in guards_fired(pre, post) {
        let after = post.cell(t);
        let kid = cfg.kid_of(after.sup, after.key);
        let (pi, qi) = (pre.inc(t), post.inc(t));
        if kid.is_supervisor {
            let want = lane_after_raise(pi.lane, pi.lane_mode, Reason::ParentKilled);
            let reaper_ok = if pi.lane == Some(Reason::Kill) {
                post.reaper.get(&t) == Some(&Reap::AbortDue)
            } else if kid.policy == Policy::Infinity {
                !post.reaper.contains_key(&t)
            } else {
                post.reaper.get(&t) == Some(&Reap::KillThenAbort)
            };
            if qi.lane != want || !reaper_ok {
                out.push(KILLED_SUPERVISOR_CHILDREN_SETTLE_IN_ORDER_WITHIN_THEIR_WINDOWS);
            }
        } else {
            let want = lane_after_raise(pi.lane, pi.lane_mode, Reason::Kill);
            if qi.lane != want || post.reaper.get(&t) != Some(&Reap::AbortDue) {
                out.push(WORKERS_UNDER_A_KILLED_SUPERVISOR_SETTLE_WITHIN_THEIR_KILL_GRACE);
            }
        }
    }
    for (&t, inc) in &pre.incs {
        let Some(after) = post.incs.get(&t) else {
            continue;
        };
        // While its Kill stage is pending, no abort reaches a supervisor child
        // but the leftover sweep after the deadline.
        if pre.reaper.get(&t) == Some(&Reap::KillThenAbort)
            && !inc.aborted
            && after.aborted
            && !matches!(step, Step::SweepAbort(_) | Step::DeadlineAbort(_))
        {
            out.push(KILLED_SUPERVISOR_CHILDREN_SETTLE_IN_ORDER_WITHIN_THEIR_WINDOWS);
        }
        // A supervisor child that chooses its stop reason with a parent kill
        // as its strongest stored reason chooses it.
        if matches!(step, Step::Observe(u) | Step::GateReturns(u) if u == t)
            && cfg.kid_of(inc.sup, inc.key).is_supervisor
            && inc.lane == Some(Reason::ParentKilled)
            && matches!(after.stage, Stage::Final | Stage::Hook)
            && after.stop_reason != Some(Reason::ParentKilled)
        {
            out.push(KILLED_SUPERVISOR_CHILDREN_SETTLE_IN_ORDER_WITHIN_THEIR_WINDOWS);
        }
        // The reason `on_stopped` received is never rewritten, but by the
        // final read of a Kill and by a failed final delivery.
        if let (Some(a), Some(b)) = (inc.stop_reason, after.stop_reason) {
            if a != b
                && b != Reason::Kill
                && !(matches!(step, Step::HookDone(u) if u == t)
                    && b == Reason::Failure(Cause::Crash))
            {
                out.push(KILLED_SUPERVISOR_CHILDREN_SETTLE_IN_ORDER_WITHIN_THEIR_WINDOWS);
            }
        }
    }
    // The pairing gate holds a covering supervisor's Kill stage while its
    // teardown still has a key to stop.
    if let Step::WindowEnds(t) | Step::ReaperKillStage(t) = step {
        let x = SupId::Inc(t);
        if pre.has_sup(x) {
            if let Some(d) = &pre.sup_ref(x).teardown {
                let busy = !d.walk.is_empty()
                    || d.current.is_some_and(|(_, u)| !pre.settled(u))
                    || d.pool_wait.iter().any(|(_, u)| !pre.settled(*u));
                if pre.oracle.p_covers.contains(&t) && busy {
                    out.push(PAIRING_RULE);
                }
            }
        }
    }
    // The system phase moves only along its steps.
    if pre.sys == SysPhase::ShuttingDown
        && post.sys != SysPhase::ShuttingDown
        && step != Step::ShutdownCompletes
    {
        out.push(ABANDONED_SHUTDOWN_REFUSES_REGISTRATIONS_UNTIL_A_LATER_ONE_COMPLETES);
    }
    if pre.sys == SysPhase::Defunct && post.sys != SysPhase::Defunct && step != Step::Reactivate {
        out.push(ABANDONED_SHUTDOWN_REFUSES_REGISTRATIONS_UNTIL_A_LATER_ONE_COMPLETES);
    }
    // An abandoned operation disarms nothing it raised.
    if matches!(step, Step::ShutdownAbandoned | Step::CallerAbandons) {
        let reaper_kept = pre.reaper.keys().all(|t| post.reaper.contains_key(t))
            && pre.sweep.keys().all(|t| post.sweep.contains_key(t));
        let abort_kept = !pre.s_abort || post.s_abort;
        let lanes_kept = pre.incs.iter().all(|(t, i)| {
            post.incs
                .get(t)
                .is_none_or(|a| !lane_weakens(i.lane, a.lane))
        });
        if !(reaper_kept && abort_kept && lanes_kept) {
            out.push(A_RAISED_KILL_SURVIVES_AN_ABANDONED_SHUTDOWN);
        }
    }
    // The leftover sweep waits for the global deadline.
    if matches!(step, Step::SweepAbort(_)) && !pre.shutdown.is_some_and(|s| s.deadline) {
        out.push(LEFTOVER_SWEEP_NEVER_CUTS_A_STILL_STOPPING_SUBTREE_BEFORE_THE_DEADLINE);
    }
    // The deadline entries act only once the global deadline has passed,
    // and an entry's abort only after its `Kill` (its kill grace between).
    if let Step::DeadlineKill(t) | Step::DeadlineAbort(t) = step {
        if pre
            .sweep
            .get(&t)
            .is_none_or(|stage| *stage == SweepStage::Armed)
        {
            out.push(LEFTOVER_SWEEP_NEVER_CUTS_A_STILL_STOPPING_SUBTREE_BEFORE_THE_DEADLINE);
        }
    }
    if let Step::DeadlineAbort(t) = step {
        if pre.sweep.get(&t) != Some(&SweepStage::AbortDue) {
            out.push(LEFTOVER_SWEEP_NEVER_CUTS_A_STILL_STOPPING_SUBTREE_BEFORE_THE_DEADLINE);
        }
    }
}

/// Every order a supervisor arms on its child raises the first signal and
/// starts the ladder its own configuration gives: `Kill` and a kill grace under
/// the `Kill` policy, `ParentRequest` and a window otherwise (an unbounded one
/// under `Infinity`); a ladder already past its start is left where it is.
/// Recomputed here from the configuration of the child's key under its own
/// supervisor, never read from what the logic chose.
fn check_orders_follow_the_configuration(
    cfg: &Cfg,
    pre: &State,
    ap: &Applied,
    out: &mut Vec<&'static str>,
) {
    let post = &ap.post;
    for (&t, cell) in &post.cells {
        let was = pre.cells.get(&t);
        let armed_now = cell.order.is_some_and(|o| o.raised)
            && !was.and_then(|c| c.order).is_some_and(|o| o.raised);
        if !armed_now {
            continue;
        }
        let kid = cfg.kid_of(cell.sup, cell.key);
        let (first, ladder) = match kid.policy {
            Policy::Kill => (Reason::Kill, Ladder::KillGrace),
            Policy::Timeout => (Reason::ParentRequest, Ladder::Window),
            Policy::Infinity => (Reason::ParentRequest, Ladder::WindowInf),
        };
        let raised_first = ap.raised.iter().find(|(u, _)| *u == t).map(|(_, r)| *r);
        if raised_first != Some(first) {
            out.push(ORDERS_FOLLOW_THE_CONFIGURATION);
        }
        let before = was.map_or(Ladder::Off, |c| c.ladder);
        let want = if before == Ladder::Off {
            ladder
        } else {
            before
        };
        if cell.ladder != want {
            out.push(ORDERS_FOLLOW_THE_CONFIGURATION);
        }
    }
}

fn check_reports_change_only_their_occupant(
    pre: &State,
    step: Step,
    ap: &Applied,
    out: &mut Vec<&'static str>,
) {
    // The draining supervisor is the one the report's incarnation belongs to.
    let reporter = match step {
        Step::DrainDeath(_, t, _) | Step::DrainOutcome(t) | Step::DrainAck(t) => t,
        _ => return,
    };
    let Some(x) = pre.cells.get(&reporter).map(|cell| cell.sup) else {
        return;
    };
    if !pre.has_sup(x) {
        return;
    }
    let (c, t) = match step {
        Step::DrainDeath(c, t, _) => (c, t),
        _ => match pre.sup_ref(x).outcomes.get(&reporter) {
            Some((c, _)) => (*c, reporter),
            None => return,
        },
    };
    let post = &ap.post;
    let occupant_report = pre.occ_of(x, c) == Some(t)
        || matches!(pre.life_of(x, c), Some(Life::Down { n, pend: Pend::Evaluate }) if n == t);
    let before = pre.life_of(x, c);
    let after = if post.has_sup(x) {
        post.life_of(x, c)
    } else {
        None
    };
    let snapshot_branch = matches!(before, Some(Life::Down { n, pend: Pend::Snapshot }) if n == t)
        && matches!(after, Some(Life::Down { n, pend: Pend::Nothing }) if n == t);
    if before != after && !occupant_report && !snapshot_branch {
        out.push(REPORTS_CHANGE_ONLY_THEIR_OCCUPANT);
    }
    // A barrier entry leaves only with the report whose token it names.
    if let Some(Group {
        phase: Phase::Stop {
            awaiting: before, ..
        },
        ..
    }) = &pre.sup_ref(x).group
    {
        let post_group = post
            .has_sup(x)
            .then(|| post.sup_ref(x).group.as_ref())
            .flatten();
        match post_group.map(|g| &g.phase) {
            Some(Phase::Stop {
                awaiting: after, ..
            }) => {
                for (k, tk) in before {
                    let own = *k == c && *tk == t;
                    if !after.contains_key(k) && !own {
                        out.push(REPORTS_CHANGE_ONLY_THEIR_OCCUPANT);
                    }
                }
            }
            Some(Phase::Chain { .. }) if before.iter().any(|(k, tk)| !(*k == c && *tk == t)) => {
                out.push(REPORTS_CHANGE_ONLY_THEIR_OCCUPANT);
            }
            Some(Phase::Chain { .. }) => {}
            None => {}
        }
    }
}

fn check_kill_is_final(pre: &State, step: Step, post: &State, out: &mut Vec<&'static str>) {
    for (&t, inc) in &pre.incs {
        let Some(after) = post.incs.get(&t) else {
            continue;
        };
        if inc.lane == Some(Reason::Kill)
            && inc.stage != after.stage
            && matches!(
                after.stage,
                Stage::PreStart | Stage::OnStarted | Stage::Gate(_) | Stage::Final | Stage::Hook
            )
        {
            out.push(KILL_IS_FINAL);
        }
        if inc.lane == Some(Reason::Kill)
            && matches!(
                step,
                Step::PFinalDeliver(u) | Step::PFinalDeliverFails(u) | Step::PFinalDone(u) if u == t
            )
        {
            out.push(KILL_IS_FINAL);
        }
        // A Kill that lands where none was has an abort armed for it.
        if inc.lane != Some(Reason::Kill) && after.lane == Some(Reason::Kill) && after.end.is_none()
        {
            let armed = post.reaper.contains_key(&t)
                || post.sweep.get(&t) == Some(&SweepStage::AbortDue)
                || post.cell(t).ladder == Ladder::KillGrace;
            if !armed {
                out.push(KILL_IS_FINAL);
            }
        }
    }
    if pre.s_lane == Some(Reason::Kill) {
        let enters_hook =
            matches!(post.s_phase, SPhase::Hook(_)) && !matches!(pre.s_phase, SPhase::Hook(_));
        let final_step = matches!(
            step,
            Step::FinalDeliver | Step::FinalDeliverFails | Step::FinalDone
        );
        if enters_hook || final_step {
            out.push(KILL_IS_FINAL);
        }
    }
    if pre.s_lane != Some(Reason::Kill)
        && post.s_lane == Some(Reason::Kill)
        && !matches!(post.s_phase, SPhase::Done(_))
        && !post.s_abort
    {
        out.push(KILL_IS_FINAL);
    }
}

fn check_chain_retry_scope(
    cfg: &Cfg,
    pre: &State,
    step: Step,
    post: &State,
    out: &mut Vec<&'static str>,
) {
    let drained = match step {
        Step::DrainOutcome(m) => match pre.sup.outcomes.get(&m) {
            Some((c, Outcome::Failed(_))) => Some((*c, m)),
            _ => None,
        },
        Step::DrainDeath(c, t, _) if matches!(pre.life(c), Some(Life::Restarting { next, .. }) if next == t) => {
            Some((c, t))
        }
        _ => None,
    };
    let Some((f, m)) = drained else {
        return;
    };
    let Some(g) = &pre.sup.group else {
        return;
    };
    let own_attempt = match &g.phase {
        Phase::Chain { queue, started } => queue.front() == Some(&f) && started.get(&f) == Some(&m),
        Phase::Stop { .. } => false,
    };
    if !own_attempt || post.s_phase != SPhase::Loop {
        return;
    }
    let Some(pg) = &post.sup.group else {
        out.push(CHAIN_ORDER_AND_RETRY_SCOPE);
        return;
    };
    let mut expected_members: BTreeMap<Key, Option<Tok>> = BTreeMap::new();
    let mut seen_f = false;
    for k in &pre.sup.order {
        if *k == f {
            seen_f = true;
        }
        let in_scope = match g.kind {
            GKind::All => true,
            GKind::Rest => seen_f,
        };
        if in_scope {
            expected_members.insert(*k, if *k == f { None } else { pre.occ(*k) });
        }
    }
    let restart_order: Vec<Key> = pre
        .sup
        .order
        .iter()
        .copied()
        .filter(|k| expected_members.contains_key(k))
        .filter(|k| cfg.kid(*k).restart != Restart::Temporary)
        .filter(|k| !g.overrides.contains(k))
        .collect();
    let awaiting_expected: BTreeMap<Key, Tok> = expected_members
        .iter()
        .filter(|(k, _)| **k != f)
        .filter_map(|(k, o)| o.map(|t| (*k, t)))
        .collect();
    let ok_members = pg.members == expected_members;
    let ok_phase = match &pg.phase {
        Phase::Stop {
            awaiting,
            restart_order: ro,
            ..
        } => *awaiting == awaiting_expected && *ro == restart_order,
        Phase::Chain { queue, .. } => {
            awaiting_expected.is_empty() && queue.iter().copied().eq(restart_order.iter().copied())
        }
    };
    if !(ok_members && ok_phase) {
        out.push(CHAIN_ORDER_AND_RETRY_SCOPE);
    }
}

// ---------------------------------------------------------------------------
// The manual table, written out literally for the check
// ---------------------------------------------------------------------------

/// What a manual call must do, read off the table from the state before it.
#[derive(Default, Debug)]
struct Exp {
    /// `None` when the call leaves a caller waiting.
    answer: Option<Answer>,
    caller: Option<(Tok, Path)>,
    commit: Option<(Tok, Kind)>,
    upgrade: Option<Tok>,
    order_written: Option<Tok>,
    raises: Vec<(Tok, Reason)>,
    reasked: Option<Tok>,
    overrides_added: bool,
    leaves_restart_set: bool,
    classified: Option<(Tok, Kind)>,
    dismissed: Option<Tok>,
    rewritten: Option<Tok>,
    initiated: bool,
    removed_spec: bool,
    spawned: bool,
    /// The cell is a `terminate_child`'s, or a `stop_child` handled as one.
    terminate_like: bool,
    /// After the call completes, the handler waits for the subtree of this
    /// incarnation to end.
    subtree: Option<Tok>,
    /// A same-id spawn waits for the retired subtree of this incarnation.
    spawn_wait: Option<Tok>,
}

impl Exp {
    fn answer(a: Answer) -> Exp {
        Exp {
            answer: Some(a),
            ..Exp::default()
        }
    }

    fn effect_free(&self) -> bool {
        self.caller.is_none()
            && self.commit.is_none()
            && self.upgrade.is_none()
            && self.order_written.is_none()
            && self.raises.is_empty()
            && self.reasked.is_none()
            && !self.overrides_added
            && !self.leaves_restart_set
            && self.classified.is_none()
            && self.dismissed.is_none()
            && self.rewritten.is_none()
            && !self.initiated
            && !self.removed_spec
            && !self.spawned
            && self.subtree.is_none()
            && self.spawn_wait.is_none()
    }
}

impl State {
    /// The table's row, read from the incarnations instead of the cells.
    fn o_row(&self, c: Key) -> Row {
        let ended = |n: Tok| self.incs.get(&n).is_some_and(|i| i.end.is_some());
        match self.life(c) {
            None => Row::NoSpec,
            Some(Life::Running(n)) => {
                if ended(n) {
                    Row::RunningEnded
                } else {
                    Row::RunningLive
                }
            }
            Some(Life::Stopping { n, kind }) => match (kind, ended(n)) {
                (Kind::Bounce, false) => Row::BounceLive,
                (Kind::Bounce, true) => Row::BounceEnded,
                (Kind::Terminate, false) => Row::TerminateLive,
                (Kind::Terminate, true) => Row::TerminateEnded,
            },
            Some(Life::Restarting { .. }) => Row::Attempt,
            Some(Life::Down { pend, .. }) => match pend {
                Pend::Nothing => Row::DownPlain,
                Pend::Snapshot => Row::DownSnapshot,
                Pend::Evaluate => Row::DownOwed,
            },
        }
    }
}

fn exp_ask(cfg: &Cfg, pre: &State, c: Key, n: Tok, gated: bool, e: &mut Exp) {
    let inc = pre.inc(n);
    if inc.end.is_none()
        && cfg.kid(c).policy == Policy::Infinity
        && vetoable(inc.lane)
        && !(gated && inc.reasked)
    {
        e.raises.push((n, Reason::ParentRequest));
        if gated {
            e.reasked = Some(n);
        }
    }
}

fn exp_finish(pre: &State, n: Tok, kind: Kind, e: &mut Exp, join_path: Path) {
    if pre.incs.get(&n).is_some_and(|i| i.end.is_some()) {
        e.classified = Some((n, kind));
        e.answer = Some(Answer::Ok);
    } else if pre.cell(n).verdict {
        e.answer = Some(Answer::ChildUnresponsive);
    } else {
        e.caller = Some((n, join_path));
        e.answer = None;
    }
}

fn exp_join(cfg: &Cfg, pre: &State, c: Key, n: Tok, kind: Kind, gated: bool) -> Exp {
    let mut e = Exp::default();
    exp_ask(cfg, pre, c, n, gated, &mut e);
    exp_finish(pre, n, kind, &mut e, Path::Join);
    e
}

/// A new order raises its first signal only where an executor runs a ladder.
fn exp_first_signal(cfg: &Cfg, pre: &State, c: Key, n: Tok, e: &mut Exp) {
    let runs_ladder = matches!(
        pre.cell(n).exec,
        Exec::Runner | Exec::Attempt(AttemptStage::Init)
    ) && pre.incs.get(&n).is_some_and(|i| i.end.is_none());
    if runs_ladder {
        e.raises.push((n, cfg.first_signal(c)));
    }
}

fn exp_commit(cfg: &Cfg, pre: &State, c: Key, n: Tok, kind: Kind) -> Exp {
    let mut e = Exp {
        answer: None,
        caller: Some((n, Path::Commit)),
        commit: Some((n, kind)),
        order_written: Some(n),
        ..Exp::default()
    };
    exp_first_signal(cfg, pre, c, n, &mut e);
    e
}

fn exp_override(cfg: &Cfg, pre: &State, c: Key, n: Tok, row: Row) -> Exp {
    let mut e = Exp {
        overrides_added: true,
        leaves_restart_set: true,
        ..Exp::default()
    };
    match row {
        Row::RunningLive => {
            if pre.cell(n).order.is_none() {
                e.order_written = Some(n);
                exp_first_signal(cfg, pre, c, n, &mut e);
            } else {
                exp_ask(cfg, pre, c, n, true, &mut e);
            }
        }
        Row::BounceLive => {
            e.upgrade = Some(n);
            exp_ask(cfg, pre, c, n, true, &mut e);
        }
        Row::TerminateLive => exp_ask(cfg, pre, c, n, true, &mut e),
        Row::BounceEnded => e.upgrade = Some(n),
        _ => {}
    }
    exp_finish(pre, n, Kind::Terminate, &mut e, Path::Override);
    e
}

fn keep_applies(pre: &State, c: Key, memb: Memb) -> bool {
    let Some(g) = &pre.sup.group else {
        return false;
    };
    memb == Memb::Outside
        && match g.kind {
            GKind::All => true,
            GKind::Rest => !pre.oracle.group_outside.contains(&c),
        }
}

fn exp_terminate(cfg: &Cfg, pre: &State, c: Key, row: Row, memb: Memb) -> Exp {
    let keep = keep_applies(pre, c, memb);
    let n = match pre.life(c) {
        Some(Life::Running(n) | Life::Stopping { n, .. } | Life::Down { n, .. }) => n,
        _ => 0,
    };
    let mut e = match (memb, row) {
        (_, Row::NoSpec) => return Exp::answer(Answer::ChildNotFound),
        (_, Row::Attempt) => return Exp::answer(Answer::ChildRestarting),
        (Memb::Outside, Row::RunningLive) => exp_commit(cfg, pre, c, n, Kind::Terminate),
        (Memb::Outside, Row::RunningEnded) => Exp {
            classified: Some((n, Kind::Terminate)),
            answer: Some(Answer::Ok),
            ..Exp::default()
        },
        (Memb::Outside, Row::BounceLive | Row::BounceEnded) => {
            let mut e = exp_join(cfg, pre, c, n, Kind::Terminate, keep);
            e.upgrade = Some(n);
            e
        }
        (Memb::Outside, Row::TerminateLive | Row::TerminateEnded) => {
            exp_join(cfg, pre, c, n, Kind::Terminate, keep)
        }
        (Memb::Released, Row::TerminateLive | Row::TerminateEnded) => {
            exp_join(cfg, pre, c, n, Kind::Terminate, true)
        }
        (Memb::Outside | Memb::Released, Row::DownPlain | Row::DownSnapshot) => {
            Exp::answer(Answer::Ok)
        }
        (Memb::Outside, Row::DownOwed) => Exp {
            dismissed: Some(n),
            answer: Some(Answer::Ok),
            ..Exp::default()
        },
        (Memb::Awaited, _) => exp_override(cfg, pre, c, n, row),
        (Memb::Queued, Row::DownPlain) => Exp {
            overrides_added: true,
            leaves_restart_set: true,
            answer: Some(Answer::Ok),
            ..Exp::default()
        },
        (Memb::Queued, Row::DownSnapshot) => Exp {
            overrides_added: true,
            leaves_restart_set: true,
            rewritten: if pre
                .sup
                .snapshots
                .get(&(c, n))
                .is_some_and(|s| s.1 == Action::RestartInitiated)
            {
                Some(n)
            } else {
                None
            },
            answer: Some(Answer::Ok),
            ..Exp::default()
        },
        (Memb::Queued, Row::DownOwed) => Exp {
            overrides_added: true,
            leaves_restart_set: true,
            dismissed: Some(n),
            answer: Some(Answer::Ok),
            ..Exp::default()
        },
        _ => return Exp::answer(Answer::ChildRestarting),
    };
    let spec_kept = !(cfg.prunable(c) && (e.classified.is_some() || e.dismissed.is_some()));
    if keep && (e.answer == Some(Answer::Ok) || e.caller.is_some()) && spec_kept {
        e.overrides_added = true;
    }
    e.terminate_like = true;
    e
}

/// After a terminate completes its stop, the handler waits for the subtree of
/// the incarnation it stopped, or of the last incarnation of a child that was
/// already down, to end.
fn exp_subtree(pre: &State, c: Key, e: &mut Exp) {
    if !e.terminate_like || e.answer != Some(Answer::Ok) {
        return;
    }
    if let Some(t) = exp_target(pre, c, e).filter(|t| !pre.gone(*t)) {
        e.answer = None;
        e.subtree = Some(t);
    }
}

/// The incarnation a terminate stops, or the last one of a child already
/// down, read from the facts of the state before the call.
fn exp_target(pre: &State, c: Key, e: &Exp) -> Option<Tok> {
    match e.classified.map(|x| x.0) {
        Some(t) => Some(t),
        None => match pre.life(c) {
            Some(Life::Running(n) | Life::Stopping { n, .. }) => Some(n),
            _ => pre.oracle.last_of(SupId::S, c),
        },
    }
}

fn exp_stop(cfg: &Cfg, pre: &State, c: Key, row: Row, memb: Memb) -> Exp {
    if row == Row::NoSpec {
        return Exp::answer(Answer::ChildNotFound);
    }
    let n = match pre.life(c) {
        Some(Life::Running(n) | Life::Stopping { n, .. }) => n,
        _ => 0,
    };
    let perm = cfg.kid(c).restart == Restart::Permanent;
    if let Some(g) = &pre.sup.group {
        let overridden = g.overrides.contains(&c);
        let restarted = pre.oracle.group_log.contains(&c);
        let front_attempt = memb == Memb::Queued && row == Row::Attempt;
        let ahead = (memb == Memb::Awaited && !overridden)
            || (memb == Memb::Queued && !front_attempt && !overridden);
        if ahead {
            return if perm {
                Exp::answer(Answer::Absorbed)
            } else {
                exp_terminate(cfg, pre, c, row, memb)
            };
        }
        if front_attempt {
            return Exp::answer(Answer::ChildRestarting);
        }
        if overridden || restarted {
            return if perm {
                Exp::answer(Answer::ChildRestarting)
            } else {
                exp_terminate(cfg, pre, c, row, memb)
            };
        }
        // A child started while the group pends, in the scope a terminate's
        // record covers: a non-`Permanent` one is stopped as its terminate.
        if !perm && keep_applies(pre, c, memb) {
            return exp_terminate(cfg, pre, c, row, memb);
        }
    }
    match row {
        Row::NoSpec => Exp::answer(Answer::ChildNotFound),
        Row::RunningLive => exp_commit(cfg, pre, c, n, Kind::Bounce),
        Row::RunningEnded => Exp {
            classified: Some((n, Kind::Bounce)),
            answer: Some(Answer::Ok),
            ..Exp::default()
        },
        Row::BounceLive | Row::BounceEnded => exp_join(cfg, pre, c, n, Kind::Bounce, false),
        Row::TerminateLive | Row::TerminateEnded => {
            exp_join(cfg, pre, c, n, Kind::Terminate, false)
        }
        Row::Attempt | Row::DownOwed => Exp::answer(Answer::ChildRestarting),
        Row::DownPlain | Row::DownSnapshot => Exp::answer(Answer::Ok),
    }
}

fn exp_restart(pre: &State, c: Key, row: Row, memb: Memb) -> Exp {
    if row == Row::NoSpec {
        return Exp::answer(Answer::ChildNotFound);
    }
    if memb != Memb::Outside {
        return Exp::answer(Answer::ChildRestarting);
    }
    let restarted = pre.oracle.group_log.contains(&c);
    match row {
        Row::RunningLive => Exp::answer(if restarted {
            Answer::ChildRestarting
        } else {
            Answer::ChildRunning
        }),
        Row::DownPlain | Row::DownSnapshot => Exp {
            answer: Some(Answer::Ok),
            initiated: true,
            ..Exp::default()
        },
        _ => Exp::answer(Answer::ChildRestarting),
    }
}

fn exp_delete(row: Row, memb: Memb) -> Exp {
    match (memb, row) {
        (_, Row::NoSpec) => Exp::answer(Answer::ChildNotFound),
        (Memb::Awaited | Memb::Queued, _) => Exp::answer(Answer::ChildRestarting),
        (_, Row::DownPlain | Row::DownSnapshot) => Exp {
            answer: Some(Answer::Ok),
            removed_spec: true,
            ..Exp::default()
        },
        (_, Row::RunningLive) => Exp::answer(Answer::ChildRunning),
        _ => Exp::answer(Answer::ChildRestarting),
    }
}

fn exp_spawn(pre: &State, cfg: &Cfg, c: Key) -> Exp {
    if pre.life(c).is_some() {
        return Exp::answer(Answer::DuplicateChild);
    }
    // A key whose last incarnation's subtree still runs: the handler waits.
    if let Some(t) = pre.oracle.last_of(SupId::S, c).filter(|t| !pre.gone(*t)) {
        return Exp {
            answer: None,
            spawn_wait: Some(t),
            ..Exp::default()
        };
    }
    exp_spawn_proceed(pre, cfg, c)
}

fn exp_spawn_proceed(pre: &State, cfg: &Cfg, c: Key) -> Exp {
    if pre.sys != SysPhase::Active {
        return Exp::answer(Answer::SystemShuttingDown);
    }
    if pre.held.contains_key(&IdKey::S(c)) || pre.foreign.contains(&IdKey::S(c)) {
        return Exp::answer(if cfg.kid(c).named {
            Answer::NameTaken
        } else {
            Answer::DuplicateChild
        });
    }
    Exp {
        answer: Some(Answer::Ok),
        spawned: true,
        ..Exp::default()
    }
}

/// Where the classification of a manual completion leaves the child.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LifeAfter {
    Down(Pend),
    Removed,
    Restarting,
}

/// The decision of a manual completion, from the table of its rows.
fn exp_class(
    cfg: &Cfg,
    pre: &State,
    c: Key,
    kind: Kind,
    r: Reason,
    overridden_now: bool,
) -> (LifeAfter, Option<Action>) {
    let restart = cfg.kid(c).restart;
    let in_restart_set = pre.sup.group.is_some()
        && !overridden_now
        && matches!(pre.o_membership(cfg, c), Memb::Awaited | Memb::Queued)
        && restart != Restart::Temporary;
    if kind == Kind::Bounce && matches!(r, Reason::Failure(_)) {
        return (LifeAfter::Down(Pend::Evaluate), None);
    }
    if in_restart_set {
        return (
            LifeAfter::Down(Pend::Snapshot),
            Some(Action::RestartInitiated),
        );
    }
    let ends = |prunable: bool| {
        if prunable {
            (LifeAfter::Removed, Some(Action::Removed))
        } else {
            (LifeAfter::Down(Pend::Snapshot), Some(Action::Removed))
        }
    };
    match kind {
        Kind::Terminate => ends(cfg.prunable(c)),
        Kind::Bounce => {
            if cfg.is_pool() {
                (LifeAfter::Removed, Some(Action::Removed))
            } else if restart == Restart::Permanent {
                (LifeAfter::Restarting, Some(Action::RestartInitiated))
            } else {
                ends(cfg.prunable(c))
            }
        }
    }
}

fn same_life(after: Option<Life>, want: Option<LifeAfter>, n: Tok) -> bool {
    match (after, want) {
        (None, Some(LifeAfter::Removed)) => true,
        (Some(Life::Down { n: m, pend }), Some(LifeAfter::Down(p))) => m == n && pend == p,
        (Some(Life::Restarting { prev, .. }), Some(LifeAfter::Restarting)) => prev == n,
        _ => false,
    }
}

fn check_manual(cfg: &Cfg, pre: &State, step: Step, ap: &Applied, out: &mut Vec<&'static str>) {
    let post = &ap.post;
    let (exp, c, op) = match step {
        Step::StopChild(c)
        | Step::TerminateChild(c)
        | Step::RestartChild(c)
        | Step::DeleteChild(c)
        | Step::SpawnChild(c) => {
            let row = pre.o_row(c);
            let memb = pre.o_membership(cfg, c);
            match step {
                Step::StopChild(_) => (exp_stop(cfg, pre, c, row, memb), c, CallOp::Stop),
                Step::TerminateChild(_) => {
                    (exp_terminate(cfg, pre, c, row, memb), c, CallOp::Terminate)
                }
                Step::RestartChild(_) => (exp_restart(pre, c, row, memb), c, CallOp::Restart),
                Step::DeleteChild(_) => (exp_delete(row, memb), c, CallOp::Delete),
                _ => (exp_spawn(pre, cfg, c), c, CallOp::Spawn),
            }
        }
        Step::SubtreeGone | Step::SubtreeWaitExpires => {
            let Some(rec) = pre.oracle.pending_call else {
                out.push(MANUAL_RESULTS_MATCH_THE_TABLE);
                return;
            };
            let caller = pre.caller.expect("model invariant: caller");
            let t = caller.t;
            let e = match (step, caller.kind) {
                (Step::SubtreeGone, CKind::TermSubtree) if pre.gone(t) => Exp::answer(Answer::Ok),
                (Step::SubtreeGone, CKind::SpawnWait) if pre.gone(t) => {
                    exp_spawn_proceed(pre, cfg, rec.c)
                }
                (Step::SubtreeWaitExpires, CKind::TermSubtree) if pre.expirable(cfg, t) => {
                    Exp::answer(Answer::ChildUnresponsive)
                }
                (Step::SubtreeWaitExpires, CKind::SpawnWait) if pre.expirable(cfg, t) => {
                    Exp::answer(Answer::SpawnUnresponsive)
                }
                _ => {
                    out.push(MANUAL_RESULTS_MATCH_THE_TABLE);
                    return;
                }
            };
            (e, rec.c, rec.op)
        }
        Step::CallerSees => {
            let Some(rec) = pre.oracle.pending_call else {
                out.push(MANUAL_RESULTS_MATCH_THE_TABLE);
                return;
            };
            let caller = pre.caller.expect("model invariant: caller");
            let t = caller.t;
            let ended = pre.incs.get(&t).is_some_and(|i| i.end.is_some());
            let mut e = Exp::default();
            if ended {
                e.answer = Some(Answer::Ok);
                match pre.life(rec.c) {
                    Some(Life::Stopping { n, kind }) if n == t => e.classified = Some((t, kind)),
                    Some(Life::Running(n)) if n == t && caller.path == Path::Override => {
                        e.classified = Some((t, Kind::Terminate));
                    }
                    _ => {}
                }
                // A terminate's answer waits for the subtree of what it stopped.
                if e.classified.is_some_and(|x| x.1 == Kind::Terminate) {
                    e.terminate_like = true;
                    exp_subtree(pre, rec.c, &mut e);
                }
            } else if pre.cell(t).verdict {
                e.answer = Some(Answer::ChildUnresponsive);
            } else {
                out.push(MANUAL_RESULTS_MATCH_THE_TABLE);
                return;
            }
            (e, rec.c, rec.op)
        }
        _ => return,
    };
    let mut exp = exp;
    if !matches!(
        step,
        Step::CallerSees | Step::SubtreeGone | Step::SubtreeWaitExpires
    ) {
        exp_subtree(pre, c, &mut exp);
    }
    // A terminate answers `Ok` only once the subtree of what it stopped, or
    // of the child's last incarnation, has ended.
    if ap.answer == Some(Answer::Ok) {
        let target = match step {
            Step::SubtreeGone => pre.caller.map(|k| k.t),
            Step::CallerSees => exp
                .classified
                .filter(|x| x.1 == Kind::Terminate)
                .map(|x| x.0),
            _ if exp.terminate_like => exp_target(pre, c, &exp),
            _ => None,
        };
        if target.is_some_and(|t| !pre.gone(t))
            && !(step == Step::SubtreeGone
                && pre.caller.is_some_and(|k| k.kind == CKind::SpawnWait))
        {
            out.push(NO_SUCCESSOR_STARTS_BESIDE_ITS_PREDECESSOR_SUBTREE);
        }
    }
    let mut bad = false;
    if ap.answer != exp.answer {
        bad = true;
    }
    let want_caller = if let Some(t) = exp.subtree {
        Some(Caller {
            c,
            t,
            path: Path::Commit,
            kind: CKind::TermSubtree,
        })
    } else if let Some(t) = exp.spawn_wait {
        Some(Caller {
            c,
            t,
            path: Path::Commit,
            kind: CKind::SpawnWait,
        })
    } else {
        exp.caller.map(|(t, p)| Caller {
            c,
            t,
            path: p,
            kind: CKind::Stop,
        })
    };
    if post.caller != want_caller {
        bad = true;
    }
    // The raises the call made, the first signal of a new order included.
    let mut got = ap.raised.clone();
    let mut want = exp.raises.clone();
    got.sort_unstable();
    want.sort_unstable();
    if got != want {
        bad = true;
    }
    if let Some((n, kind)) = exp.commit {
        if post.life(c) != Some(Life::Stopping { n, kind }) {
            bad = true;
        }
    }
    if let Some(n) = exp.order_written {
        let now = post.cells.get(&n).and_then(|x| x.order);
        let before = pre.cells.get(&n).and_then(|x| x.order);
        if before.is_some() || now.map(|o| o.from) != Some(OrderFrom::Manual) {
            bad = true;
        }
    } else {
        // No order is written by a call that expects none.
        for (t, cell) in &post.cells {
            if cell.order.is_some() && pre.cells.get(t).is_some_and(|x| x.order.is_none()) {
                bad = true;
            }
        }
    }
    if let Some(n) = exp.reasked {
        if !post.incs.get(&n).is_some_and(|i| i.reasked) {
            bad = true;
        }
    } else if op != CallOp::Spawn {
        for (t, inc) in &post.incs {
            if inc.reasked && pre.incs.get(t).is_some_and(|x| !x.reasked) {
                bad = true;
            }
        }
    }
    // Overrides and the restart set.
    let pre_over: BTreeSet<Key> = pre
        .sup
        .group
        .as_ref()
        .map(|g| g.overrides.clone())
        .unwrap_or_default();
    let post_over: BTreeSet<Key> = post
        .sup
        .group
        .as_ref()
        .map(|g| g.overrides.clone())
        .unwrap_or_default();
    let added: BTreeSet<Key> = post_over.difference(&pre_over).copied().collect();
    if pre.sup.group.is_some() && post.sup.group.is_some() {
        // A pruned spec takes its override with it.
        let spec_removed = cfg.prunable(c) && (exp.classified.is_some() || exp.dismissed.is_some());
        let only_c = added.iter().all(|k| *k == c);
        let kept = exp.overrides_added && !spec_removed;
        if !only_c
            || (kept && !post_over.contains(&c))
            || (!exp.overrides_added && !added.is_empty())
            || (spec_removed && post_over.contains(&c))
        {
            bad = true;
        }
    }
    if exp.leaves_restart_set {
        if let Some(g) = &post.sup.group {
            if g.phase.restart_set_contains(c) {
                bad = true;
            }
        }
    }
    // The child's ledger entry after the call.
    let n_of = |s: &State| -> Tok {
        match s.life(c) {
            Some(Life::Running(n) | Life::Stopping { n, .. } | Life::Down { n, .. }) => n,
            Some(Life::Restarting { prev, .. }) => prev,
            None => 0,
        }
    };
    let n0 = n_of(pre);
    let mut want_life: Option<Option<LifeAfter>> = None;
    let mut want_snapshot: Option<(Tok, Option<(Reason, Action)>)> = None;
    if let Some((n, kind)) = exp.classified {
        let r = pre.incs.get(&n).and_then(|i| i.end).unwrap_or(Reason::Kill);
        let overridden_now = exp.overrides_added || pre_over.contains(&c);
        let (la, act) = exp_class(cfg, pre, c, kind, r, overridden_now);
        want_life = Some(Some(la));
        want_snapshot = Some((n, act.map(|a| (r, a))));
    } else if let Some(n) = exp.dismissed {
        let r = pre
            .sup
            .queued
            .iter()
            .find(|e| e.0 == c && e.1 == n)
            .map(|e| e.2)
            .or_else(|| {
                pre.sup
                    .deaths
                    .iter()
                    .find(|e| e.0 == c && e.1 == n)
                    .map(|e| e.2)
            });
        let la = if cfg.prunable(c) {
            LifeAfter::Removed
        } else {
            LifeAfter::Down(Pend::Snapshot)
        };
        want_life = Some(Some(la));
        want_snapshot = Some((n, r.map(|r| (r, Action::Removed))));
    } else if let Some(n) = exp.rewritten {
        let r = pre.sup.snapshots.get(&(c, n)).map(|s| s.0);
        want_snapshot = Some((n, r.map(|r| (r, Action::Removed))));
    }
    if let Some(Some(la)) = want_life {
        if !same_life(post.life(c), Some(la), n0) {
            bad = true;
        }
    }
    if let Some((n, snap)) = want_snapshot {
        if post.sup.snapshots.get(&(c, n)).copied() != snap {
            bad = true;
        }
    }
    if let Some(n) = exp.upgrade {
        if exp.classified.is_none()
            && post.life(c)
                != Some(Life::Stopping {
                    n,
                    kind: Kind::Terminate,
                })
        {
            bad = true;
        }
    }
    if exp.initiated && !matches!(post.life(c), Some(Life::Restarting { .. })) {
        bad = true;
    }
    if exp.removed_spec && post.life(c).is_some() {
        bad = true;
    }
    if exp.spawned && !matches!(post.life(c), Some(Life::Running(_))) {
        bad = true;
    }
    // A call with no effect to expect leaves the supervision state alone.
    if exp.effect_free()
        && !matches!(
            step,
            Step::CallerSees | Step::SubtreeGone | Step::SubtreeWaitExpires
        )
        && (pre.sup != post.sup
            || pre.cells != post.cells
            || pre.incs != post.incs
            || pre.caller != post.caller
            || pre.reaper != post.reaper
            || pre.held != post.held)
    {
        bad = true;
    }
    if bad {
        out.push(MANUAL_RESULTS_MATCH_THE_TABLE);
    }
}

// ---------------------------------------------------------------------------
// Conformance of the production decision functions
// ---------------------------------------------------------------------------
//
// Each function of `rules` is checked in two forms. Site conformance: at every
// step of the model that takes the decision, the function is called with the
// production image of the model's inputs and its answer must equal the model's
// decision. Domain conformance: once per run, the function is evaluated over a
// finite domain of inputs, against the model's own function written below, so a
// change that only shows on an input the model never produces still fails.
//
// A conformance failure is recorded under the name of the check that owns the
// decision, with the function's name in parentheses, and never stops the search
// from expanding the state: a change to the model must not hide the checks that
// name it.

use std::time::{Duration, Instant as StdInstant};

use super::rules::{
    self, Absorption, AttemptAction, AttemptEvent, AttemptLadder, AttemptReport,
    AttemptStage as RAttemptStage, AttemptView, Commit, CommitEffect, GroupMember, GroupMembership,
    LadderAction, LadderEvent, LadderStep, ManualOp, ManualReply, QueueHead, QueueStep,
    ReportPlace, Signal, TaskState, TeardownAction, WalkChild,
};
use super::{
    ChildLifecycle, ChildSpec, ChildState, DownPending, GroupPhase, GroupRestart, GroupScope,
    ManualStop, Origin as ProdOrigin, SupervisionConfig, SupervisionState,
};
use crate::types::{
    ActorId, ChildEvent, LaneState, RaiseResult, RestartStrategy, RestartType, Shutdown,
    StopReason, SupervisionAction,
};
use std::collections::{HashMap, VecDeque as ProdQueue};

const LADDER_STEP_CONFORMANCE: &str = "every_wait_is_bounded (conformance of ladder_step)";
const MANUAL_COMMIT_CONFORMANCE: &str =
    "manual_results_match_the_table (conformance of manual_commit)";
const MANUAL_ANSWER_CONFORMANCE: &str =
    "manual_results_match_the_table (conformance of manual_answer)";
const GROUP_MEMBERSHIP_CONFORMANCE: &str =
    "manual_results_match_the_table (conformance of group_membership)";
const IS_LEGAL_CONFORMANCE: &str = "every_transition_is_in_the_table (conformance of is_legal)";
const REPORT_CLASS_CONFORMANCE: &str =
    "reports_change_only_their_occupant (conformance of report_class)";
const CHILD_INFO_FLAGS_CONFORMANCE: &str =
    "children_view_is_truthful (conformance of child_info_flags)";
const CHAIN_FRONT_CONFORMANCE: &str =
    "no_group_completes_on_an_incarnation_its_chain_did_not_start (conformance of chain_front)";
const CLASSIFY_MANUAL_CONFORMANCE: &str =
    "reports_carry_the_real_reason (conformance of classify_manual)";
const ATTEMPT_ROUTE_CONFORMANCE: &str =
    "budget_matches_the_serial_count (conformance of attempt_failure_route)";
const ATTEMPT_STEP_CONFORMANCE: &str = "every_wait_is_bounded (conformance of attempt_step)";
const GROUP_COMMIT_CONFORMANCE: &str = "group_barrier_holds (conformance of group_commit)";
const STOP_QUEUE_NEXT_CONFORMANCE: &str = "group_barrier_holds (conformance of stop_queue_next)";
const RETRY_PLAN_CONFORMANCE: &str = "chain_order_and_retry_scope (conformance of retry_plan)";
const IN_RESTART_SET_CONFORMANCE: &str =
    "reports_carry_the_real_reason (conformance of in_restart_set)";
const GROUP_MEMBER_END_CONFORMANCE: &str = "group_barrier_holds (conformance of group_member_end)";
const TEARDOWN_WALK_CONFORMANCE: &str =
    "stopped_only_when_every_child_is_settled (conformance of teardown_walk)";
const ABSORBABLE_CONFORMANCE: &str = "budget_matches_the_serial_count (conformance of absorbable)";
/// The model's own decision tables (`m_attempt_step`, `m_commit`,
/// `m_teardown`) against the decisions the model took.
const MODEL_TABLE_CONFORMANCE: &str =
    "model_tables_restate_the_model (self-check of m_attempt_step, m_commit and m_teardown)";
const BARRIER_VERDICT_CONFORMANCE: &str =
    "one_verdict_and_no_silent_wedge (conformance of barrier_blocked_only_by_verdicts)";
const LANE_ACCEPT_CONFORMANCE: &str = "stop_requests_never_weaken (conformance of lane_accept)";
const INIT_KILL_CHECK_CONFORMANCE: &str = "kill_is_final (conformance of init_kill_check)";
const INHERITED_STOP_CONFORMANCE: &str =
    "killed_supervisor_children_settle_in_order_within_their_windows (conformance of inherited_stop)";
const OWN_BUDGET_CONFORMANCE: &str =
    "no_successor_starts_beside_its_predecessor_subtree (conformance of own_budget)";
const OWN_FIN_CONFORMANCE: &str =
    "no_successor_starts_beside_its_predecessor_subtree (conformance of own_fin)";
const SUCCESSOR_BOUND_CONFORMANCE: &str =
    "no_successor_starts_beside_its_predecessor_subtree (conformance of successor_bound)";
const SUCCESSOR_WAIT_STEP_CONFORMANCE: &str =
    "no_successor_starts_beside_its_predecessor_subtree (conformance of successor_wait_step)";
const VERDICT_FLOOR_CONFORMANCE: &str =
    "no_verdict_before_the_abort_that_could_end_the_occupant (conformance of verdict_floor)";

/// The longest deadline a ladder step may be given, in nanoseconds: the same
/// horizon the production cap uses.
const FAR_FUTURE_NS: u64 = 86_400 * 365 * 30 * 1_000_000_000;

fn m_after(base: u64, d: u64) -> u64 {
    base.saturating_add(d.min(FAR_FUTURE_NS))
}

fn dur_ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

// ---- the stop ladder -------------------------------------------------------

/// Where the model's ladder stands, with symbolic times in nanoseconds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MStep {
    Idle,
    Graceful(Option<u64>),
    KillGrace(u64),
    AbortGrace(u64),
    Spent,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MPolicy {
    Kill,
    Timeout(u64),
    Infinity,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MEvent {
    Order {
        policy: MPolicy,
        issued: u64,
        raised: bool,
    },
    Deadline,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MAction {
    Nothing,
    Raise(Reason),
    Abort,
    Verdict,
}

/// The model's ladder: an order arms it (raising the first signal unless it was
/// raised already, the clock starting at the raise), the graceful window ends
/// in a `Kill`, the kill grace in an abort, the abort wait in the verdict.
fn m_ladder(step: MStep, event: MEvent, now: u64, grace: u64) -> (MStep, MAction) {
    match (step, event) {
        (
            MStep::Idle,
            MEvent::Order {
                policy,
                issued,
                raised,
            },
        ) => {
            let (base, action) = if raised {
                (issued, MAction::Nothing)
            } else {
                let first = match policy {
                    MPolicy::Kill => Reason::Kill,
                    MPolicy::Timeout(_) | MPolicy::Infinity => Reason::ParentRequest,
                };
                (now, MAction::Raise(first))
            };
            let next = match policy {
                MPolicy::Kill => MStep::KillGrace(m_after(base, grace)),
                MPolicy::Timeout(w) => MStep::Graceful(Some(m_after(base, w))),
                MPolicy::Infinity => MStep::Graceful(None),
            };
            (next, action)
        }
        (MStep::Graceful(Some(_)), MEvent::Deadline) => (
            MStep::KillGrace(m_after(now, grace)),
            MAction::Raise(Reason::Kill),
        ),
        (MStep::KillGrace(_), MEvent::Deadline) => (
            MStep::AbortGrace(m_after(now, ABORT_SETTLE)),
            MAction::Abort,
        ),
        (MStep::AbortGrace(_), MEvent::Deadline) => (MStep::Spent, MAction::Verdict),
        (step, _) => (step, MAction::Nothing),
    }
}

/// Symbolic nanoseconds as instants of one run.
struct Clock {
    epoch: StdInstant,
}

impl Clock {
    fn new() -> Clock {
        Clock {
            epoch: StdInstant::now(),
        }
    }

    /// The production value of a model duration in nanoseconds: the largest
    /// model value stands for the largest production duration.
    fn duration(ns: u64) -> Duration {
        if ns == u64::MAX {
            Duration::MAX
        } else {
            Duration::from_nanos(ns)
        }
    }

    fn at(&self, ns: u64) -> StdInstant {
        self.epoch + Duration::from_nanos(ns)
    }

    fn off(&self, t: StdInstant) -> u64 {
        dur_ns(t.duration_since(self.epoch))
    }

    fn step(&self, s: MStep) -> LadderStep {
        match s {
            MStep::Idle => LadderStep::Idle,
            MStep::Graceful(d) => LadderStep::Graceful {
                deadline: d.map(|d| self.at(d)),
            },
            MStep::KillGrace(d) => LadderStep::KillGrace {
                deadline: self.at(d),
            },
            MStep::AbortGrace(d) => LadderStep::AbortGrace {
                deadline: self.at(d),
            },
            MStep::Spent => LadderStep::Spent,
        }
    }

    fn back(&self, s: LadderStep) -> MStep {
        match s {
            LadderStep::Idle => MStep::Idle,
            LadderStep::Graceful { deadline } => MStep::Graceful(deadline.map(|d| self.off(d))),
            LadderStep::KillGrace { deadline } => MStep::KillGrace(self.off(deadline)),
            LadderStep::AbortGrace { deadline } => MStep::AbortGrace(self.off(deadline)),
            LadderStep::Spent => MStep::Spent,
        }
    }

    fn shutdown(p: MPolicy) -> Shutdown {
        match p {
            MPolicy::Kill => Shutdown::Kill,
            MPolicy::Timeout(w) => Shutdown::Timeout(Clock::duration(w)),
            MPolicy::Infinity => Shutdown::Infinity,
        }
    }

    fn event(&self, e: MEvent) -> LadderEvent {
        match e {
            MEvent::Order {
                policy,
                issued,
                raised,
            } => LadderEvent::Order {
                policy: Clock::shutdown(policy),
                issued_at: self.at(issued),
                initial_raised: raised,
            },
            MEvent::Deadline => LadderEvent::DeadlineReached,
        }
    }

    fn action(a: LadderAction) -> MAction {
        match a {
            LadderAction::None => MAction::Nothing,
            LadderAction::Raise(Signal::Kill) => MAction::Raise(Reason::Kill),
            LadderAction::Raise(Signal::ParentRequest) => MAction::Raise(Reason::ParentRequest),
            LadderAction::Abort => MAction::Abort,
            LadderAction::WriteVerdict => MAction::Verdict,
        }
    }

    /// The production function applied to the image of the model's inputs.
    fn production(&self, step: MStep, event: MEvent, now: u64, grace: u64) -> (MStep, MAction) {
        let (next, action) = rules::ladder_step(
            self.step(step),
            self.event(event),
            self.at(now),
            Clock::duration(grace),
        );
        (self.back(next), Clock::action(action))
    }
}

fn m_policy(kid: &ChildCfg) -> MPolicy {
    match kid.policy {
        Policy::Kill => MPolicy::Kill,
        Policy::Timeout => MPolicy::Timeout(kid.window_ns),
        Policy::Infinity => MPolicy::Infinity,
    }
}

/// Site conformance of `ladder_step`: every order armed in the step, and every
/// ladder step the step took.
fn site_ladder(cfg: &Cfg, pre: &State, step: Step, ap: &Applied, out: &mut Vec<&'static str>) {
    let post = &ap.post;
    let clock = Clock::new();
    let (issued, now) = (1_000_000_000_u64, 3_000_000_000_u64);
    let mut bad = false;
    // An order armed in this step: the first signal and the position it starts.
    for (&t, cell) in &post.cells {
        let was = pre.cells.get(&t);
        let armed_now = cell.order.is_some_and(|o| o.raised)
            && !was.and_then(|c| c.order).is_some_and(|o| o.raised);
        if !armed_now || was.is_some_and(|c| c.ladder != Ladder::Off) {
            continue;
        }
        let kid = cfg.kid_of(cell.sup, cell.key);
        let policy = m_policy(kid);
        let event = MEvent::Order {
            policy,
            issued,
            raised: false,
        };
        let (next, action) = clock.production(MStep::Idle, event, now, kid.kg());
        let raised = ap.raised.iter().find(|(u, _)| *u == t).map(|(_, r)| *r);
        if raised.map(MAction::Raise) != Some(action) {
            bad = true;
        }
        let want = match cell.ladder {
            Ladder::KillGrace => MStep::KillGrace(m_after(now, kid.kg())),
            Ladder::Window => MStep::Graceful(Some(m_after(now, kid.window_ns))),
            Ladder::WindowInf => MStep::Graceful(None),
            _ => MStep::Idle,
        };
        if next != want {
            bad = true;
        }
        // The same order with its first signal already raised: the clock
        // started when the order was issued, and nothing is raised again.
        let event = MEvent::Order {
            policy,
            issued,
            raised: true,
        };
        let got = clock.production(MStep::Idle, event, now, kid.kg());
        if got != m_ladder(MStep::Idle, event, now, kid.kg()) || got.1 != MAction::Nothing {
            bad = true;
        }
    }
    // A ladder step taken in this step.
    match step {
        Step::WindowEnds(t) => {
            let kid = cfg.kid_of(pre.cell(t).sup, pre.cell(t).key);
            let deadline = m_after(now, kid.window_ns);
            let (next, action) = clock.production(
                MStep::Graceful(Some(deadline)),
                MEvent::Deadline,
                deadline,
                kid.kg(),
            );
            if action != MAction::Raise(Reason::Kill)
                || !ap.raised.contains(&(t, Reason::Kill))
                || next != MStep::KillGrace(m_after(deadline, kid.kg()))
                || post.cell(t).ladder != Ladder::KillGrace
            {
                bad = true;
            }
        }
        Step::GraceEnds(t) => {
            let kid = cfg.kid_of(pre.cell(t).sup, pre.cell(t).key);
            let deadline = m_after(now, kid.kg());
            let (next, action) = clock.production(
                MStep::KillGrace(deadline),
                MEvent::Deadline,
                deadline,
                kid.kg(),
            );
            let live_after = post.live(t);
            if action != MAction::Abort
                || !post.inc(t).aborted
                || next != MStep::AbortGrace(m_after(deadline, ABORT_SETTLE))
                || (live_after && post.cell(t).ladder != Ladder::AbortGrace)
            {
                bad = true;
            }
        }
        Step::AbortGraceEnds(t) => {
            let kid = cfg.kid_of(pre.cell(t).sup, pre.cell(t).key);
            let deadline = m_after(now, ABORT_SETTLE);
            let (next, action) = clock.production(
                MStep::AbortGrace(deadline),
                MEvent::Deadline,
                deadline,
                kid.kg(),
            );
            if action != MAction::Verdict
                || next != MStep::Spent
                || !post.cell(t).verdict
                || post.cell(t).ladder != Ladder::Spent
            {
                bad = true;
            }
        }
        _ => {}
    }
    if bad {
        out.push(LADDER_STEP_CONFORMANCE);
    }
}

/// Domain conformance of `ladder_step`: every position and event, with a
/// kill grace of zero, the default, a long one and an unbounded one.
fn domain_ladder() -> bool {
    let clock = Clock::new();
    let graces = [0, 120_000_000, 1_500_000_000, u64::MAX];
    let policies = [
        MPolicy::Kill,
        MPolicy::Timeout(0),
        MPolicy::Timeout(200_000_000),
        MPolicy::Timeout(u64::MAX),
        MPolicy::Infinity,
    ];
    let mut steps = vec![
        MStep::Idle,
        MStep::Graceful(None),
        MStep::Graceful(Some(5_000_000_000)),
        MStep::KillGrace(5_000_000_000),
        MStep::AbortGrace(5_000_000_000),
        MStep::Spent,
    ];
    steps.dedup();
    let mut events = vec![MEvent::Deadline];
    for policy in policies {
        for raised in [false, true] {
            events.push(MEvent::Order {
                policy,
                issued: 1_000_000_000,
                raised,
            });
        }
    }
    let now = 3_000_000_000;
    let mut ok = u64::MAX == dur_ns(Duration::MAX)
        && Duration::from_nanos(ABORT_SETTLE) == super::ABORT_SETTLE;
    for grace in graces {
        for &step in &steps {
            for &event in &events {
                // The graceful position holds its own deadline; the production
                // image of a deadline beyond the far horizon is checked against
                // the same capped arithmetic.
                let want = m_ladder(step, event, now, grace);
                let got = clock.production(step, event, now, grace);
                if got != want {
                    ok = false;
                }
            }
        }
    }
    ok
}

// ---- the manual stop calls -------------------------------------------------

fn shutdown_of(kid: &ChildCfg) -> Shutdown {
    Clock::shutdown(m_policy(kid))
}

fn img_life(life: Life) -> ChildLifecycle {
    match life {
        Life::Running(n) => ChildLifecycle::Running(u64::from(n)),
        Life::Stopping { n, kind } => ChildLifecycle::Stopping {
            incarnation: u64::from(n),
            kind: img_kind(kind),
        },
        Life::Restarting { prev, next } => ChildLifecycle::Restarting {
            incarnation: u64::from(prev),
            next: u64::from(next),
        },
        Life::Down { n, pend } => ChildLifecycle::Down {
            incarnation: u64::from(n),
            pending: img_pending(pend),
        },
    }
}

fn img_pending(pend: Pend) -> DownPending {
    match pend {
        Pend::Nothing => DownPending::None,
        Pend::Snapshot => DownPending::Snapshot,
        Pend::Evaluate => DownPending::Evaluate,
    }
}

fn img_kind(kind: Kind) -> ManualStop {
    match kind {
        Kind::Bounce => ManualStop::Bounce,
        Kind::Terminate => ManualStop::Terminate,
    }
}

fn img_membership(memb: Memb, overridden: bool) -> GroupMembership {
    match memb {
        Memb::Outside => GroupMembership::None,
        Memb::Awaited => GroupMembership::Awaited { overridden },
        Memb::Queued => GroupMembership::Queued,
        Memb::Released => GroupMembership::Released,
    }
}

fn answer_effect(a: ManualReply) -> Commit {
    Commit {
        to: None,
        effects: vec![CommitEffect::Answer(a)],
    }
}

/// The inputs of a cell of the manual table: the call, the ledger row, the
/// membership, and the facts the cells read.
#[derive(Clone, Copy, Debug)]
struct CellIn {
    op: CallOp,
    row: Row,
    /// The occupant's task runs and its runner failed.
    lost: bool,
    memb: Memb,
    /// The key is in the pending group's `overrides`.
    overridden: bool,
    restart: Restart,
    /// The supervisor is a pool.
    pool: bool,
    /// A chain of the pending group already restarted the key.
    restarted: bool,
    /// A group restart is pending and covers the key: every key under
    /// `OneForAll`, every key not before the first failed member under
    /// `RestForOne`.
    keep_scope: bool,
    /// The occupant's policy is `Infinity`.
    infinity: bool,
    /// The occupant's lane holds a request its `pre_stop` may veto.
    vetoable: bool,
    /// The one-more-ask record of the occupant's slot is set.
    asked: bool,
    /// The occupant's slot already holds a stop order.
    ordered: bool,
    /// The incarnation the call concerns.
    n: u64,
}

impl CellIn {
    fn permanent(&self) -> bool {
        self.restart == Restart::Permanent
    }

    fn prunable(&self) -> bool {
        self.pool || self.restart == Restart::Temporary
    }

    /// The occupant is a vetoing `Infinity` child.
    fn ask(&self) -> bool {
        self.infinity && self.vetoable
    }

    /// The occupant's task ended: nothing is asked of it.
    fn ended(&self) -> bool {
        matches!(
            self.row,
            Row::RunningEnded | Row::BounceEnded | Row::TerminateEnded
        )
    }

    /// The one more ask a join gives: shared with the group when `shared`.
    fn ask_effects(&self, shared: bool) -> Vec<CommitEffect> {
        if !self.ask() || self.ended() {
            Vec::new()
        } else if shared {
            if self.asked {
                Vec::new()
            } else {
                vec![CommitEffect::ReaskOnce]
            }
        } else {
            vec![CommitEffect::Reask]
        }
    }

    fn stopping(&self, kind: ManualStop) -> ChildLifecycle {
        ChildLifecycle::Stopping {
            incarnation: self.n,
            kind,
        }
    }
}

/// The model's table of the manual stop calls (3.1 and 3.2): `None` for a
/// cell that is unreachable.
fn m_manual_cell(i: CellIn) -> Option<Commit> {
    // A removed spec is in no group set: only the key's own row answers.
    if i.row == Row::NoSpec {
        return (i.memb == Memb::Outside && matches!(i.op, CallOp::Stop | CallOp::Terminate))
            .then(|| answer_effect(ManualReply::ChildNotFound));
    }
    match i.op {
        CallOp::Stop => m_stop_cell(i),
        CallOp::Terminate => m_terminate_cell(i),
        _ => None,
    }
}

/// 3.1: pending-group membership first, read with the key's restart type; a
/// `Transient` or `Temporary` key the check would decide takes 3.2's cell for
/// the same row and membership.
fn m_stop_cell(i: CellIn) -> Option<Commit> {
    let permanent = i.permanent();
    let busy = || Some(answer_effect(ManualReply::ChildRestarting));
    let absorbed = || Some(answer_effect(ManualReply::Absorbed));
    let as_terminate = || {
        m_terminate_cell(CellIn {
            op: CallOp::Terminate,
            ..i
        })
    };
    match i.memb {
        Memb::Awaited if !i.overridden => {
            if permanent {
                absorbed()
            } else {
                as_terminate()
            }
        }
        Memb::Queued => {
            if i.row == Row::Attempt {
                busy()
            } else if permanent {
                absorbed()
            } else {
                as_terminate()
            }
        }
        Memb::Awaited | Memb::Released => {
            if permanent {
                busy()
            } else {
                as_terminate()
            }
        }
        Memb::Outside if i.restarted => {
            if permanent {
                busy()
            } else {
                as_terminate()
            }
        }
        Memb::Outside if !permanent && i.keep_scope => as_terminate(),
        Memb::Outside => match i.row {
            Row::NoSpec => Some(answer_effect(ManualReply::ChildNotFound)),
            Row::RunningLive => Some(Commit {
                to: Some(i.stopping(ManualStop::Bounce)),
                effects: if i.lost {
                    vec![CommitEffect::WriteOrder, CommitEffect::Join]
                } else {
                    vec![
                        CommitEffect::RaiseFirst,
                        CommitEffect::WriteOrder,
                        CommitEffect::Join,
                    ]
                },
            }),
            Row::RunningEnded => Some(Commit {
                to: None,
                effects: vec![
                    CommitEffect::Classify,
                    CommitEffect::Answer(ManualReply::Ok),
                ],
            }),
            Row::BounceLive | Row::TerminateLive | Row::BounceEnded | Row::TerminateEnded => {
                let mut effects = i.ask_effects(false);
                effects.push(CommitEffect::Join);
                Some(Commit { to: None, effects })
            }
            Row::Attempt | Row::DownOwed => busy(),
            Row::DownPlain | Row::DownSnapshot => Some(answer_effect(ManualReply::Ok)),
        },
    }
}

/// 3.2: an attempt in flight first, then membership, then the state; every
/// `None` cell that answers `Ok` or leaves a caller waiting while the group is
/// pending also records the key (KEEP).
fn m_terminate_cell(i: CellIn) -> Option<Commit> {
    use CommitEffect::{
        Answer, Classify, Dismiss, Join, LeaveRestartOrder, RaiseFirst, RecordOverride,
        RewriteSnapshot, WriteOrder,
    };
    let busy = || Some(answer_effect(ManualReply::ChildRestarting));
    let done = |mut effects: Vec<CommitEffect>| {
        effects.push(Answer(ManualReply::Ok));
        Some(Commit { to: None, effects })
    };
    let waits = |to: Option<ChildLifecycle>, mut effects: Vec<CommitEffect>| {
        effects.push(Join);
        Some(Commit { to, effects })
    };
    let upgrade = i.stopping(ManualStop::Terminate);
    let bounce = matches!(i.row, Row::BounceLive | Row::BounceEnded);
    if i.row == Row::Attempt {
        return (i.memb != Memb::Released).then(busy).flatten();
    }
    match i.memb {
        Memb::Outside => {
            let keep = i.keep_scope;
            let rec = |kept: bool| {
                if keep && kept {
                    vec![RecordOverride]
                } else {
                    Vec::new()
                }
            };
            match i.row {
                Row::NoSpec | Row::Attempt => None,
                Row::RunningLive => {
                    let mut effects = if i.lost {
                        vec![WriteOrder]
                    } else {
                        vec![RaiseFirst, WriteOrder]
                    };
                    effects.extend(rec(true));
                    waits(Some(upgrade), effects)
                }
                Row::RunningEnded => {
                    let mut effects = vec![Classify];
                    effects.extend(rec(!i.prunable()));
                    done(effects)
                }
                Row::BounceLive | Row::TerminateLive | Row::BounceEnded | Row::TerminateEnded => {
                    let mut effects = i.ask_effects(keep);
                    effects.extend(rec(true));
                    waits(bounce.then_some(upgrade), effects)
                }
                Row::DownPlain | Row::DownSnapshot => done(rec(true)),
                Row::DownOwed => {
                    let mut effects = vec![Dismiss];
                    effects.extend(rec(!i.prunable()));
                    done(effects)
                }
            }
        }
        Memb::Awaited => {
            // The override: the member leaves the restart set and the call
            // waits for its end. A running member the queue has not reached
            // gets its order from the call; a member whose stop is in
            // progress (the queue's order, a caller's stop, an earlier
            // override's order) is joined, with the one more ask shared with
            // the group.
            let release = [RecordOverride, LeaveRestartOrder];
            let with = |tail: Vec<CommitEffect>| -> Vec<CommitEffect> {
                release.iter().copied().chain(tail).collect()
            };
            match i.row {
                Row::RunningLive if i.ordered => waits(None, with(i.ask_effects(true))),
                Row::RunningLive if i.lost => waits(None, with(vec![WriteOrder])),
                Row::RunningLive => waits(None, with(vec![RaiseFirst, WriteOrder])),
                Row::RunningEnded | Row::TerminateEnded => waits(None, with(Vec::new())),
                Row::TerminateLive => waits(None, with(i.ask_effects(true))),
                Row::BounceLive => waits(Some(upgrade), with(i.ask_effects(true))),
                Row::BounceEnded => waits(Some(upgrade), with(Vec::new())),
                _ => None,
            }
        }
        Memb::Queued => match i.row {
            Row::DownPlain => done(vec![RecordOverride, LeaveRestartOrder]),
            Row::DownSnapshot => done(vec![RecordOverride, LeaveRestartOrder, RewriteSnapshot]),
            Row::DownOwed => done(vec![Dismiss, RecordOverride, LeaveRestartOrder]),
            _ => None,
        },
        Memb::Released => match i.row {
            Row::TerminateLive => {
                let effects = i.ask_effects(true);
                waits(None, effects)
            }
            Row::TerminateEnded => waits(None, Vec::new()),
            Row::DownPlain | Row::DownSnapshot => Some(answer_effect(ManualReply::Ok)),
            _ => None,
        },
    }
}

/// The decision the model took at a manual call, read from the effects it
/// prescribed for the call (the independent table of the call checks).
fn img_from_exp(cfg: &Cfg, c: Key, row: Row, memb: Memb, exp: &Exp) -> Option<Commit> {
    if exp.effect_free() {
        let reply = match exp.answer? {
            Answer::Ok => ManualReply::Ok,
            Answer::ChildNotFound => ManualReply::ChildNotFound,
            Answer::ChildRestarting => ManualReply::ChildRestarting,
            Answer::Absorbed => ManualReply::Absorbed,
            // A join on an occupant that carries the verdict is answered at
            // once, with no effect of its own: the wait on the occupant's
            // cell returns the verdict at its first poll.
            Answer::ChildUnresponsive if matches!(row, Row::BounceLive | Row::TerminateLive) => {
                return Some(Commit {
                    to: None,
                    effects: vec![CommitEffect::Join],
                });
            }
            _ => return None,
        };
        return Some(answer_effect(reply));
    }
    let first = cfg.first_signal(c);
    if let Some((n, kind)) = exp.commit {
        let mut effects = Vec::new();
        if exp.raises.contains(&(n, first)) {
            effects.push(CommitEffect::RaiseFirst);
        }
        if exp.order_written == Some(n) {
            effects.push(CommitEffect::WriteOrder);
        }
        if exp.overrides_added {
            effects.push(CommitEffect::RecordOverride);
        }
        effects.push(CommitEffect::Join);
        return Some(Commit {
            to: Some(ChildLifecycle::Stopping {
                incarnation: u64::from(n),
                kind: img_kind(kind),
            }),
            effects,
        });
    }
    // A running child that ended before the call: classified at once.
    if row == Row::RunningEnded && memb == Memb::Outside && exp.classified.is_some() {
        let mut effects = vec![CommitEffect::Classify];
        if exp.overrides_added {
            effects.push(CommitEffect::RecordOverride);
        }
        effects.push(CommitEffect::Answer(ManualReply::Ok));
        return Some(Commit { to: None, effects });
    }
    let to = exp.upgrade.map(|n| ChildLifecycle::Stopping {
        incarnation: u64::from(n),
        kind: ManualStop::Terminate,
    });
    // A member whose end the group awaits: the override and the wait, the
    // order a member the queue has not reached gets from the call, or the one
    // more ask a member whose stop is in progress gets.
    if exp.overrides_added && memb == Memb::Awaited {
        let mut effects = vec![CommitEffect::RecordOverride];
        if exp.leaves_restart_set {
            effects.push(CommitEffect::LeaveRestartOrder);
        }
        match exp.order_written {
            Some(n) => {
                match exp.raises.as_slice() {
                    [] => {}
                    [(t, reason)] if *t == n && *reason == first => {
                        effects.push(CommitEffect::RaiseFirst);
                    }
                    _ => return None,
                }
                effects.push(CommitEffect::WriteOrder);
            }
            None => match exp.raises.as_slice() {
                [] => {}
                [(_, Reason::ParentRequest)] => effects.push(if exp.reasked.is_some() {
                    CommitEffect::ReaskOnce
                } else {
                    CommitEffect::Reask
                }),
                _ => return None,
            },
        }
        effects.push(CommitEffect::Join);
        return Some(Commit { to, effects });
    }
    // A child that is down: the owed decision dismissed, the key recorded, the
    // member taken out of the restart set, a pending restart snapshot
    // rewritten.
    if matches!(row, Row::DownPlain | Row::DownSnapshot | Row::DownOwed) && exp.answer.is_some() {
        let reply = match exp.answer? {
            Answer::Ok => ManualReply::Ok,
            _ => return None,
        };
        let mut effects = Vec::new();
        if exp.dismissed.is_some() {
            effects.push(CommitEffect::Dismiss);
        }
        if exp.overrides_added {
            effects.push(CommitEffect::RecordOverride);
        }
        if exp.leaves_restart_set {
            effects.push(CommitEffect::LeaveRestartOrder);
        }
        if memb == Memb::Queued && row == Row::DownSnapshot {
            effects.push(CommitEffect::RewriteSnapshot);
        }
        effects.push(CommitEffect::Answer(reply));
        return Some(Commit { to: None, effects });
    }
    // A join: no order, at most one request raised on the joined child, the
    // key recorded when the call keeps it down.
    if exp.order_written.is_none() {
        let mut effects = Vec::new();
        match exp.raises.as_slice() {
            [] => {}
            [(_, Reason::ParentRequest)] => effects.push(if exp.reasked.is_some() {
                CommitEffect::ReaskOnce
            } else {
                CommitEffect::Reask
            }),
            _ => return None,
        }
        if exp.overrides_added {
            effects.push(CommitEffect::RecordOverride);
        }
        effects.push(CommitEffect::Join);
        return Some(Commit { to, effects });
    }
    None
}

fn img_task(pre: &State, row: Row, n: Tok) -> TaskState {
    match row {
        Row::RunningEnded | Row::BounceEnded | Row::TerminateEnded => TaskState::Ended,
        Row::RunningLive | Row::BounceLive | Row::TerminateLive => {
            if matches!(
                pre.cells.get(&n).map(|c| c.exec),
                Some(Exec::RunnerFailed | Exec::Orphan)
            ) {
                TaskState::RunnerLost
            } else {
                TaskState::Live
            }
        }
        _ => TaskState::Ended,
    }
}

/// The ledger entry a row names, over the token `n`.
fn row_life(row: Row, n: u64) -> Option<ChildLifecycle> {
    match row {
        Row::NoSpec => None,
        Row::RunningLive | Row::RunningEnded => Some(ChildLifecycle::Running(n)),
        Row::BounceLive | Row::BounceEnded => Some(ChildLifecycle::Stopping {
            incarnation: n,
            kind: ManualStop::Bounce,
        }),
        Row::TerminateLive | Row::TerminateEnded => Some(ChildLifecycle::Stopping {
            incarnation: n,
            kind: ManualStop::Terminate,
        }),
        Row::Attempt => Some(ChildLifecycle::Restarting {
            incarnation: n,
            next: n + 1,
        }),
        Row::DownPlain => Some(ChildLifecycle::Down {
            incarnation: n,
            pending: DownPending::None,
        }),
        Row::DownSnapshot => Some(ChildLifecycle::Down {
            incarnation: n,
            pending: DownPending::Snapshot,
        }),
        Row::DownOwed => Some(ChildLifecycle::Down {
            incarnation: n,
            pending: DownPending::Evaluate,
        }),
    }
}

/// The production image of the facts of a manual call.
fn img_facts(i: &CellIn, task: TaskState, policy: Shutdown) -> rules::ManualFacts {
    rules::ManualFacts {
        life: row_life(i.row, i.n),
        task,
        membership: img_membership(i.memb, i.overridden),
        restart_type: img_restart(i.restart),
        pool: i.pool,
        restarted: i.restarted,
        in_keep_scope: i.keep_scope,
        policy,
        lane_vetoable: i.vetoable,
        reasked: i.asked,
        has_order: i.ordered,
    }
}

/// Whether a group restart is pending and covers the key, read from the
/// model's state: every key under `OneForAll`, every key not before the
/// group's first failed member under `RestForOne`.
fn state_keep_scope(pre: &State, c: Key) -> bool {
    pre.sup.group.as_ref().is_some_and(|g| match g.kind {
        GKind::All => true,
        GKind::Rest => !g.outside.contains(&c),
    })
}

/// The cell inputs a manual call reads from the state before it.
fn site_cell(cfg: &Cfg, pre: &State, op: CallOp, c: Key) -> (CellIn, Row, Option<Life>) {
    let row = pre.o_row(c);
    let memb = pre.o_membership(cfg, c);
    let group = pre.sup.group.as_ref();
    let life = pre.life(c);
    let n = match life {
        Some(Life::Running(n) | Life::Stopping { n, .. } | Life::Down { n, .. }) => n,
        Some(Life::Restarting { prev, .. }) => prev,
        None => 0,
    };
    let kid = cfg.kid(c);
    let vetoable_lane = pre.incs.get(&n).is_some_and(|i| vetoable(i.lane));
    let task = img_task(pre, row, n);
    let cell = CellIn {
        op,
        row,
        lost: task == TaskState::RunnerLost,
        memb,
        overridden: group.is_some_and(|g| g.overrides.contains(&c)),
        restart: kid.restart,
        pool: cfg.is_pool(),
        restarted: group.is_some_and(|g| g.restarted.contains(&c)),
        keep_scope: state_keep_scope(pre, c),
        infinity: kid.policy == Policy::Infinity,
        vetoable: vetoable_lane,
        asked: pre.incs.get(&n).is_some_and(|i| i.reasked),
        ordered: pre.cells.get(&n).is_some_and(|c| c.order.is_some()),
        n: u64::from(n),
    };
    (cell, row, life)
}

/// Site conformance of `manual_commit`: every `stop_child` and
/// `terminate_child` the model takes.
fn site_manual(cfg: &Cfg, pre: &State, step: Step, out: &mut Vec<&'static str>) {
    let (op, c) = match step {
        Step::StopChild(c) => (CallOp::Stop, c),
        Step::TerminateChild(c) => (CallOp::Terminate, c),
        _ => return,
    };
    let (cell, row, life) = site_cell(cfg, pre, op, c);
    let memb = cell.memb;
    let Some(table) = m_manual_cell(cell) else {
        return;
    };
    let exp = match op {
        CallOp::Stop => exp_stop(cfg, pre, c, row, memb),
        _ => exp_terminate(cfg, pre, c, row, memb),
    };
    let kind = match op {
        CallOp::Stop => ManualStop::Bounce,
        _ => ManualStop::Terminate,
    };
    let kid = cfg.kid(c);
    let task = img_task(pre, row, Tok::try_from(cell.n).unwrap_or_default());
    let facts = rules::ManualFacts {
        life: life.map(img_life),
        ..img_facts(&cell, task, shutdown_of(kid))
    };
    MANUAL_SITES.with(|n| n.set(n.get() + 1));
    let got = rules::manual_commit(kind, &facts);
    let want = img_from_exp(cfg, c, row, memb, &exp);
    if want.as_ref() != Some(&got) || table != got {
        out.push(MANUAL_COMMIT_CONFORMANCE);
    }
}

std::thread_local! {
    /// The `stop_child` and `terminate_child` calls the commit check compared.
    static MANUAL_SITES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// The calls the answer check compared.
    static ANSWER_SITES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// The keys the membership check compared.
    static MEMBERSHIP_SITES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// The keys the membership check compared whose override was recorded at
    /// an incarnation other than the one the group's plan holds for the key:
    /// a member a chain restarted and a terminate then released.
    static RELEASED_AFTER_RESTART: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// The same, for a child started since the group restart began, which
    /// the plan holds no incarnation for.
    static RELEASED_SPAWNED_SINCE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Every cell input of the manual domain sweeps, once per process: the four
/// calls over every row and membership, the restart types, `restarted`, the
/// key's place in KEEP's scope, the request state of an `Infinity` child and
/// the one-more-ask record.
fn domain_cells() -> &'static [CellIn] {
    static CELLS: std::sync::OnceLock<Vec<CellIn>> = std::sync::OnceLock::new();
    CELLS.get_or_init(|| {
        let rows = [
            Row::NoSpec,
            Row::RunningLive,
            Row::RunningEnded,
            Row::BounceLive,
            Row::BounceEnded,
            Row::TerminateLive,
            Row::TerminateEnded,
            Row::Attempt,
            Row::DownPlain,
            Row::DownSnapshot,
            Row::DownOwed,
        ];
        let mut cells = Vec::new();
        for op in [
            CallOp::Stop,
            CallOp::Terminate,
            CallOp::Restart,
            CallOp::Delete,
        ] {
            for row in rows {
                for memb in [Memb::Outside, Memb::Awaited, Memb::Queued, Memb::Released] {
                    for overridden in [false, true] {
                        for restart in [Restart::Permanent, Restart::Transient, Restart::Temporary]
                        {
                            for restarted in [false, true] {
                                for keep_scope in [false, true] {
                                    for (infinity, vetoable) in
                                        [(false, false), (true, false), (false, true), (true, true)]
                                    {
                                        for asked in [false, true] {
                                            for lost in [false, true] {
                                                for ordered in [false, true] {
                                                    let live = matches!(
                                                        row,
                                                        Row::RunningLive
                                                            | Row::BounceLive
                                                            | Row::TerminateLive
                                                    );
                                                    if lost && !live {
                                                        continue;
                                                    }
                                                    // Whether the slot holds an order
                                                    // decides only the override of a
                                                    // running member.
                                                    if ordered
                                                        && !(memb == Memb::Awaited
                                                            && row == Row::RunningLive)
                                                    {
                                                        continue;
                                                    }
                                                    cells.push(CellIn {
                                                        op,
                                                        row,
                                                        lost,
                                                        memb,
                                                        overridden,
                                                        restart,
                                                        pool: false,
                                                        restarted,
                                                        keep_scope,
                                                        infinity,
                                                        vetoable,
                                                        asked,
                                                        ordered,
                                                        n: 7,
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // A pool prunes every stopped child, whatever its restart type; it has
        // no group, so only the cells with no pending group differ.
        for op in [CallOp::Stop, CallOp::Terminate] {
            for row in rows {
                for restart in [Restart::Permanent, Restart::Temporary] {
                    for lost in [false, true] {
                        let live =
                            matches!(row, Row::RunningLive | Row::BounceLive | Row::TerminateLive);
                        if lost && !live {
                            continue;
                        }
                        cells.push(CellIn {
                            op,
                            row,
                            lost,
                            memb: Memb::Outside,
                            overridden: false,
                            restart,
                            pool: true,
                            restarted: false,
                            keep_scope: false,
                            infinity: false,
                            vetoable: false,
                            asked: false,
                            ordered: false,
                            n: 7,
                        });
                    }
                }
            }
        }
        cells
    })
}

/// The task state a domain cell names.
fn domain_task(i: &CellIn) -> TaskState {
    match i.row {
        Row::RunningEnded | Row::BounceEnded | Row::TerminateEnded => TaskState::Ended,
        _ if i.lost => TaskState::RunnerLost,
        Row::RunningLive | Row::BounceLive | Row::TerminateLive => TaskState::Live,
        _ => TaskState::Ended,
    }
}

/// The policy a domain cell names.
fn domain_policy(i: &CellIn) -> Shutdown {
    if i.infinity {
        Shutdown::Infinity
    } else {
        Shutdown::Timeout(Duration::from_millis(50))
    }
}

/// Domain conformance of `manual_commit`: every cell of the model's table that
/// production implements, under every restart type, `restarted`, KEEP scope,
/// policy and request state.
fn domain_manual() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        let mut ok = true;
        for i in domain_cells() {
            if !matches!(i.op, CallOp::Stop | CallOp::Terminate) {
                continue;
            }
            let Some(want) = m_manual_cell(*i) else {
                continue;
            };
            let facts = img_facts(i, domain_task(i), domain_policy(i));
            let kind = match i.op {
                CallOp::Stop => ManualStop::Bounce,
                _ => ManualStop::Terminate,
            };
            if rules::manual_commit(kind, &facts) != want {
                ok = false;
            }
        }
        ok
    })
}

// ---- the child ledger, reports and the group's decisions ---------------------

/// What the chain does with its front member, as the model decided it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FrontKind {
    Fresh,
    Initiate,
    StopThenInitiate,
    Hold,
}

/// How a failed restart attempt was routed, as the model decided it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RouteKind {
    ChainRetry,
    Member,
    Removed,
    Evaluate,
}

/// A decision the model took in a step that production takes with a `rules`
/// function: the conformance checks replay it with the production image of
/// its inputs.
#[derive(Clone, Debug)]
enum Obs {
    /// A move between two ledger entries, and whether the model's table
    /// allowed it.
    Transition { from: Life, to: Life, legal: bool },
    /// The class the model gave a drained report.
    Report {
        life: Option<Life>,
        t: Tok,
        class: ReportClass,
    },
    /// What the chain did with its front member.
    Front {
        life: Life,
        started: Option<Tok>,
        action: FrontKind,
    },
    /// The decision of a manual completion.
    Classify {
        kind: Kind,
        reason: Reason,
        restart: Restart,
        pool: bool,
        in_restart_set: bool,
        action: Option<Action>,
        pending: Pend,
        remove: bool,
        initiate: bool,
    },
    /// How a failed attempt was routed.
    Route {
        chain_front_own: bool,
        memb: Memb,
        origin: Origin,
        route: RouteKind,
    },
    /// A decision of a restart attempt's executor.
    Attempt {
        stage: AtStage,
        event: AtEvent,
        ladder: Ladder,
        kill_stored: bool,
        acked: bool,
        want: AtWant,
    },
    /// What a group evaluation or a retry committed: the keys of the ledger
    /// in start order with what the evaluation read of each, the failed key,
    /// and the barrier and restart order the model built. `retry`: the
    /// chain's own start failed, and `overrides` are the keys a
    /// `terminate_child` released from the group.
    Commit {
        failed: Key,
        rest_for_one: bool,
        members: Vec<CommitInput>,
        awaiting: BTreeMap<Key, Tok>,
        restart_order: Vec<Key>,
        retry: bool,
        overrides: Vec<Key>,
    },
    /// A member's end in a group: its restart type and whether the model
    /// removed its spec and reported it removed.
    MemberEnd { restart: Restart, prune: bool },
    /// What the teardown did about a key when its turn came, in a pool or
    /// not.
    Teardown {
        input: WalkInput,
        want: TdWant,
        pool: bool,
    },
    /// What a pool's teardown did about every key in the one step that
    /// stops them all, newest first.
    PoolWalk {
        inputs: Vec<WalkInput>,
        wants: Vec<TdWant>,
    },
    /// What the stop queue did about its head: what the model read and did,
    /// what production's `queue_head` reads of the production image of the
    /// same state, and whether the model's queue marks the head reached
    /// after the step.
    Queue {
        input: QueueInput,
        want: QueueWant,
        prod: QueueHead,
        reached: bool,
    },
    /// What a group evaluation did with the owed evaluation of a member.
    Absorb {
        place: AbPlace,
        in_plan: bool,
        life: Option<Life>,
        want: AbWant,
    },
    /// The barrier the supervisor checked for the verdict, in reverse start
    /// order with each entry's flag, and the key the model's exit names.
    Verdict {
        entries: Vec<(Key, bool)>,
        exit: Option<Key>,
    },
    /// A request raised on a lane: the lane before it, how the model's lane
    /// answered, and the lane after.
    Lane {
        stored: Option<Reason>,
        new: Reason,
        mode: LaneMode,
        receiver_gone: bool,
        answer: LaneAnswer,
        after: Option<Reason>,
    },
    /// A start hook's read of the lane, and whether the model skips the
    /// remaining start hooks on it.
    InitRead {
        lane: Option<Reason>,
        kill_now: bool,
    },
    /// The budget a child's creation added to the subtree it hangs from: the
    /// child's shape, policy and kill grace, and the model's two values.
    Budget {
        is_supervisor: bool,
        policy: Policy,
        window_ns: u64,
        kill_grace_ns: u64,
        own_fin: u64,
        own_budget: u64,
    },
    /// What a link guard did to an incarnation it fired on: the child's
    /// shape, the reason it raised and the reaper entry it left.
    Guard {
        is_supervisor: bool,
        policy: Policy,
        window_ns: u64,
        kill_grace_ns: u64,
        raised: Reason,
        entry: Option<Reap>,
        /// The lane held a `Kill` when the guard fired.
        kill_stored: bool,
    },
}

/// How the model's lane answers a request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LaneAnswer {
    Recorded,
    Superseded,
    Refused,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AtStage {
    Factory,
    Init,
    InTransit,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AtEvent {
    Begin { ordered: bool },
    Order,
    Ack { ordered: bool, ended: bool },
    InitEnded,
    StartTimeout,
    LadderSpent,
    Ended { ordered: bool, timeout_raised: bool },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AtReport {
    Stopped,
    StartTimeout,
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AtWant {
    Nothing,
    Cancel,
    Spawn,
    ArmLadder,
    Adopt,
    KillPath { raise: bool },
    WriteVerdict,
    Report(AtReport),
}

/// What a group evaluation read of one key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct CommitInput {
    key: Key,
    life: Life,
    restart: Restart,
}

/// What the teardown read of a key when its turn came.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct WalkInput {
    life: Option<Life>,
    has_order: bool,
    ask_possible: bool,
    reasked: bool,
    past_init: bool,
}

/// What the stop queue read of its head.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct QueueInput {
    awaited: bool,
    verdict: bool,
    signalled: bool,
    handled: bool,
    /// The head's ledger entry when it holds the awaited incarnation.
    life: Option<Life>,
    has_order: bool,
    ask_possible: bool,
    reasked: bool,
    past_init: bool,
    /// The head's restart attempt published its lane: its incarnation exists.
    spawned: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum QueueWant {
    Pop,
    Wait,
    Order,
    OrderAttempt { ask: bool },
    OrderUnpublished,
    Await,
    Join { ask: bool },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TdWant {
    Skip,
    Order,
    Join { ask: bool },
    OrderAttempt { ask: bool },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AbPlace {
    Queued,
    Worklist,
    DeathPlane,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AbWant {
    Keep,
    Deliver,
    Snapshot,
}

fn img_pending_back(pending: DownPending) -> Pend {
    match pending {
        DownPending::None => Pend::Nothing,
        DownPending::Snapshot => Pend::Snapshot,
        DownPending::Evaluate => Pend::Evaluate,
    }
}

fn img_class(class: ReportClass) -> rules::ReportClass {
    match class {
        ReportClass::Occupant => rules::ReportClass::Occupant,
        ReportClass::OwedEvaluation => rules::ReportClass::OwedEvaluation,
        ReportClass::Stale => rules::ReportClass::Stale,
    }
}

fn img_reason(reason: Reason) -> StopReason {
    match reason {
        Reason::Graceful => StopReason::Graceful,
        Reason::ParentRequest => StopReason::ParentRequest,
        Reason::Failure(_) => StopReason::Failure(crate::error::ActorError::user("model failure")),
        Reason::ParentKilled => StopReason::ParentKilled,
        Reason::Cancelled => StopReason::Cancelled,
        Reason::Kill => StopReason::Kill,
    }
}

fn img_restart(restart: Restart) -> RestartType {
    match restart {
        Restart::Permanent => RestartType::Permanent,
        Restart::Transient => RestartType::Transient,
        Restart::Temporary => RestartType::Temporary,
    }
}

fn img_origin(origin: Origin) -> ProdOrigin {
    match origin {
        Origin::Initial => ProdOrigin::Initial,
        Origin::Strategy => ProdOrigin::Strategy,
        Origin::Bounce => ProdOrigin::Bounce,
        Origin::Manual => ProdOrigin::Manual,
        Origin::Chain => ProdOrigin::Chain,
    }
}

fn img_action(action: Action) -> SupervisionAction {
    match action {
        Action::RestartInitiated => SupervisionAction::RestartInitiated,
        Action::Removed => SupervisionAction::Removed,
        Action::BudgetExhausted => SupervisionAction::BudgetExhausted,
        Action::SupervisorStopping => SupervisionAction::SupervisorStopping,
    }
}

fn img_front(action: FrontKind) -> rules::FrontAction {
    match action {
        FrontKind::Fresh => rules::FrontAction::Fresh,
        FrontKind::Initiate => rules::FrontAction::Initiate,
        FrontKind::StopThenInitiate => rules::FrontAction::StopThenInitiate,
        FrontKind::Hold => rules::FrontAction::Hold,
    }
}

fn img_route(route: RouteKind) -> rules::Route {
    match route {
        RouteKind::ChainRetry => rules::Route::ChainRetry,
        RouteKind::Member => rules::Route::Member,
        RouteKind::Removed => rules::Route::Removed,
        RouteKind::Evaluate => rules::Route::Evaluate,
    }
}

fn img_reply(answer: Answer) -> Option<ManualReply> {
    match answer {
        Answer::Ok => Some(ManualReply::Ok),
        Answer::ChildNotFound => Some(ManualReply::ChildNotFound),
        Answer::ChildRestarting => Some(ManualReply::ChildRestarting),
        Answer::ChildRunning => Some(ManualReply::ChildRunning),
        Answer::Absorbed => Some(ManualReply::Absorbed),
        _ => None,
    }
}

/// The incarnation a ledger entry names for a barrier or an override: the one
/// the entry holds or last held.
fn held_token(life: Life) -> Tok {
    match life {
        Life::Running(n) | Life::Stopping { n, .. } | Life::Down { n, .. } => n,
        Life::Restarting { prev, .. } => prev,
    }
}

/// Replays the observed decisions of one step.
fn site_observed(ap: &Applied, out: &mut Vec<&'static str>) {
    for obs in &ap.obs {
        match obs {
            Obs::Transition { from, to, legal } => {
                if rules::is_legal(&img_life(*from), &img_life(*to)) != *legal {
                    out.push(IS_LEGAL_CONFORMANCE);
                }
            }
            Obs::Report { life, t, class } => {
                if let Some(life) = life {
                    if rules::report_class(img_life(*life), u64::from(*t)) != img_class(*class) {
                        out.push(REPORT_CLASS_CONFORMANCE);
                    }
                }
            }
            Obs::Front {
                life,
                started,
                action,
            } => {
                let got = rules::chain_front(img_life(*life), started.map(u64::from));
                if got != img_front(*action) {
                    out.push(CHAIN_FRONT_CONFORMANCE);
                }
            }
            Obs::Classify {
                kind,
                reason,
                restart,
                pool,
                in_restart_set,
                action,
                pending,
                remove,
                initiate,
            } => {
                let got = rules::classify_manual(
                    img_kind(*kind),
                    &img_reason(*reason),
                    img_restart(*restart),
                    *pool,
                    *in_restart_set,
                );
                let same_end =
                    got.remove == *remove && (got.remove || got.pending == img_pending(*pending));
                if got.action != action.map(img_action) || !same_end || got.initiate != *initiate {
                    out.push(CLASSIFY_MANUAL_CONFORMANCE);
                }
            }
            Obs::Route {
                chain_front_own,
                memb,
                origin,
                route,
            } => {
                let got = rules::attempt_failure_route(
                    *chain_front_own,
                    img_membership(*memb, false),
                    img_origin(*origin),
                );
                if got != img_route(*route) {
                    out.push(ATTEMPT_ROUTE_CONFORMANCE);
                }
            }
            Obs::Attempt {
                stage,
                event,
                ladder,
                kill_stored,
                acked,
                want,
            } => {
                let view = AttemptView {
                    ladder: img_ladder(*ladder),
                    kill_stored: *kill_stored,
                    acked: *acked,
                };
                let got = rules::attempt_step(img_at_stage(*stage), img_at_event(*event), view);
                if got != img_at_want(*want) {
                    out.push(ATTEMPT_STEP_CONFORMANCE);
                }
                if m_attempt_step(*stage, *event, *ladder, *kill_stored, *acked) != *want {
                    out.push(MODEL_TABLE_CONFORMANCE);
                }
            }
            Obs::Commit {
                failed,
                rest_for_one,
                members,
                awaiting,
                restart_order,
                retry,
                overrides,
            } => site_commit(
                *failed,
                *rest_for_one,
                members,
                awaiting,
                restart_order,
                *retry,
                overrides,
                out,
            ),
            Obs::MemberEnd { restart, prune } => {
                let end = rules::group_member_end(img_restart(*restart));
                if (end == rules::MemberEnd::PruneAndRemove) != *prune {
                    out.push(GROUP_MEMBER_END_CONFORMANCE);
                }
            }
            Obs::Teardown { input, want, pool } => {
                let child = img_walk(input);
                let strategy = if *pool {
                    RestartStrategy::SimpleOneForOne
                } else {
                    RestartStrategy::OneForOne
                };
                let walked = rules::teardown_walk(strategy, &[(ActorId::from("child"), child)]);
                if walked.len() != 1 || walked[0].1 != img_td(*want) {
                    out.push(TEARDOWN_WALK_CONFORMANCE);
                }
                if m_teardown(input) != *want {
                    out.push(MODEL_TABLE_CONFORMANCE);
                }
            }
            Obs::PoolWalk { inputs, wants } => {
                POOL_SITES.with(|c| c.set(c.get() + 1));
                let id = |i: usize| ActorId::from(format!("child-{i}"));
                let children: Vec<(ActorId, WalkChild)> = inputs
                    .iter()
                    .enumerate()
                    .map(|(i, input)| (id(i), img_walk(input)))
                    .collect();
                let walked = rules::teardown_walk(RestartStrategy::SimpleOneForOne, &children);
                let mut bad = walked.len() != wants.len();
                // Newest first: the walk visits the keys in reverse.
                for (entry, (i, want)) in walked.iter().zip(wants.iter().enumerate().rev()) {
                    bad |= entry.0 != id(i) || entry.1 != img_td(*want);
                }
                if bad {
                    out.push(TEARDOWN_WALK_CONFORMANCE);
                }
            }
            Obs::Queue {
                input,
                want,
                prod,
                reached,
            } => {
                QUEUE_SITES.with(|c| c.set(c.get() + 1));
                let mut model = img_queue(input);
                // A verdict the key moved past is unknown to production.
                model.verdict = model.verdict && model.life.is_some();
                let step = rules::stop_queue_next(&model);
                if step != img_queue_step(*want)
                    || m_stop_queue_next(input) != *want
                    || !same_head(&model, prod)
                    || rules::stop_queue_next(prod) != step
                    || *want != QueueWant::Pop
                        && rules::queue_head_reached_after(step, input.signalled) != *reached
                {
                    eprintln!(
                        "stop queue step differs: model reads {model:?}, production reads {prod:?}, \
                         model did {want:?}, function says {step:?}, model reached {reached}"
                    );
                    out.push(STOP_QUEUE_NEXT_CONFORMANCE);
                }
            }
            Obs::Absorb {
                place,
                in_plan,
                life,
                want,
            } => {
                let got = rules::absorbable(img_place(*place), *in_plan, life.map(img_life));
                if got != img_ab(*want) {
                    out.push(ABSORBABLE_CONFORMANCE);
                }
            }
            Obs::Verdict { entries, exit } => {
                let image: Vec<(ActorId, bool)> = entries
                    .iter()
                    .map(|(k, v)| (ActorId::from(format!("child-{k}")), *v))
                    .collect();
                let got = rules::barrier_blocked_only_by_verdicts(&image);
                if got != exit.map(|k| ActorId::from(format!("child-{k}"))) {
                    out.push(BARRIER_VERDICT_CONFORMANCE);
                }
            }
            Obs::Lane {
                stored,
                new,
                mode,
                receiver_gone,
                answer,
                after,
            } => {
                LANE_SITES.with(|c| c.set(c.get() + 1));
                let (got, store, bump) = prod_lane(*stored, *new, *mode, *receiver_gone);
                let want = match answer {
                    LaneAnswer::Recorded => RaiseResult::Recorded,
                    LaneAnswer::Superseded => RaiseResult::Superseded,
                    LaneAnswer::Refused => RaiseResult::Refused,
                };
                let kept = if store { Some(*new) } else { *stored };
                if got != want || bump != (want == RaiseResult::Recorded) || kept != *after {
                    out.push(LANE_ACCEPT_CONFORMANCE);
                }
            }
            Obs::Budget {
                is_supervisor,
                policy,
                window_ns,
                kill_grace_ns,
                own_fin,
                own_budget,
            } => {
                BUDGET_SITES.with(|c| c.set(c.get() + 1));
                let shutdown = Clock::shutdown(match policy {
                    Policy::Kill => MPolicy::Kill,
                    Policy::Timeout => MPolicy::Timeout(*window_ns),
                    Policy::Infinity => MPolicy::Infinity,
                });
                let grace = img_grace(*kill_grace_ns);
                if rules::saturating_nanos(rules::own_fin(*is_supervisor, shutdown, grace))
                    != *own_fin
                {
                    out.push(OWN_FIN_CONFORMANCE);
                }
                if opt_ns(rules::own_budget(*is_supervisor, shutdown, grace)) != *own_budget {
                    out.push(OWN_BUDGET_CONFORMANCE);
                }
            }
            Obs::InitRead { lane, kill_now } => {
                INIT_SITES.with(|c| c.set(c.get() + 1));
                let image = lane.map(img_reason);
                let step = rules::init_kill_check(image.as_ref());
                if (step == rules::InitStep::KillNow) != *kill_now {
                    out.push(INIT_KILL_CHECK_CONFORMANCE);
                }
            }
            Obs::Guard {
                is_supervisor,
                policy,
                window_ns,
                kill_grace_ns,
                raised,
                entry,
                kill_stored,
            } => {
                GUARD_SITES.with(|c| c.set(c.get() + 1));
                let shutdown = Clock::shutdown(m_policy_of(*policy, *window_ns));
                let got = rules::fired_stop(
                    rules::inherited_stop(
                        *is_supervisor,
                        shutdown,
                        Clock::duration(*kill_grace_ns),
                    ),
                    *kill_stored,
                );
                if Some(got) != img_guard(*policy, *window_ns, *kill_grace_ns, *raised, *entry) {
                    out.push(INHERITED_STOP_CONFORMANCE);
                }
            }
        }
    }
}

thread_local! {
    /// The queue steps the stop queue check compared with the production
    /// function in this search, and the pool teardown steps whose every key
    /// the teardown check compared.
    static QUEUE_SITES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static POOL_SITES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

thread_local! {
    /// The raises whose answer the lane check compared with the production
    /// function in this search, and the start reads the init check compared.
    static LANE_SITES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static INIT_SITES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// The creations whose budgets the two budget checks compared, the waits
    /// whose bound the bound check compared, and the wait steps the step
    /// check compared.
    static BUDGET_SITES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static BOUND_SITES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static WAIT_STEP_SITES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// The waits whose verdict the floor check compared while an abort was
    /// owed or issued.
    static FLOOR_SITES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The production floor read at an image instant: the wait's budget deadline
/// at `base`, its cap one second past it, an abort issued (when `aborted`) at
/// `base`, and the clock at `base` plus the settle time, past every deadline
/// but the cap. The model has no clock: a verdict it allows stands for one
/// given after every deadline that is due, and one held back for an owed
/// abort stands for a wait the cap has not reached.
fn img_verdict_allowed(aborted: bool, owed: bool) -> bool {
    let base = StdInstant::now();
    let settle = Duration::from_nanos(ABORT_SETTLE);
    rules::verdict_floor(base, Duration::from_secs(1), aborted.then_some(base), owed)
        <= base + settle
}

/// Site conformance of `verdict_floor`: at every state where the model
/// holds a verdict for an occupant, a lost runner's incarnation that is
/// owed an abort or was aborted, and a subtree wait whose stuck member's
/// abort is owed or was issued, the production floor allows the verdict
/// exactly where the model enables it.
fn site_verdict_floor(cfg: &Cfg, pre: &State, out: &mut Vec<&'static str>) {
    for (&t, cell) in &pre.cells {
        if cell.exec != Exec::RunnerFailed || !pre.live(t) || cell.verdict || !pre.has_sup(cell.sup)
        {
            continue;
        }
        let (aborted, owed) = (pre.inc(t).aborted, pre.abort_owed(t));
        if !aborted && !owed {
            continue;
        }
        FLOOR_SITES.with(|c| c.set(c.get() + 1));
        if img_verdict_allowed(aborted, owed) != aborted {
            out.push(VERDICT_FLOOR_CONFORMANCE);
        }
    }
    let mut waits: Vec<Tok> = pre
        .cells
        .values()
        .filter(|c| c.exec == Exec::Attempt(AttemptStage::Wait))
        .filter_map(|c| c.waits_on)
        .collect();
    if let Some(caller) = pre.caller {
        if matches!(caller.kind, CKind::TermSubtree | CKind::SpawnWait) {
            waits.push(caller.t);
        }
    }
    for w in waits {
        let live = pre.subtree_live(w);
        let stuck = live
            .iter()
            .any(|u| matches!(pre.inc(*u).stage, Stage::Stuck(_)));
        if pre.gone(w) || !stuck || pre.release_bound(cfg, w) == INF {
            continue;
        }
        let owed = live.iter().any(|u| pre.abort_owed(*u));
        let aborted = live.iter().any(|u| pre.inc(*u).aborted);
        if !owed && !aborted {
            continue;
        }
        FLOOR_SITES.with(|c| c.set(c.get() + 1));
        if img_verdict_allowed(aborted, owed) != pre.expirable(cfg, w) {
            out.push(VERDICT_FLOOR_CONFORMANCE);
        }
    }
}

/// The model's floor in nanoseconds past a base: the later of the budget
/// deadline and the abort plus the settle time, at least the cap while an
/// abort is owed, and never past the cap plus the settle time.
fn m_verdict_floor(budget_at: u64, cap: u64, aborted_at: Option<u64>, owed: bool) -> u64 {
    let settled = aborted_at.map_or(budget_at, |at| budget_at.max(sat(at, ABORT_SETTLE)));
    let held = if owed {
        settled.max(sat(budget_at, cap))
    } else {
        settled
    };
    held.min(sat(sat(budget_at, cap), ABORT_SETTLE))
}

/// Domain conformance of `verdict_floor` over instants within reach and a
/// cap at the far-future limit, where the production sum saturates.
fn domain_verdict_floor() -> bool {
    let base = StdInstant::now();
    let far = rules::saturating_nanos(crate::actor::runtime::FAR_FUTURE);
    let points = [
        0u64,
        1,
        50_000_000,
        ABORT_SETTLE,
        250_000_000,
        1_000_000_000,
    ];
    let caps = [0u64, 1, ABORT_SETTLE, 600_000_000, far, INF];
    let mut ok = true;
    for &b in &points {
        for &cap in &caps {
            for a in std::iter::once(None).chain(points.iter().map(|p| Some(*p))) {
                for owed in [false, true] {
                    let img_cap = if cap == INF {
                        Duration::MAX
                    } else {
                        Duration::from_nanos(cap)
                    };
                    let got = rules::verdict_floor(
                        base + Duration::from_nanos(b),
                        img_cap,
                        a.map(|a| base + Duration::from_nanos(a)),
                        owed,
                    );
                    let want = m_verdict_floor(b, cap.min(far), a, owed);
                    if got != base + Duration::from_nanos(want) {
                        ok = false;
                    }
                }
            }
        }
    }
    ok
}

/// A model duration in nanoseconds as a production duration: `INF` reads as
/// the longest duration.
fn img_grace(ns: u64) -> Duration {
    if ns == INF {
        Duration::MAX
    } else {
        Duration::from_nanos(ns)
    }
}

/// A production bound in nanoseconds: none reads as `INF`.
fn opt_ns(bound: Option<Duration>) -> u64 {
    bound.map_or(INF, rules::saturating_nanos)
}

/// The model's decision of a wait on a subtree: an order cancels, the end of
/// the subtree lets the waiter go on, the expiry of the bound is the verdict.
fn m_wait_step(gone: bool, order: bool, expired: bool) -> rules::WaitAction {
    if order {
        rules::WaitAction::Cancel
    } else if gone {
        rules::WaitAction::Proceed
    } else if expired {
        rules::WaitAction::Escalate
    } else {
        rules::WaitAction::Keep
    }
}

/// The model's bound of a wait on a subtree carrying `below`: unbounded while
/// an unbounded supervisor child lives below it, else the budget plus the
/// settle time.
fn m_release_bound(below: u64, inf_live: bool) -> u64 {
    if inf_live {
        INF
    } else {
        sat(below, ABORT_SETTLE)
    }
}

/// Site conformance of `successor_bound` and `successor_wait_step`.
///
/// The bound: for every wait on a subtree in the state (an attempt in its wait
/// stage, a handler waiting for a subtree) the model's bound, the production
/// function's answer and the model's own formula agree.
///
/// The decision: the inputs of the production function are read from the
/// state (the cell's order, the end of the subtree, the model's expiry), and
/// its answer must equal the decision of the model's step at each of the five
/// steps that end or answer a wait, and `Keep` at every wait that has none of
/// them enabled. A case where an order and an expired bound hold together is
/// not forced here: a state that reaches it is compared at its step, and the
/// domain sweep holds the rule for the rows no state reaches.
///
/// The production budget of a parent rises before a registration that can be
/// refused, and the model's only after one that succeeds, so after a refused
/// spawn the production bound can exceed the model's. That is the safe
/// direction (a longer wait, never a shorter one), and it is not compared.
fn site_waits(cfg: &Cfg, pre: &State, step: Step, out: &mut Vec<&'static str>) {
    let fails = std::cell::RefCell::new(Vec::<&'static str>::new());
    let compare_bound = |w: Tok| {
        BOUND_SITES.with(|c| c.set(c.get() + 1));
        let below = pre.incs.get(&w).map_or(0, |i| i.below);
        let inf_live = pre.inf_live(cfg, w);
        let got = rules::successor_bound(below, u32::from(inf_live));
        if opt_ns(got) != pre.release_bound(cfg, w)
            || m_release_bound(below, inf_live) != pre.release_bound(cfg, w)
        {
            fails.borrow_mut().push(SUCCESSOR_BOUND_CONFORMANCE);
        }
    };
    let mut compare_decision = |gone: bool, order: bool, expired: bool, want: rules::WaitAction| {
        WAIT_STEP_SITES.with(|c| c.set(c.get() + 1));
        if rules::successor_wait_step(gone, order, expired) != want
            || m_wait_step(gone, order, expired) != want
        {
            out.push(SUCCESSOR_WAIT_STEP_CONFORMANCE);
        }
    };
    // Every wait in the state.
    for cell in pre.cells.values() {
        if cell.exec != Exec::Attempt(AttemptStage::Wait) {
            continue;
        }
        let Some(w) = cell.waits_on else {
            continue;
        };
        compare_bound(w);
        let (gone, order, expired) = (pre.gone(w), cell.order.is_some(), pre.expirable(cfg, w));
        if !gone && !order && !expired {
            compare_decision(gone, order, expired, rules::WaitAction::Keep);
        }
    }
    if let Some(caller) = pre.caller {
        if matches!(caller.kind, CKind::TermSubtree | CKind::SpawnWait) {
            compare_bound(caller.t);
            let (gone, expired) = (pre.gone(caller.t), pre.expirable(cfg, caller.t));
            if !gone && !expired {
                compare_decision(gone, false, expired, rules::WaitAction::Keep);
            }
        }
    }
    // The step taken: the decision it stands for, on the inputs the state holds.
    let (waited, order, want) = match step {
        Step::AttemptCancels(m) if pre.cell(m).exec == Exec::Attempt(AttemptStage::Wait) => (
            pre.cell(m).waits_on,
            pre.cell(m).order.is_some(),
            rules::WaitAction::Cancel,
        ),
        Step::SuccessorProceeds(m) => (
            pre.cell(m).waits_on,
            pre.cell(m).order.is_some(),
            rules::WaitAction::Proceed,
        ),
        Step::SuccessorWaitExpires(m) => (
            pre.cell(m).waits_on,
            pre.cell(m).order.is_some(),
            rules::WaitAction::Escalate,
        ),
        Step::SubtreeGone => (pre.caller.map(|c| c.t), false, rules::WaitAction::Proceed),
        Step::SubtreeWaitExpires => (pre.caller.map(|c| c.t), false, rules::WaitAction::Escalate),
        _ => {
            out.extend(fails.into_inner());
            return;
        }
    };
    match waited {
        Some(w) => compare_decision(pre.gone(w), order, pre.expirable(cfg, w), want),
        None => fails.borrow_mut().push(SUCCESSOR_WAIT_STEP_CONFORMANCE),
    }
    out.extend(fails.into_inner());
}

/// Domain conformance of `successor_bound`: a budget of nothing, a small one,
/// an unbounded one and one near the top of the range, with an unbounded
/// supervisor child below or not.
fn domain_successor_bound() -> bool {
    [0, 7_000_000, INF, INF - 1].iter().all(|&below| {
        [false, true].iter().all(|&inf| {
            opt_ns(rules::successor_bound(below, u32::from(inf))) == m_release_bound(below, inf)
        })
    })
}

/// Domain conformance of `successor_wait_step`: every combination of the
/// subtree having ended, an order being present and the bound having passed.
fn domain_successor_wait_step() -> bool {
    [false, true].iter().all(|&gone| {
        [false, true].iter().all(|&order| {
            [false, true].iter().all(|&expired| {
                rules::successor_wait_step(gone, order, expired)
                    == m_wait_step(gone, order, expired)
            })
        })
    })
}

thread_local! {
    /// The guard firings whose inherited stop the guard check compared with
    /// the production function in this search.
    static GUARD_SITES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The model's inherited stop of a child, with the durations of its
/// configuration: a worker is killed and its abort follows its kill grace; a
/// supervisor child stops in order, and `Kill` is raised at its window (zero
/// under `Kill`, none under `Infinity`) with the abort its kill grace after
/// that.
fn m_inherited_stop(
    is_supervisor: bool,
    policy: Policy,
    window_ns: u64,
    kill_grace_ns: u64,
    kill_stored: bool,
) -> rules::InheritedStop {
    let grace = Clock::duration(kill_grace_ns);
    if !is_supervisor || kill_stored {
        return rules::InheritedStop::Kill { grace };
    }
    let window = match policy {
        Policy::Kill => Some(0),
        Policy::Timeout => Some(window_ns),
        Policy::Infinity => None,
    };
    rules::InheritedStop::Orderly {
        window: window.map(Clock::duration),
        grace,
    }
}

/// The production image of what the model's guard did: `Kill` and an abort
/// entry for a worker, `ParentKilled` and a two-stage entry for a supervisor
/// child under `Kill` or `Timeout`, `ParentKilled` and no entry under
/// `Infinity`. None for anything the production function has no value for.
fn img_guard(
    policy: Policy,
    window_ns: u64,
    kill_grace_ns: u64,
    raised: Reason,
    entry: Option<Reap>,
) -> Option<rules::InheritedStop> {
    let grace = Clock::duration(kill_grace_ns);
    match (raised, entry) {
        (Reason::Kill, Some(Reap::AbortDue)) => Some(rules::InheritedStop::Kill { grace }),
        (Reason::ParentKilled, Some(Reap::KillThenAbort)) => {
            let window = match policy {
                Policy::Kill => 0,
                Policy::Timeout => window_ns,
                Policy::Infinity => return None,
            };
            Some(rules::InheritedStop::Orderly {
                window: Some(Clock::duration(window)),
                grace,
            })
        }
        (Reason::ParentKilled, None) if policy == Policy::Infinity => {
            Some(rules::InheritedStop::Orderly {
                window: None,
                grace,
            })
        }
        _ => None,
    }
}

/// A production duration for a configured model duration, where the largest
/// stands for the longest.
fn m_policy_of(policy: Policy, window_ns: u64) -> MPolicy {
    match policy {
        Policy::Kill => MPolicy::Kill,
        Policy::Timeout => MPolicy::Timeout(window_ns),
        Policy::Infinity => MPolicy::Infinity,
    }
}

/// Domain conformance of `inherited_stop`: a worker or a supervisor child
/// under every policy, against a kill grace of nothing, the default, a long
/// one and the longest, with a window of nothing, a few seconds and the
/// longest, each answer against the model's.
fn domain_inherited_stop() -> bool {
    let mut ok = true;
    for is_supervisor in [false, true] {
        for policy in [Policy::Kill, Policy::Timeout, Policy::Infinity] {
            for window_ns in [0, 3_000_000_000, INF] {
                for grace_ns in [0, 120_000_000, 1_500_000_000, INF] {
                    for kill_stored in [false, true] {
                        let shutdown = Clock::shutdown(m_policy_of(policy, window_ns));
                        let got = rules::fired_stop(
                            rules::inherited_stop(
                                is_supervisor,
                                shutdown,
                                Clock::duration(grace_ns),
                            ),
                            kill_stored,
                        );
                        let want = m_inherited_stop(
                            is_supervisor,
                            policy,
                            window_ns,
                            grace_ns,
                            kill_stored,
                        );
                        if got != want {
                            ok = false;
                        }
                    }
                }
            }
        }
    }
    ok
}

/// Conformance of `own_budget` and `own_fin` over the child shapes of one
/// configuration: every child the run creates, a supervisor child included,
/// against the model function of the same name.
fn site_budgets(cfg: &Cfg, out: &mut Vec<&'static str>) {
    for kid in cfg.kids.iter().chain(cfg.workers.iter()) {
        BUDGET_SITES.with(|c| c.set(c.get() + 1));
        compare_budgets(kid, out);
    }
}

/// One child shape's two budgets, production against the model.
fn compare_budgets(kid: &ChildCfg, out: &mut Vec<&'static str>) {
    let shutdown = shutdown_of(kid);
    let grace = Clock::duration(kid.kg());
    let fin = rules::saturating_nanos(rules::own_fin(kid.is_supervisor, shutdown, grace));
    if fin != kid.own_fin() {
        out.push(OWN_FIN_CONFORMANCE);
    }
    let budget = rules::own_budget(kid.is_supervisor, shutdown, grace);
    if budget.map_or(INF, rules::saturating_nanos) != kid.own_budget() {
        out.push(OWN_BUDGET_CONFORMANCE);
    }
}

/// Domain conformance of `own_budget` and `own_fin`: a worker or a supervisor
/// child under every policy, against a kill grace of nothing, the default, a
/// long one and the longest, and a window of nothing, a few seconds and the
/// longest.
fn domain_budgets(out: &mut Vec<&'static str>) {
    for is_supervisor in [false, true] {
        for policy in [Policy::Kill, Policy::Timeout, Policy::Infinity] {
            for window_ns in [0, 3_000_000_000, INF] {
                for grace in [
                    Grace::Zero,
                    Grace::Standard,
                    Grace::Ns(1_500_000_000),
                    Grace::Ns(INF),
                ] {
                    let mut kid = ChildCfg::new().policy(policy).grace(grace);
                    kid.is_supervisor = is_supervisor;
                    kid.window_ns = window_ns;
                    compare_budgets(&kid, out);
                }
            }
        }
    }
}

/// The production lane's answer for the image of a model lane holding
/// `stored` in `mode`, and a request `new`.
fn prod_lane(
    stored: Option<Reason>,
    new: Reason,
    mode: LaneMode,
    receiver_gone: bool,
) -> (RaiseResult, bool, bool) {
    let state = LaneState {
        reason: stored.map(img_reason),
        generation: 0,
        kill_only: mode != LaneMode::Open,
        sealed: mode == LaneMode::Sealed,
    };
    rules::lane_accept(&state, &img_reason(new), receiver_gone)
}

/// The model's decision on a request: refused when the target ended, the lane
/// is sealed, or it is open to `Kill` only and the request is not one; a
/// strictly weaker request is superseded; anything else is recorded and
/// stored when it is stronger, or equally strong and at least as high in rank.
fn m_lane_accept(
    stored: Option<Reason>,
    new: Reason,
    mode: LaneMode,
    receiver_gone: bool,
) -> (RaiseResult, bool, bool) {
    if receiver_gone
        || mode == LaneMode::Sealed
        || (mode == LaneMode::KillOnly && new != Reason::Kill)
    {
        return (RaiseResult::Refused, false, false);
    }
    match stored {
        None => (RaiseResult::Recorded, true, true),
        Some(cur) if strength(new) < strength(cur) => (RaiseResult::Superseded, false, false),
        Some(cur) => (
            RaiseResult::Recorded,
            strength(new) > strength(cur) || rank(new) >= rank(cur),
            true,
        ),
    }
}

/// Domain conformance of `lane_accept`: every stored reason (none included)
/// against every request, with and without a receiver, in every lane mode.
fn domain_lane_accept() -> bool {
    let reasons = [
        Reason::Graceful,
        Reason::ParentRequest,
        Reason::Failure(Cause::Crash),
        Reason::ParentKilled,
        Reason::Cancelled,
        Reason::Kill,
    ];
    let stored_rows: Vec<Option<Reason>> = std::iter::once(None)
        .chain(reasons.iter().copied().map(Some))
        .collect();
    let mut ok = true;
    for stored in &stored_rows {
        for new in reasons {
            for receiver_gone in [false, true] {
                for mode in [LaneMode::Open, LaneMode::KillOnly, LaneMode::Sealed] {
                    if prod_lane(*stored, new, mode, receiver_gone)
                        != m_lane_accept(*stored, new, mode, receiver_gone)
                    {
                        ok = false;
                    }
                }
            }
        }
    }
    ok
}

/// Domain conformance of `init_kill_check`: a lane holding nothing, a
/// requested reason, a forced reason and `Kill`.
fn domain_init_kill_check() -> bool {
    let rows = [
        None,
        Some(Reason::Graceful),
        Some(Reason::ParentRequest),
        Some(Reason::Failure(Cause::Crash)),
        Some(Reason::ParentKilled),
        Some(Reason::Cancelled),
        Some(Reason::Kill),
    ];
    rows.iter().all(|lane| {
        let image = lane.map(img_reason);
        (rules::init_kill_check(image.as_ref()) == rules::InitStep::KillNow)
            == (*lane == Some(Reason::Kill))
    })
}

fn img_ladder(ladder: Ladder) -> AttemptLadder {
    match ladder {
        Ladder::Off => AttemptLadder::Off,
        Ladder::Window | Ladder::WindowInf => AttemptLadder::Window,
        Ladder::KillGrace => AttemptLadder::KillGrace,
        Ladder::AbortGrace => AttemptLadder::AbortGrace,
        Ladder::Spent => AttemptLadder::Spent,
    }
}

fn img_at_stage(stage: AtStage) -> RAttemptStage {
    match stage {
        AtStage::Factory => RAttemptStage::Factory,
        AtStage::Init => RAttemptStage::Init,
        AtStage::InTransit => RAttemptStage::InTransit,
    }
}

fn img_at_event(event: AtEvent) -> AttemptEvent {
    match event {
        AtEvent::Begin { ordered } => AttemptEvent::Begin { ordered },
        AtEvent::Order => AttemptEvent::Order,
        AtEvent::Ack { ordered, ended } => AttemptEvent::Ack { ordered, ended },
        AtEvent::InitEnded => AttemptEvent::InitEnded,
        AtEvent::StartTimeout => AttemptEvent::StartTimeout,
        AtEvent::LadderSpent => AttemptEvent::LadderSpent,
        AtEvent::Ended {
            ordered,
            timeout_raised,
        } => AttemptEvent::Ended {
            ordered,
            timeout_raised,
        },
    }
}

fn img_at_want(want: AtWant) -> AttemptAction {
    match want {
        AtWant::Nothing => AttemptAction::None,
        AtWant::Cancel => AttemptAction::Cancel,
        AtWant::Spawn => AttemptAction::Spawn,
        AtWant::ArmLadder => AttemptAction::ArmLadder,
        AtWant::Adopt => AttemptAction::Adopt,
        AtWant::KillPath { raise } => AttemptAction::KillPath { raise },
        AtWant::WriteVerdict => AttemptAction::WriteVerdict,
        AtWant::Report(AtReport::Stopped) => AttemptAction::Report(AttemptReport::Stopped),
        AtWant::Report(AtReport::StartTimeout) => {
            AttemptAction::Report(AttemptReport::StartTimeout)
        }
        AtWant::Report(AtReport::Failed) => AttemptAction::Report(AttemptReport::Failed),
    }
}

fn img_walk(input: &WalkInput) -> WalkChild {
    WalkChild {
        life: input.life.map(img_life),
        has_order: input.has_order,
        ask_possible: input.ask_possible,
        reasked: input.reasked,
        attempt_past_init: input.past_init,
    }
}

fn img_td(want: TdWant) -> TeardownAction {
    match want {
        TdWant::Skip => TeardownAction::Skip,
        TdWant::Order => TeardownAction::Order,
        TdWant::Join { ask } => TeardownAction::Join { ask },
        TdWant::OrderAttempt { ask } => TeardownAction::OrderAttempt { ask },
    }
}

fn img_place(place: AbPlace) -> ReportPlace {
    match place {
        AbPlace::Queued => ReportPlace::Queued,
        AbPlace::Worklist => ReportPlace::Worklist,
        AbPlace::DeathPlane => ReportPlace::DeathPlane,
    }
}

fn img_ab(want: AbWant) -> Absorption {
    match want {
        AbWant::Keep => Absorption::Keep,
        AbWant::Deliver => Absorption::Deliver,
        AbWant::Snapshot => Absorption::Snapshot,
    }
}

/// The model's decision for one member of a group evaluation, written out
/// from the facts of the member: whether the evaluation commits the member to
/// the barrier (it has an occupant, and is not the trigger), whether it
/// cancels the member's restart attempt, and whether the member restarts. The
/// evaluation stops and asks nothing: the stop queue does, in each member's
/// turn.
fn m_commit(input: &CommitInput, failed: Key, overridden: bool) -> (bool, bool, bool) {
    let occupied = matches!(
        input.life,
        Life::Running(_) | Life::Stopping { .. } | Life::Restarting { .. }
    );
    let held = input.key != failed && occupied;
    // An attempt in flight is committed at its own incarnation, spawned or
    // not: the flag cancels one that has not spawned its actor yet and an
    // attempt past that point ignores it.
    let cancel = held && matches!(input.life, Life::Restarting { .. });
    (
        held,
        cancel,
        input.restart != Restart::Temporary && !overridden,
    )
}

/// Site conformance of `group_commit` and of `retry_plan`: the set the model
/// built at an evaluation or at a retry, against the commit list production
/// computes from the same ledger. An attempt in flight is committed at its own
/// incarnation like any member with an occupant.
#[allow(clippy::too_many_arguments)]
fn site_commit(
    failed: Key,
    rest_for_one: bool,
    members: &[CommitInput],
    awaiting: &BTreeMap<Key, Tok>,
    restart_order: &[Key],
    retry: bool,
    overrides: &[Key],
    out: &mut Vec<&'static str>,
) {
    let id = |k: Key| ActorId::from(format!("child-{k}"));
    let children: Vec<GroupMember> = members
        .iter()
        .map(|m| GroupMember {
            id: id(m.key),
            life: img_life(m.life),
            restart_type: img_restart(m.restart),
        })
        .collect();
    let strategy = if rest_for_one {
        RestartStrategy::RestForOne
    } else {
        RestartStrategy::OneForAll
    };
    let released: Vec<ActorId> = overrides.iter().map(|k| id(*k)).collect();
    let plan = rules::retry_plan(strategy, &children, &released, &id(failed));
    let commit = if retry {
        plan.commit.clone()
    } else {
        rules::group_commit(strategy, &children, &id(failed))
    };
    let covered: Vec<Key> = {
        let start = if rest_for_one {
            members.iter().position(|m| m.key == failed).unwrap_or(0)
        } else {
            0
        };
        members[start..].iter().map(|m| m.key).collect()
    };
    let mut bad_commit = commit.len() != covered.len();
    let mut bad_scope = false;
    for (entry, input) in commit
        .iter()
        .zip(members.iter().skip(members.len() - covered.len()))
    {
        let overridden = retry && overrides.contains(&input.key);
        let (want_held, want_cancel, want_restarts) = m_commit(input, failed, overridden);
        let barrier_token = awaiting.get(&input.key).copied();
        // The model's own table against its state: a member the model stops
        // is one the barrier awaits, and a member it restarts is one the
        // restart order holds (a retry's scope is the retry's own).
        if want_held != barrier_token.is_some()
            || (want_restarts != restart_order.contains(&input.key) && !retry)
        {
            out.push(MODEL_TABLE_CONFORMANCE);
        }
        if entry.0 != id(input.key)
            || entry.2.held != want_held
            || entry.2.cancel != want_cancel
            || want_held != barrier_token.is_some()
            || barrier_token.is_some_and(|t| u64::from(t) != entry.1)
        {
            bad_commit = true;
        }
        if entry.2.restarts != want_restarts
            || entry.2.restarts != restart_order.contains(&input.key)
        {
            bad_scope = true;
        }
    }
    if bad_commit || (bad_scope && !retry) {
        out.push(GROUP_COMMIT_CONFORMANCE);
    }
    if retry {
        let want_order: Vec<ActorId> = restart_order.iter().map(|k| id(*k)).collect();
        if bad_scope || plan.restart_order != want_order {
            out.push(RETRY_PLAN_CONFORMANCE);
        }
    }
}

/// The model's children view of every key against `rules::child_info_flags`
/// computed from the same three inputs.
fn state_flags(post: &State, out: &mut Vec<&'static str>) {
    for (c, life) in &post.sup.ledger {
        let fate_present = match life {
            Life::Running(n) | Life::Stopping { n, .. } => post.ended(*n),
            _ => false,
        };
        let got = rules::child_info_flags(img_life(*life), fate_present, post.in_restart_set(*c));
        if got != post.child_info_flags(*c) {
            out.push(CHILD_INFO_FLAGS_CONFORMANCE);
        }
    }
}

/// The independent table of the answers a manual call gives without waiting:
/// `restart_child` and `delete_child` (3.3 and 3.4), and `stop_child` and
/// `terminate_child` (3.1 and 3.2: the answer of a cell that answers, `Ok` for
/// a call that goes on). `None` for a cell that is unreachable or decided by
/// a later rule.
fn m_answer_cell(i: CellIn) -> Option<ManualReply> {
    use ManualReply::{ChildNotFound, ChildRestarting, ChildRunning, Ok as Done};
    let (row, memb) = (i.row, i.memb);
    if matches!(i.op, CallOp::Stop | CallOp::Terminate) {
        let commit = m_manual_cell(i)?;
        return Some(match commit.effects.as_slice() {
            [CommitEffect::Answer(reply)] => *reply,
            _ => Done,
        });
    }
    let restart = i.op == CallOp::Restart;
    Some(match (row, memb) {
        (Row::NoSpec, Memb::Outside) => ChildNotFound,
        (Row::NoSpec, _) => return None,
        (Row::RunningLive, Memb::Outside) => {
            if restart && i.restarted {
                ChildRestarting
            } else {
                ChildRunning
            }
        }
        (Row::RunningLive, Memb::Awaited) => ChildRestarting,
        (Row::RunningEnded, Memb::Outside | Memb::Awaited) => ChildRestarting,
        (
            Row::BounceLive | Row::BounceEnded | Row::TerminateLive | Row::TerminateEnded,
            Memb::Outside | Memb::Awaited,
        ) => ChildRestarting,
        (Row::TerminateLive | Row::TerminateEnded, Memb::Released) => ChildRestarting,
        (Row::Attempt, Memb::Outside | Memb::Awaited | Memb::Queued) => ChildRestarting,
        (Row::DownPlain | Row::DownSnapshot, Memb::Outside) => Done,
        (Row::DownPlain | Row::DownSnapshot, Memb::Queued) => ChildRestarting,
        (Row::DownPlain | Row::DownSnapshot, Memb::Released) => {
            if restart {
                ChildRestarting
            } else {
                Done
            }
        }
        (Row::DownOwed, Memb::Outside | Memb::Queued) => ChildRestarting,
        _ => return None,
    })
}

/// The production operation a call maps to.
fn img_op(op: CallOp) -> Option<ManualOp> {
    match op {
        CallOp::Stop => Some(ManualOp::StopChild),
        CallOp::Terminate => Some(ManualOp::TerminateChild),
        CallOp::Restart => Some(ManualOp::RestartChild),
        CallOp::Delete => Some(ManualOp::DeleteChild),
        CallOp::Spawn => None,
    }
}

/// Site conformance of `manual_answer`: every manual call the model takes
/// except a spawn.
fn site_answer(cfg: &Cfg, pre: &State, step: Step, ap: &Applied, out: &mut Vec<&'static str>) {
    let (op, c) = match step {
        Step::StopChild(c) => (CallOp::Stop, c),
        Step::TerminateChild(c) => (CallOp::Terminate, c),
        Step::RestartChild(c) => (CallOp::Restart, c),
        Step::DeleteChild(c) => (CallOp::Delete, c),
        _ => return,
    };
    let (cell, row, life) = site_cell(cfg, pre, op, c);
    let task = img_task(pre, row, Tok::try_from(cell.n).unwrap_or_default());
    let facts = rules::ManualFacts {
        life: life.map(img_life),
        ..img_facts(&cell, task, shutdown_of(cfg.kid(c)))
    };
    ANSWER_SITES.with(|n| n.set(n.get() + 1));
    let got = rules::manual_answer(img_op(op).expect("a manual call"), &facts);
    // A `stop_child` or `terminate_child` that goes on (a stop it commits or
    // joins, a wait on the subtree, a verdict answered at its first poll) is
    // answered `Ok` here; the answer it gives later is the check of
    // `CallerSees`.
    let want = match (op, ap.answer) {
        (CallOp::Stop | CallOp::Terminate, None | Some(Answer::ChildUnresponsive)) => {
            Some(ManualReply::Ok)
        }
        (_, answer) => answer.and_then(img_reply),
    };
    if want != Some(got) || m_answer_cell(cell).is_some_and(|table| table != got) {
        out.push(MANUAL_ANSWER_CONFORMANCE);
    }
}

/// The production image of the supervisor under test: its ledger, the pending
/// group's sets and the snapshots of completed manual stops.
/// What keeps an imaged slot alive: its executor half and the senders of the
/// planes its cell publishes, so the cell reads open.
struct KeptSlot {
    _io: super::SlotIo,
    _lane_rx: tokio::sync::watch::Receiver<LaneState>,
    _status_tx: Option<tokio::sync::watch::Sender<crate::types::StatusValue>>,
}

fn img_status(status: Status) -> crate::types::ActorStatus {
    use crate::types::ActorStatus;
    match status {
        Status::Initializing => ActorStatus::Initializing,
        Status::Running => ActorStatus::Running,
        Status::Stopping => ActorStatus::Stopping,
        Status::Stopped | Status::Closed => ActorStatus::Stopped,
    }
}

/// The production image of the supervisor under test: its ledger with each
/// slot as the model's cell and incarnation read (the stop order, the
/// one-more-ask record, the verdict, the fate, and, once the incarnation
/// exists, a published lane carrying the request and a status plane), the
/// pending group and the snapshots. `kept` holds what keeps the slots open.
fn img_supervision(cfg: &Cfg, pre: &State, kept: &mut Vec<KeptSlot>) -> SupervisionState {
    let mut sup = SupervisionState::new(SupervisionConfig::default());
    let id = |k: Key| ActorId::from(format!("child-{k}"));
    for (k, life) in &pre.sup.ledger {
        let (ctl, io) = super::slot_pair(Default::default());
        let occupant = match life {
            Life::Running(n) | Life::Stopping { n, .. } | Life::Down { n, .. } => *n,
            Life::Restarting { next, .. } => *next,
        };
        let policy = shutdown_of(cfg.kid(*k));
        let mut lane_keep = None;
        let mut status_keep = None;
        if let Some(cell) = pre.cells.get(&occupant) {
            if let Some(order) = cell.order {
                ctl.write_order(policy, order.raised);
            }
            if cell.verdict {
                io.write_verdict();
            }
            if let Some(fate) = cell.fate {
                io.record_fate(crate::types::ChildFate {
                    reason: img_reason(fate),
                    incarnation: u64::from(occupant),
                });
            }
        }
        if let Some(inc) = pre.incs.get(&occupant) {
            if inc.reasked {
                ctl.mark_reasked();
            }
            let (lane, lane_rx) = crate::types::StopLane::new();
            if let Some(reason) = inc.lane {
                lane.raise(img_reason(reason));
            }
            let (status_tx, status_rx) =
                tokio::sync::watch::channel(crate::types::StatusValue::new(img_status(inc.status)));
            io.publish(
                lane,
                status_rx,
                crate::actor::supervision::OwnStop::worker(
                    crate::actor::supervision::DEFAULT_KILL_GRACE,
                ),
                None,
            );
            lane_keep = Some(lane_rx);
            status_keep = Some(status_tx);
        }
        if let Some(lane_rx) = lane_keep {
            kept.push(KeptSlot {
                _io: io,
                _lane_rx: lane_rx,
                _status_tx: status_keep,
            });
        } else {
            let (_lane, lane_rx) = crate::types::StopLane::new();
            kept.push(KeptSlot {
                _io: io,
                _lane_rx: lane_rx,
                _status_tx: None,
            });
        }
        sup.registry.register(ChildState {
            id: id(*k),
            name: None,
            spec: ChildSpec {
                restart_type: img_restart(cfg.kid(*k).restart),
                shutdown: shutdown_of(cfg.kid(*k)),
                start_timeout: None,
                kill_grace: crate::actor::supervision::DEFAULT_KILL_GRACE,
                is_supervisor: false,
            },
            ctl,
            lifecycle: img_life(*life),
            origin: ProdOrigin::Initial,
            predecessor: None,
        });
    }
    if let Some(g) = &pre.sup.group {
        // An override on a member the stop phase awaits names the incarnation
        // the group told to stop. Any other override is a `terminate_child`
        // that keeps its key down: it names the incarnation the call
        // concerned, the one the ledger holds for the key (a member a chain
        // restarted holds its second incarnation, a child started since has
        // no entry in the group's plan).
        let overrides: HashMap<ActorId, u64> = g
            .overrides
            .iter()
            .filter_map(|k| {
                let held = pre.life(*k).map(held_token)?;
                let token = match &g.phase {
                    Phase::Stop { awaiting, .. } => awaiting.get(k).copied().unwrap_or(held),
                    Phase::Chain { .. } => held,
                };
                Some((id(*k), u64::from(token)))
            })
            .collect();
        let scope = GroupScope {
            restarted: g.restarted.iter().map(|k| id(*k)).collect(),
            outside: g.outside.iter().map(|k| id(*k)).collect(),
        };
        sup.pending_group = Some(match &g.phase {
            Phase::Stop {
                awaiting,
                queue,
                signalled,
                restart_order,
            } => GroupPhase::Stopping(GroupRestart {
                awaiting: awaiting
                    .iter()
                    .map(|(k, t)| (id(*k), u64::from(*t)))
                    .collect(),
                stop_queue: queue.iter().map(|k| id(*k)).collect::<ProdQueue<_>>(),
                head_reached: queue.front().is_some_and(|h| signalled.contains(h)),
                restart_order: restart_order.iter().map(|k| id(*k)).collect(),
                manual_overrides: overrides,
                scope,
            }),
            Phase::Chain { queue, started } => GroupPhase::Restarting {
                queue: queue.iter().map(|k| id(*k)).collect::<ProdQueue<_>>(),
                overrides,
                started: started
                    .iter()
                    .map(|(k, t)| (id(*k), u64::from(*t)))
                    .collect(),
                scope,
            },
        });
    }
    for ((k, t), (r, a)) in &pre.sup.snapshots {
        sup.pending_manual_events.insert(
            (id(*k), u64::from(*t)),
            ChildEvent {
                child_id: id(*k),
                child_name: None,
                reason: img_reason(*r),
                action: img_action(*a),
            },
        );
    }
    sup
}

/// Conformance of `group_membership`: for every key of the ledger of every
/// state reached while a group restart is pending, production's membership of
/// the key, read from the production image of the model's state, against the
/// model's. A key a `terminate_child` released is imaged at the ledger
/// occupant at the call, which for a member the stop queue had not reached,
/// or a child started since, is not the incarnation the group's plan holds
/// for the key.
fn site_membership(cfg: &Cfg, post: &State, out: &mut Vec<&'static str>) {
    let Some(g) = &post.sup.group else {
        return;
    };
    let mut kept = Vec::new();
    let sup = img_supervision(cfg, post, &mut kept);
    for (c, life) in &post.sup.ledger {
        let want = img_membership(post.membership(*c), g.overrides.contains(c));
        let got = sup.group_membership(&ActorId::from(format!("child-{c}")));
        MEMBERSHIP_SITES.with(|n| n.set(n.get() + 1));
        if g.overrides.contains(c)
            && !matches!(g.members.get(c), Some(Some(t)) if *t == held_token(*life))
        {
            if g.restarted.contains(c) {
                RELEASED_AFTER_RESTART.with(|n| n.set(n.get() + 1));
            } else if !g.members.contains_key(c) {
                RELEASED_SPAWNED_SINCE.with(|n| n.set(n.get() + 1));
            }
        }
        if got != want {
            out.push(GROUP_MEMBERSHIP_CONFORMANCE);
        }
        // The input of the manual classification: the key is in the stop
        // phase's restart order or the chain's queue, and not released.
        let in_set = g.phase.restart_set_contains(*c) && !g.overrides.contains(c);
        if sup.in_restart_set(&ActorId::from(format!("child-{c}"))) != in_set {
            out.push(IN_RESTART_SET_CONFORMANCE);
        }
    }
}

// ---- the domains --------------------------------------------------------------

/// The ledger entries of the domain sweeps: every shape over small tokens.
fn domain_lives() -> Vec<Life> {
    let mut lives = Vec::new();
    for n in 0..4u8 {
        lives.push(Life::Running(n));
        lives.push(Life::Stopping {
            n,
            kind: Kind::Bounce,
        });
        lives.push(Life::Stopping {
            n,
            kind: Kind::Terminate,
        });
        for next in 0..4u8 {
            lives.push(Life::Restarting { prev: n, next });
        }
        for pend in [Pend::Nothing, Pend::Snapshot, Pend::Evaluate] {
            lives.push(Life::Down { n, pend });
        }
    }
    lives
}

/// The model's table of ledger moves with the tokens it draws later than
/// every token in use: eight rows.
fn m_legal(from: Life, to: Life) -> bool {
    match (from, to) {
        (Life::Running(n), Life::Stopping { n: m, .. }) => n == m,
        (Life::Running(n), Life::Down { n: m, .. }) => n == m,
        (Life::Stopping { n, .. }, Life::Down { n: m, .. }) => n == m,
        (
            Life::Stopping {
                n,
                kind: Kind::Bounce,
            },
            Life::Stopping {
                n: m,
                kind: Kind::Terminate,
            },
        ) => n == m,
        (Life::Restarting { next, .. }, Life::Running(m)) => next == m,
        (Life::Restarting { next, .. }, Life::Down { n: m, .. }) => next == m,
        (Life::Down { n, .. }, Life::Restarting { prev, next }) => n == prev && next > n,
        (Life::Down { n, .. }, Life::Down { n: m, .. }) => n == m,
        _ => false,
    }
}

fn domain_is_legal() -> bool {
    let lives = domain_lives();
    lives.iter().all(|from| {
        lives
            .iter()
            .all(|to| rules::is_legal(&img_life(*from), &img_life(*to)) == m_legal(*from, *to))
    })
}

fn domain_report_class() -> bool {
    domain_lives().into_iter().all(|life| {
        (0..5u8).all(|t| {
            let want = if match life {
                Life::Running(n) | Life::Stopping { n, .. } => n == t,
                Life::Restarting { next, .. } => next == t,
                Life::Down { .. } => false,
            } {
                rules::ReportClass::Occupant
            } else if matches!(life, Life::Down { n, pend: Pend::Evaluate } if n == t) {
                rules::ReportClass::OwedEvaluation
            } else {
                rules::ReportClass::Stale
            };
            rules::report_class(img_life(life), u64::from(t)) == want
        })
    })
}

fn domain_child_info_flags() -> bool {
    domain_lives().into_iter().all(|life| {
        [false, true].into_iter().all(|fate| {
            [false, true].into_iter().all(|in_set| {
                let want = match life {
                    Life::Running(_) => (!fate, fate),
                    Life::Stopping { .. } => (!fate, false),
                    Life::Restarting { .. } => (false, true),
                    Life::Down { pend, .. } => (false, pend == Pend::Evaluate || in_set),
                };
                rules::child_info_flags(img_life(life), fate, in_set) == want
            })
        })
    })
}

fn domain_chain_front() -> bool {
    domain_lives().into_iter().all(|life| {
        let tokens: Vec<Option<u8>> = std::iter::once(None).chain((0..4u8).map(Some)).collect();
        tokens.into_iter().all(|started| {
            let want = match life {
                Life::Down { .. } => rules::FrontAction::Initiate,
                Life::Running(k) if started == Some(k) => rules::FrontAction::Fresh,
                Life::Running(_) => rules::FrontAction::StopThenInitiate,
                Life::Stopping { .. } | Life::Restarting { .. } => rules::FrontAction::Hold,
            };
            rules::chain_front(img_life(life), started.map(u64::from)) == want
        })
    })
}

fn domain_attempt_route() -> bool {
    let membs = [Memb::Outside, Memb::Awaited, Memb::Queued, Memb::Released];
    let origins = [
        Origin::Initial,
        Origin::Strategy,
        Origin::Bounce,
        Origin::Manual,
        Origin::Chain,
    ];
    membs.into_iter().all(|memb| {
        origins.into_iter().all(|origin| {
            [false, true].into_iter().all(|own| {
                let want = if own {
                    RouteKind::ChainRetry
                } else if matches!(memb, Memb::Awaited | Memb::Queued) {
                    RouteKind::Member
                } else if origin == Origin::Manual {
                    RouteKind::Removed
                } else {
                    RouteKind::Evaluate
                };
                rules::attempt_failure_route(own, img_membership(memb, false), img_origin(origin))
                    == img_route(want)
            })
        })
    })
}

fn domain_manual_answer() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        let mut ok = true;
        for i in domain_cells() {
            let Some(want) = m_answer_cell(*i) else {
                continue;
            };
            let facts = img_facts(i, domain_task(i), domain_policy(i));
            if rules::manual_answer(img_op(i.op).expect("a manual call"), &facts) != want {
                ok = false;
            }
        }
        ok
    })
}

fn domain_classify_manual() -> bool {
    let reasons = [
        Reason::Graceful,
        Reason::ParentRequest,
        Reason::Failure(Cause::Crash),
        Reason::Cancelled,
        Reason::Kill,
    ];
    let mut ok = true;
    for kind in [Kind::Bounce, Kind::Terminate] {
        for reason in reasons {
            for restart in [Restart::Permanent, Restart::Transient, Restart::Temporary] {
                for pool in [false, true] {
                    for in_set in [false, true] {
                        let crash = matches!(reason, Reason::Failure(_));
                        let prunable = pool || restart == Restart::Temporary;
                        // The decision by the rows of the model's procedure.
                        let want = if kind == Kind::Bounce && crash {
                            (None, Pend::Evaluate, false, false)
                        } else if in_set {
                            (Some(Action::RestartInitiated), Pend::Snapshot, false, false)
                        } else if kind == Kind::Terminate || pool {
                            (Some(Action::Removed), Pend::Snapshot, prunable, false)
                        } else if restart == Restart::Permanent {
                            (Some(Action::RestartInitiated), Pend::Snapshot, false, true)
                        } else {
                            (Some(Action::Removed), Pend::Snapshot, prunable, false)
                        };
                        let got = rules::classify_manual(
                            img_kind(kind),
                            &img_reason(reason),
                            img_restart(restart),
                            pool,
                            in_set,
                        );
                        let same = got.action == want.0.map(img_action)
                            && got.remove == want.2
                            && (got.remove || img_pending_back(got.pending) == want.1)
                            && got.initiate == want.3;
                        if !same {
                            ok = false;
                        }
                    }
                }
            }
        }
    }
    ok
}

/// The model's table of an attempt's executor, written out from the model's
/// own rules (the start timeout acts while the init has not acknowledged and
/// the ladder is not past its window; an acknowledgment hands the incarnation
/// over only under no order and before the ladder acted).
fn m_attempt_step(
    stage: AtStage,
    event: AtEvent,
    ladder: Ladder,
    kill_stored: bool,
    acked: bool,
) -> AtWant {
    let before_window = matches!(ladder, Ladder::Off | Ladder::Window | Ladder::WindowInf);
    match (stage, event) {
        (AtStage::Factory, AtEvent::Begin { ordered }) => {
            if ordered {
                AtWant::Cancel
            } else {
                AtWant::Spawn
            }
        }
        (AtStage::Init, AtEvent::Order) => AtWant::ArmLadder,
        (AtStage::Init, AtEvent::Ack { ordered, .. }) => {
            if !ordered && ladder == Ladder::Off {
                AtWant::Adopt
            } else {
                AtWant::Nothing
            }
        }
        (AtStage::Init, AtEvent::InitEnded) => {
            if ladder == Ladder::Off {
                AtWant::KillPath {
                    raise: !kill_stored,
                }
            } else {
                AtWant::Nothing
            }
        }
        (AtStage::Init, AtEvent::StartTimeout) => {
            if !acked && before_window {
                AtWant::KillPath {
                    raise: !kill_stored,
                }
            } else {
                AtWant::Nothing
            }
        }
        (AtStage::Init, AtEvent::LadderSpent) => AtWant::WriteVerdict,
        (
            AtStage::Init,
            AtEvent::Ended {
                ordered,
                timeout_raised,
            },
        ) => AtWant::Report(match (timeout_raised, ordered) {
            (true, _) => AtReport::StartTimeout,
            (false, true) => AtReport::Stopped,
            (false, false) => AtReport::Failed,
        }),
        _ => AtWant::Nothing,
    }
}

fn domain_attempt_step() -> bool {
    let stages = [AtStage::Factory, AtStage::Init, AtStage::InTransit];
    let ladders = [
        Ladder::Off,
        Ladder::Window,
        Ladder::WindowInf,
        Ladder::KillGrace,
        Ladder::AbortGrace,
        Ladder::Spent,
    ];
    let mut events = vec![
        AtEvent::Order,
        AtEvent::InitEnded,
        AtEvent::StartTimeout,
        AtEvent::LadderSpent,
    ];
    for flag in [false, true] {
        events.push(AtEvent::Begin { ordered: flag });
        for other in [false, true] {
            events.push(AtEvent::Ack {
                ordered: flag,
                ended: other,
            });
            events.push(AtEvent::Ended {
                ordered: flag,
                timeout_raised: other,
            });
        }
    }
    let mut ok = true;
    for stage in stages {
        for event in &events {
            for ladder in ladders {
                for kill_stored in [false, true] {
                    for acked in [false, true] {
                        let want = m_attempt_step(stage, *event, ladder, kill_stored, acked);
                        let view = AttemptView {
                            ladder: img_ladder(ladder),
                            kill_stored,
                            acked,
                        };
                        let got =
                            rules::attempt_step(img_at_stage(stage), img_at_event(*event), view);
                        if got != img_at_want(want) {
                            ok = false;
                        }
                    }
                }
            }
        }
    }
    ok
}

/// The domain of `retry_plan`: both strategies over registry views of up to
/// four keys, every failed key, and every set of released keys (a key a
/// `terminate_child` released included), against the model's own retry: the
/// scope, the barrier and the restart order written out from the facts of the
/// keys. The result is a function of the production code alone, so it is
/// computed once per process.
fn domain_retry_plan() -> bool {
    static RESULT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *RESULT.get_or_init(retry_plan_over_its_domain)
}

fn retry_plan_over_its_domain() -> bool {
    let id = |k: Key| ActorId::from(format!("child-{k}"));
    let down = Life::Down {
        n: 1,
        pend: Pend::Nothing,
    };
    // Per key: its life and its restart type.
    let full: Vec<(Life, Restart)> = [Restart::Permanent, Restart::Temporary]
        .into_iter()
        .flat_map(|restart| {
            [
                (Life::Running(1), restart),
                (
                    Life::Stopping {
                        n: 1,
                        kind: Kind::Terminate,
                    },
                    restart,
                ),
                (Life::Restarting { prev: 1, next: 2 }, restart),
                (down, restart),
            ]
        })
        .collect();
    let slim: Vec<(Life, Restart)> = vec![
        (Life::Running(1), Restart::Permanent),
        (Life::Restarting { prev: 1, next: 2 }, Restart::Permanent),
        (down, Restart::Permanent),
        (down, Restart::Temporary),
    ];
    let mut ok = true;
    for len in 1..=4usize {
        let views = if len <= 3 { &full } else { &slim };
        let combos = views.len().pow(len as u32);
        for code in 0..combos {
            let mut rest = code;
            let members: Vec<CommitInput> = (0..len)
                .map(|k| {
                    let (life, restart) = views[rest % views.len()];
                    rest /= views.len();
                    CommitInput {
                        key: k as Key,
                        life,
                        restart,
                    }
                })
                .collect();
            let children: Vec<GroupMember> = members
                .iter()
                .map(|m| GroupMember {
                    id: id(m.key),
                    life: img_life(m.life),
                    restart_type: img_restart(m.restart),
                })
                .collect();
            for mask in 0..(1u32 << len) {
                let overrides: Vec<Key> = (0..len)
                    .filter(|k| mask & (1 << k) != 0)
                    .map(|k| k as Key)
                    .collect();
                let released: Vec<ActorId> = overrides.iter().map(|k| id(*k)).collect();
                for failed in 0..len {
                    for strategy in [RestartStrategy::OneForAll, RestartStrategy::RestForOne] {
                        let start = if strategy == RestartStrategy::RestForOne {
                            failed
                        } else {
                            0
                        };
                        let plan =
                            rules::retry_plan(strategy, &children, &released, &id(failed as Key));
                        let covered = &members[start..];
                        let mut want_order: Vec<ActorId> = Vec::new();
                        if plan.commit.len() != covered.len() {
                            ok = false;
                            continue;
                        }
                        for (entry, member) in plan.commit.iter().zip(covered) {
                            let overridden = overrides.contains(&member.key);
                            let (held, cancel, restarts) =
                                m_commit(member, failed as Key, overridden);
                            let want_token = match member.life {
                                Life::Restarting { next, .. } => u64::from(next),
                                life => u64::from(held_token(life)),
                            };
                            if entry.0 != id(member.key)
                                || entry.1 != want_token
                                || entry.2.held != held
                                || entry.2.cancel != cancel
                                || entry.2.restarts != restarts
                            {
                                ok = false;
                            }
                            if restarts {
                                want_order.push(id(member.key));
                            }
                        }
                        if plan.restart_order != want_order {
                            ok = false;
                        }
                    }
                }
            }
        }
    }
    ok
}

/// The domain of `group_member_end`: every restart type against the model's
/// rule that a `Temporary` member is removed and any other is restarted.
fn domain_group_member_end() -> bool {
    [Restart::Permanent, Restart::Transient, Restart::Temporary]
        .into_iter()
        .all(|restart| {
            let want = if restart == Restart::Temporary {
                rules::MemberEnd::PruneAndRemove
            } else {
                rules::MemberEnd::RestartInSlot
            };
            rules::group_member_end(img_restart(restart)) == want
        })
}

fn domain_group_commit() -> bool {
    let mut ok = true;
    let id = |k: Key| ActorId::from(format!("child-{k}"));
    for life in domain_lives() {
        for restart in [Restart::Permanent, Restart::Transient, Restart::Temporary] {
            for is_failed in [false, true] {
                let input = CommitInput {
                    key: 1,
                    life,
                    restart,
                };
                // Neighbours before and after, so both strategies cover the
                // member.
                let before = CommitInput {
                    key: 0,
                    life: Life::Down {
                        n: 0,
                        pend: Pend::Nothing,
                    },
                    ..input
                };
                let members = [before, input];
                let children: Vec<GroupMember> = members
                    .iter()
                    .map(|m| GroupMember {
                        id: id(m.key),
                        life: img_life(m.life),
                        restart_type: img_restart(m.restart),
                    })
                    .collect();
                let failed = if is_failed { 1 } else { 0 };
                for strategy in [RestartStrategy::OneForAll, RestartStrategy::RestForOne] {
                    let commit = rules::group_commit(strategy, &children, &id(failed));
                    let covered: &[CommitInput] =
                        if strategy == RestartStrategy::RestForOne && failed == 1 {
                            &members[1..]
                        } else {
                            &members
                        };
                    if commit.len() != covered.len() {
                        ok = false;
                        continue;
                    }
                    for (entry, member) in commit.iter().zip(covered) {
                        let (held, cancel, restarts) = m_commit(member, failed, false);
                        let token = u64::from(held_token(member.life));
                        let want_token = match member.life {
                            Life::Restarting { next, .. } => u64::from(next),
                            _ => token,
                        };
                        if entry.0 != id(member.key)
                            || entry.1 != want_token
                            || entry.2.held != held
                            || entry.2.cancel != cancel
                            || entry.2.restarts != restarts
                        {
                            ok = false;
                        }
                    }
                }
            }
        }
    }
    ok
}

/// The model's table of the stop queue's step, from what it reads of its
/// head: a head whose end was delivered, that carries the verdict, or whose
/// end a completed manual stop classified before the queue reached it is
/// passed over; a head the queue reached is waited for; an unreached head is
/// signalled by its state, a stop in progress joined and a vetoing
/// `Infinity` member asked once more if nobody asked.
fn m_stop_queue_next(i: &QueueInput) -> QueueWant {
    if !i.awaited || i.verdict || (i.handled && !i.signalled) {
        return QueueWant::Pop;
    }
    if i.signalled {
        return QueueWant::Wait;
    }
    let ask = i.ask_possible && !i.reasked;
    match i.life {
        Some(Life::Running(_)) if !i.has_order => QueueWant::Order,
        Some(Life::Running(_)) => QueueWant::Join { ask },
        Some(Life::Stopping { .. }) => QueueWant::Join { ask },
        Some(Life::Restarting { .. }) if !i.spawned => QueueWant::OrderUnpublished,
        Some(Life::Restarting { .. }) => QueueWant::OrderAttempt {
            ask: i.has_order && i.past_init && ask,
        },
        Some(Life::Down { .. }) | None => QueueWant::Await,
    }
}

fn img_queue(i: &QueueInput) -> QueueHead {
    QueueHead {
        awaited: i.awaited,
        verdict: i.verdict,
        signalled: i.signalled,
        handled: i.handled,
        life: i.life.map(img_life),
        has_order: i.has_order,
        ask_possible: i.ask_possible,
        reasked: i.reasked,
        attempt_past_init: i.past_init,
        attempt_spawned: i.spawned,
    }
}

/// Whether production's reading of the head is the model's.
fn same_head(model: &QueueHead, prod: &QueueHead) -> bool {
    model.awaited == prod.awaited
        && model.verdict == prod.verdict
        && model.signalled == prod.signalled
        && model.handled == prod.handled
        && model.life == prod.life
        && model.has_order == prod.has_order
        && model.ask_possible == prod.ask_possible
        && model.reasked == prod.reasked
        // Whether the attempt finished its initialization decides the step
        // only for an ordered attempt whose incarnation can be asked: an
        // acknowledged attempt whose outcome is still in transit has left
        // `Initializing` in production's status plane, and the model counts
        // it as past its initialization only before the hand-over.
        && (!(model.has_order && model.ask_possible)
            || model.attempt_past_init == prod.attempt_past_init)
        && model.attempt_spawned == prod.attempt_spawned
}

fn img_queue_step(want: QueueWant) -> QueueStep {
    match want {
        QueueWant::Pop => QueueStep::Pop,
        QueueWant::Wait => QueueStep::Wait,
        QueueWant::Order => QueueStep::Order,
        QueueWant::OrderAttempt { ask } => QueueStep::OrderAttempt { ask },
        QueueWant::OrderUnpublished => QueueStep::OrderUnpublished,
        QueueWant::Await => QueueStep::Await,
        QueueWant::Join { ask } => QueueStep::Join { ask },
    }
}

/// Domain conformance of `stop_queue_next`: every head state against the
/// model's table: absent, with a verdict, signalled, handled inline, and
/// unsignalled in each life with and without an order in its cell, a request
/// to veto and the record of the one more ask.
fn domain_stop_queue_next() -> bool {
    let mut lives: Vec<Option<Life>> = vec![None];
    lives.extend(domain_lives().into_iter().map(Some));
    let mut ok = true;
    for life in lives {
        for flags in 0..512u32 {
            let bit = |n: u32| flags & (1 << n) != 0;
            let input = QueueInput {
                awaited: bit(0),
                verdict: bit(1),
                signalled: bit(2),
                handled: bit(3),
                life,
                has_order: bit(4),
                ask_possible: bit(5),
                reasked: bit(6),
                past_init: bit(7),
                spawned: bit(8),
            };
            if rules::stop_queue_next(&img_queue(&input))
                != img_queue_step(m_stop_queue_next(&input))
            {
                ok = false;
            }
        }
    }
    ok
}

/// The model's teardown table, from the facts of a key.
fn m_teardown(input: &WalkInput) -> TdWant {
    match input.life {
        Some(Life::Running(_)) if !input.has_order => TdWant::Order,
        Some(Life::Running(_) | Life::Stopping { .. }) => TdWant::Join {
            ask: input.ask_possible,
        },
        Some(Life::Restarting { .. }) => TdWant::OrderAttempt {
            ask: input.has_order && input.past_init && input.ask_possible,
        },
        Some(Life::Down { .. }) | None => TdWant::Skip,
    }
}

fn domain_teardown_walk() -> bool {
    let mut lives: Vec<Option<Life>> = vec![None];
    lives.extend(domain_lives().into_iter().map(Some));
    let single = lives.iter().all(|life| {
        [false, true].into_iter().all(|has_order| {
            [false, true].into_iter().all(|ask_possible| {
                [false, true].into_iter().all(|reasked| {
                    [false, true].into_iter().all(|past_init| {
                        let input = WalkInput {
                            life: *life,
                            has_order,
                            ask_possible,
                            reasked,
                            past_init,
                        };
                        [
                            RestartStrategy::OneForOne,
                            RestartStrategy::OneForAll,
                            RestartStrategy::RestForOne,
                            RestartStrategy::SimpleOneForOne,
                        ]
                        .into_iter()
                        .all(|strategy| {
                            let walked = rules::teardown_walk(
                                strategy,
                                &[(ActorId::from("child"), img_walk(&input))],
                            );
                            walked.len() == 1 && walked[0].1 == img_td(m_teardown(&input))
                        })
                    })
                })
            })
        })
    });
    // A pool is walked in one call: every key has its action, newest first.
    let shapes: Vec<WalkInput> = [
        None,
        Some(Life::Running(1)),
        Some(Life::Stopping {
            n: 1,
            kind: Kind::Bounce,
        }),
        Some(Life::Restarting { prev: 1, next: 2 }),
        Some(Life::Down {
            n: 1,
            pend: Pend::Nothing,
        }),
    ]
    .into_iter()
    .flat_map(|life| {
        [false, true].into_iter().flat_map(move |has_order| {
            [false, true]
                .into_iter()
                .map(move |ask_possible| WalkInput {
                    life,
                    has_order,
                    ask_possible,
                    reasked: has_order,
                    past_init: ask_possible,
                })
        })
    })
    .collect();
    let mut pool = true;
    for len in 2..=3usize {
        for code in 0..shapes.len().pow(len as u32) {
            let mut rest = code;
            let inputs: Vec<WalkInput> = (0..len)
                .map(|_| {
                    let input = shapes[rest % shapes.len()];
                    rest /= shapes.len();
                    input
                })
                .collect();
            let children: Vec<(ActorId, WalkChild)> = inputs
                .iter()
                .enumerate()
                .map(|(i, input)| (ActorId::from(format!("child-{i}")), img_walk(input)))
                .collect();
            let walked = rules::teardown_walk(RestartStrategy::SimpleOneForOne, &children);
            pool &= walked.len() == len;
            for (entry, (i, input)) in walked.iter().zip(inputs.iter().enumerate().rev()) {
                pool &= entry.0 == ActorId::from(format!("child-{i}"))
                    && entry.1 == img_td(m_teardown(input));
            }
        }
    }
    single && pool
}

fn domain_absorbable() -> bool {
    let mut lives: Vec<Option<Life>> = vec![None];
    lives.extend(domain_lives().into_iter().map(Some));
    [AbPlace::Queued, AbPlace::Worklist, AbPlace::DeathPlane]
        .into_iter()
        .all(|place| {
            [false, true].into_iter().all(|in_plan| {
                lives.iter().all(|life| {
                    let owed = matches!(
                        life,
                        Some(Life::Down {
                            pend: Pend::Evaluate,
                            ..
                        })
                    );
                    let want = if !in_plan || !owed {
                        AbWant::Keep
                    } else if place == AbPlace::DeathPlane {
                        AbWant::Snapshot
                    } else {
                        AbWant::Deliver
                    };
                    rules::absorbable(img_place(place), in_plan, life.map(img_life)) == img_ab(want)
                })
            })
        })
}

fn domain_barrier_verdict() -> bool {
    let id = |k: Key| ActorId::from(format!("child-{k}"));
    let mut ok = rules::barrier_blocked_only_by_verdicts(&[]).is_none();
    for size in 1..=3usize {
        for mask in 0..(1u32 << size) {
            let flags: Vec<bool> = (0..size).map(|i| mask & (1 << i) != 0).collect();
            let entries: Vec<(ActorId, bool)> = flags
                .iter()
                .enumerate()
                .map(|(i, v)| (id(i as Key), *v))
                .collect();
            let want = flags.iter().all(|v| *v).then(|| id(0));
            if rules::barrier_blocked_only_by_verdicts(&entries) != want {
                ok = false;
            }
        }
    }
    ok
}

/// The conformance checks of one step of a run.
fn step_conformance(cfg: &Cfg, pre: &State, step: Step, ap: &Applied, out: &mut Vec<&'static str>) {
    site_ladder(cfg, pre, step, ap, out);
    site_manual(cfg, pre, step, out);
    site_answer(cfg, pre, step, ap, out);
    site_observed(ap, out);
    site_waits(cfg, pre, step, out);
    site_verdict_floor(cfg, pre, out);
    state_flags(&ap.post, out);
}

/// The conformance checks that need no run, evaluated once per search.
fn domain_conformance(out: &mut Vec<&'static str>) {
    if !domain_ladder() {
        out.push(LADDER_STEP_CONFORMANCE);
    }
    if !domain_manual() {
        out.push(MANUAL_COMMIT_CONFORMANCE);
    }
    if !domain_manual_answer() {
        out.push(MANUAL_ANSWER_CONFORMANCE);
    }
    if !domain_is_legal() {
        out.push(IS_LEGAL_CONFORMANCE);
    }
    if !domain_report_class() {
        out.push(REPORT_CLASS_CONFORMANCE);
    }
    if !domain_child_info_flags() {
        out.push(CHILD_INFO_FLAGS_CONFORMANCE);
    }
    if !domain_chain_front() {
        out.push(CHAIN_FRONT_CONFORMANCE);
    }
    if !domain_attempt_route() {
        out.push(ATTEMPT_ROUTE_CONFORMANCE);
    }
    if !domain_classify_manual() {
        out.push(CLASSIFY_MANUAL_CONFORMANCE);
    }
    if !domain_attempt_step() {
        out.push(ATTEMPT_STEP_CONFORMANCE);
    }
    if !domain_group_commit() {
        out.push(GROUP_COMMIT_CONFORMANCE);
    }
    if !domain_retry_plan() {
        out.push(RETRY_PLAN_CONFORMANCE);
    }
    if !domain_group_member_end() {
        out.push(GROUP_MEMBER_END_CONFORMANCE);
    }
    if !domain_teardown_walk() {
        out.push(TEARDOWN_WALK_CONFORMANCE);
    }
    if !domain_stop_queue_next() {
        out.push(STOP_QUEUE_NEXT_CONFORMANCE);
    }
    if !domain_absorbable() {
        out.push(ABSORBABLE_CONFORMANCE);
    }
    if !domain_barrier_verdict() {
        out.push(BARRIER_VERDICT_CONFORMANCE);
    }
    if !domain_lane_accept() {
        out.push(LANE_ACCEPT_CONFORMANCE);
    }
    if !domain_init_kill_check() {
        out.push(INIT_KILL_CHECK_CONFORMANCE);
    }
    if !domain_successor_bound() {
        out.push(SUCCESSOR_BOUND_CONFORMANCE);
    }
    if !domain_successor_wait_step() {
        out.push(SUCCESSOR_WAIT_STEP_CONFORMANCE);
    }
    if !domain_inherited_stop() {
        out.push(INHERITED_STOP_CONFORMANCE);
    }
    if !domain_verdict_floor() {
        out.push(VERDICT_FLOOR_CONFORMANCE);
    }
    domain_budgets(out);
}

// ---------------------------------------------------------------------------
// Canonical form
// ---------------------------------------------------------------------------

impl State {
    /// Every token a record names (cells and incarnations excluded).
    fn named_tokens(&self) -> TokSet {
        let mut v = TokSet::default();
        for x in self.all_sups() {
            self.sup_tokens(x, &mut v);
        }
        v.extend(self.caller.map(|c| c.t).iter());
        v.extend(self.held.values());
        v.extend(self.reaper.keys());
        v.extend(self.oracle.decided.keys().map(|k| &k.1));
        v.extend(self.oracle.pending_call.map(|p| p.t).iter());
        // A live incarnation names the supervisor incarnation that spawned
        // it, and an attempt waits on its predecessor.
        loop {
            let before = v.count();
            for (t, i) in &self.incs {
                if let (true, Some(p)) = (i.end.is_none() || v.contains(t), i.parent) {
                    v.insert(p);
                }
            }
            for (t, c) in &self.cells {
                if let (true, Some(w)) = (v.contains(t), c.waits_on) {
                    v.insert(w);
                }
                // An owed report nobody delivered stays visible, and so does
                // the supervisor incarnation that owed it.
                if let (true, SupId::Inc(p)) = (c.owed && !c.delivered && !c.excused, c.sup) {
                    v.insert(p);
                }
            }
            if v.count() == before {
                break;
            }
        }
        v
    }

    /// The supervisors whose supervision state exists.
    fn all_sups(&self) -> impl Iterator<Item = SupId> + '_ {
        std::iter::once(SupId::S).chain(self.psups.keys().map(|p| SupId::Inc(*p)))
    }

    /// Every token the records of one supervision state name.
    fn sup_tokens(&self, x: SupId, v: &mut TokSet) {
        let s = self.sup_ref(x);
        for l in s.ledger.values() {
            match l {
                Life::Running(n) | Life::Stopping { n, .. } | Life::Down { n, .. } => {
                    v.insert(*n);
                }
                Life::Restarting { prev, next } => {
                    v.insert(*prev);
                    v.insert(*next);
                }
            }
        }
        if let Some(g) = &s.group {
            v.extend(g.members.values().flatten());
            match &g.phase {
                Phase::Stop { awaiting, .. } => v.extend(awaiting.values()),
                Phase::Chain { started, .. } => v.extend(started.values()),
            }
        }
        v.extend(s.queued.iter().map(|e| e.1));
        v.extend(s.snapshots.keys().map(|k| k.1));
        v.extend(s.deaths.iter().map(|e| e.1));
        v.extend(s.outcomes.keys());
        if let Some(d) = &s.teardown {
            v.extend(d.current.map(|c| c.1));
            v.extend(d.pool_wait.iter().map(|e| e.1));
        }
        v.extend(s.final_q.iter().map(|e| e.1));
        v.extend(s.pred.values());
        v.extend(s.retired.values());
    }

    /// Drop what nothing names, then renumber tokens densely in the order a
    /// traversal of the records meets them.
    fn canonicalize(&mut self) {
        self.canonicalize_pass();
        // A canonical state is a fixed point: a second pass changes nothing.
        #[cfg(debug_assertions)]
        {
            let once = self.clone();
            self.canonicalize_pass();
            assert!(
                *self == once,
                "model invariant: canonical form is not a fixed point"
            );
        }
    }

    fn canonicalize_pass(&mut self) {
        // A reaper entry whose target ended, a decided entry whose snapshot
        // is gone, a retired record whose subtree is gone, a predecessor
        // whose subtree is gone, and the oracle's entries of a supervisor
        // incarnation whose state and task ended name nothing.
        let incs = &self.incs;
        self.reaper
            .retain(|t, _| incs.get(t).is_some_and(|i| i.end.is_none()));
        self.sweep
            .retain(|t, _| incs.get(t).is_some_and(|i| i.end.is_none()));
        self.oracle
            .sweep_leftovers
            .retain(|t| incs.get(t).is_some_and(|i| i.end.is_none()));
        let snaps = &self.sup.snapshots;
        self.oracle.decided.retain(|k, _| snaps.contains_key(k));
        self.sup.retired.retain(|_, t| !gone_in(incs, *t));
        self.sup.pred.retain(|_, t| !gone_in(incs, *t));
        for s in self.psups.values_mut() {
            s.retired.retain(|_, t| !gone_in(incs, *t));
            s.pred.retain(|_, t| !gone_in(incs, *t));
        }
        let dead: BTreeSet<Tok> = self
            .oracle
            .charges_p
            .keys()
            .chain(self.oracle.exhausted_p.iter())
            .chain(self.oracle.final_failed_p.iter())
            .chain(self.oracle.p_cause.keys())
            .chain(self.oracle.p_req.keys())
            .chain(self.oracle.p_covers.iter())
            .chain(self.oracle.last_p.keys().map(|k| &k.0))
            .copied()
            .filter(|p| {
                !self.psups.contains_key(p) && self.incs.get(p).is_none_or(|i| i.end.is_some())
            })
            .collect();
        if !dead.is_empty() {
            self.oracle.charges_p.retain(|p, _| !dead.contains(p));
            self.oracle.exhausted_p.retain(|p| !dead.contains(p));
            self.oracle.final_failed_p.retain(|p| !dead.contains(p));
            self.oracle.p_cause.retain(|p, _| !dead.contains(p));
            self.oracle.p_req.retain(|p, _| !dead.contains(p));
            self.oracle.p_covers.retain(|p| !dead.contains(p));
            self.oracle.last_p.retain(|k, _| !dead.contains(&k.0));
        }
        let named = self.named_tokens();
        self.incs
            .retain(|t, i| i.end.is_none() || named.contains(t));
        self.cells
            .retain(|t, c| named.contains(t) || (c.owed && !c.delivered && !c.excused));
        // A creation record whose subtree is gone reads as none.
        let incs = &self.incs;
        for v in self.oracle.last_s.iter_mut() {
            if !matches!(*v, 0 | 255) && gone_in(incs, *v) {
                *v = 255;
            }
        }
        for v in self.oracle.last_p.values_mut() {
            if v.is_some_and(|t| gone_in(incs, t)) {
                *v = None;
            }
        }

        let mut order: Vec<Tok> = Vec::with_capacity(24);
        for x in self.all_sups() {
            self.sup_visit_order(x, &mut order);
            if x == SupId::S {
                order.extend(self.caller.map(|c| c.t));
            }
        }
        order.extend(self.held.values());
        order.extend(self.reaper.keys());
        order.extend(self.oracle.decided.keys().map(|k| k.1));
        order.extend(self.oracle.pending_call.map(|p| p.t));
        order.extend(self.cells.keys());
        order.extend(self.incs.keys());
        let mut map = [0u8; 256];
        let mut next: u8 = 0;
        let mut identity = true;
        for t in order {
            if map[t as usize] == 0 {
                next += 1;
                map[t as usize] = next;
                identity &= t == next;
            }
        }
        if identity {
            self.next_tok = next + 1;
            return;
        }
        self.rename(&map, next);
    }

    /// The tokens one supervision state's records name, in the order the
    /// canonical numbering meets them.
    fn sup_visit_order(&self, x: SupId, out: &mut Vec<Tok>) {
        if let SupId::Inc(p) = x {
            out.push(p);
        }
        let s = self.sup_ref(x);
        for c in &s.order {
            match s.ledger.get(c) {
                Some(Life::Running(n) | Life::Stopping { n, .. } | Life::Down { n, .. }) => {
                    out.push(*n)
                }
                Some(Life::Restarting { prev, next }) => {
                    out.push(*prev);
                    out.push(*next);
                }
                None => {}
            }
            out.extend(s.pred.get(c));
        }
        if let Some(g) = &s.group {
            out.extend(g.members.values().flatten());
            match &g.phase {
                Phase::Stop { awaiting, .. } => out.extend(awaiting.values()),
                Phase::Chain { started, .. } => out.extend(started.values()),
            }
        }
        out.extend(s.queued.iter().map(|e| e.1));
        out.extend(s.snapshots.keys().map(|k| k.1));
        out.extend(s.deaths.iter().map(|e| e.1));
        out.extend(s.outcomes.keys());
        out.extend(s.retired.values());
        if let Some(d) = &s.teardown {
            out.extend(d.current.map(|c| c.1));
            out.extend(d.pool_wait.iter().map(|e| e.1));
        }
        out.extend(s.final_q.iter().map(|e| e.1));
    }

    fn rename(&mut self, m: &[u8; 256], count: u8) {
        let f = |t: Tok| -> Tok {
            let n = m[t as usize];
            assert!(n != 0, "model invariant: every token is mapped");
            n
        };
        let fx = |x: SupId| -> SupId {
            match x {
                SupId::S => SupId::S,
                SupId::Inc(p) => SupId::Inc(f(p)),
            }
        };
        rename_sup(&mut self.sup, &f);
        self.psups = std::mem::take(&mut self.psups)
            .into_iter()
            .map(|(p, mut s)| {
                rename_sup(&mut s, &f);
                (f(p), s)
            })
            .collect();
        if let Some(c) = self.caller.as_mut() {
            c.t = f(c.t);
        }
        for t in self.held.values_mut() {
            *t = f(*t);
        }
        self.reaper = std::mem::take(&mut self.reaper)
            .into_iter()
            .map(|(t, r)| (f(t), r))
            .collect();
        self.sweep = std::mem::take(&mut self.sweep)
            .into_iter()
            .map(|(t, r)| (f(t), r))
            .collect();
        self.oracle.sweep_leftovers = std::mem::take(&mut self.oracle.sweep_leftovers)
            .into_iter()
            .map(f)
            .collect();
        self.oracle.decided = std::mem::take(&mut self.oracle.decided)
            .into_iter()
            .map(|((c, t), v)| ((c, f(t)), v))
            .collect();
        self.oracle.charges_p = std::mem::take(&mut self.oracle.charges_p)
            .into_iter()
            .map(|(p, v)| (f(p), v))
            .collect();
        self.oracle.exhausted_p = std::mem::take(&mut self.oracle.exhausted_p)
            .into_iter()
            .map(f)
            .collect();
        self.oracle.final_failed_p = std::mem::take(&mut self.oracle.final_failed_p)
            .into_iter()
            .map(f)
            .collect();
        self.oracle.p_cause = std::mem::take(&mut self.oracle.p_cause)
            .into_iter()
            .map(|(p, v)| (f(p), v))
            .collect();
        self.oracle.p_req = std::mem::take(&mut self.oracle.p_req)
            .into_iter()
            .map(|(p, v)| (f(p), v))
            .collect();
        self.oracle.p_covers = std::mem::take(&mut self.oracle.p_covers)
            .into_iter()
            .map(f)
            .collect();
        for v in self.oracle.last_s.iter_mut() {
            if !matches!(*v, 0 | 255) {
                *v = f(*v);
            }
        }
        self.oracle.last_p = std::mem::take(&mut self.oracle.last_p)
            .into_iter()
            .map(|((p, c), v)| ((f(p), c), v.map(f)))
            .collect();
        if let Some(p) = self.oracle.pending_call.as_mut() {
            p.t = f(p.t);
        }
        self.cells = std::mem::take(&mut self.cells)
            .into_iter()
            .map(|(t, mut v)| {
                v.sup = fx(v.sup);
                v.waits_on = v.waits_on.map(f);
                (f(t), v)
            })
            .collect();
        self.incs = std::mem::take(&mut self.incs)
            .into_iter()
            .map(|(t, mut v)| {
                v.sup = fx(v.sup);
                v.parent = v.parent.map(f);
                (f(t), v)
            })
            .collect();
        self.next_tok = count + 1;
    }
}

/// Whether `t` and everything below it ended, read from the incarnations.
fn gone_in(incs: &BTreeMap<Tok, Inc>, t: Tok) -> bool {
    if incs.get(&t).is_some_and(|i| i.end.is_none()) {
        return false;
    }
    !incs
        .iter()
        .any(|(&u, i)| i.parent == Some(t) && !gone_in(incs, u))
}

/// A set of tokens: tokens are bytes.
#[derive(Clone, Copy, Default)]
struct TokSet([u64; 4]);

impl TokSet {
    fn insert(&mut self, t: Tok) {
        self.0[(t >> 6) as usize] |= 1 << (t & 63);
    }

    fn contains(&self, t: &Tok) -> bool {
        self.0[(*t >> 6) as usize] & (1 << (*t & 63)) != 0
    }

    fn extend<I>(&mut self, it: I)
    where
        I: IntoIterator,
        I::Item: std::borrow::Borrow<Tok>,
    {
        for t in it {
            self.insert(*std::borrow::Borrow::borrow(&t));
        }
    }

    fn count(&self) -> u32 {
        self.0.iter().map(|w| w.count_ones()).sum()
    }
}

/// Rewrite every token one supervision state names.
fn rename_sup(s: &mut Sup, f: &dyn Fn(Tok) -> Tok) {
    for l in s.ledger.values_mut() {
        *l = match *l {
            Life::Running(n) => Life::Running(f(n)),
            Life::Stopping { n, kind } => Life::Stopping { n: f(n), kind },
            Life::Restarting { prev, next } => Life::Restarting {
                prev: f(prev),
                next: f(next),
            },
            Life::Down { n, pend } => Life::Down { n: f(n), pend },
        };
    }
    if let Some(g) = s.group.as_mut() {
        for o in g.members.values_mut() {
            *o = o.map(f);
        }
        match &mut g.phase {
            Phase::Stop { awaiting, .. } => awaiting.values_mut().for_each(|t| *t = f(*t)),
            Phase::Chain { started, .. } => started.values_mut().for_each(|t| *t = f(*t)),
        }
    }
    for e in &mut s.queued {
        e.1 = f(e.1);
    }
    s.snapshots = std::mem::take(&mut s.snapshots)
        .into_iter()
        .map(|((c, t), v)| ((c, f(t)), v))
        .collect();
    s.deaths = std::mem::take(&mut s.deaths)
        .into_iter()
        .map(|(c, t, r)| (c, f(t), r))
        .collect();
    s.outcomes = std::mem::take(&mut s.outcomes)
        .into_iter()
        .map(|(t, v)| (f(t), v))
        .collect();
    if let Some(d) = s.teardown.as_mut() {
        d.current = d.current.map(|(c, t)| (c, f(t)));
        d.pool_wait = std::mem::take(&mut d.pool_wait)
            .into_iter()
            .map(|(c, t)| (c, f(t)))
            .collect();
    }
    for e in &mut s.final_q {
        e.1 = f(e.1);
    }
    for t in s.pred.values_mut() {
        *t = f(*t);
    }
    for t in s.retired.values_mut() {
        *t = f(*t);
    }
}

// ---------------------------------------------------------------------------
// Compact keys for the visited set
// ---------------------------------------------------------------------------

/// A hasher that records the bytes it is fed, so that a state's derived `Hash`
/// yields a compact exact encoding: tags and lengths are one byte each, every
/// leaf is fixed width, so equal encodings are equal states.
#[derive(Default)]
struct Sink(Vec<u8>);

impl Hasher for Sink {
    fn finish(&self) -> u64 {
        0
    }
    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
    fn write_u8(&mut self, i: u8) {
        self.0.push(i);
    }
    fn write_isize(&mut self, i: isize) {
        self.0.push(i as u8);
    }
    fn write_usize(&mut self, i: usize) {
        self.0.push(i as u8);
    }
}

fn encode<T: Hash>(v: &T) -> Box<[u8]> {
    let mut sink = Sink(Vec::with_capacity(160));
    v.hash(&mut sink);
    sink.0.into_boxed_slice()
}

/// A fast hasher for the byte keys.
#[derive(Default)]
struct Fx(u64);

impl Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        const K: u64 = 0x51_7c_c1_b7_27_22_0a_95;
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            let w = u64::from_le_bytes(c.try_into().expect("model invariant: eight bytes"));
            self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(K);
        }
        let mut tail = 0u64;
        for (i, b) in chunks.remainder().iter().enumerate() {
            tail |= u64::from(*b) << (8 * i);
        }
        self.0 = (self.0.rotate_left(5) ^ tail ^ bytes.len() as u64).wrapping_mul(K);
    }
}

type Visited = HashSet<Box<[u8]>, BuildHasherDefault<Fx>>;

// ---------------------------------------------------------------------------
// The search
// ---------------------------------------------------------------------------

struct Failure {
    check: &'static str,
    detail: String,
}

struct Outcome2 {
    /// The deepest search `every_wait_is_bounded` ran.
    deepest: usize,
    states: usize,
    quiescent: usize,
    wit: BTreeMap<Wit, u64>,
    failures: Vec<Failure>,
    limit_reached: bool,
    /// The text of a panic that ended the search: the model's own assertion
    /// on a state it cannot represent.
    panic: Option<String>,
    /// The raises the lane check compared with `lane_accept`, and the start
    /// reads the init check compared with `init_kill_check`.
    lane_sites: u64,
    init_sites: u64,
    /// The budget creations and child shapes, the wait bounds and the wait
    /// steps the subtree checks compared.
    budget_sites: u64,
    bound_sites: u64,
    wait_step_sites: u64,
    /// The waits the verdict floor check compared.
    floor_sites: u64,
    /// The manual calls the commit and the answer checks compared, and the
    /// keys the membership check compared.
    manual_sites: u64,
    answer_sites: u64,
    membership_sites: u64,
    released_after_restart: u64,
    released_spawned_since: u64,
    queue_sites: u64,
    pool_sites: u64,
    /// The guard firings the inherited stop check compared.
    guard_sites: u64,
}

fn record(failures: &mut Vec<Failure>, check: &'static str, detail: impl FnOnce() -> String) {
    if !failures.iter().any(|f| f.check == check) {
        failures.push(Failure {
            check,
            detail: detail(),
        });
    }
}

/// Witnesses that are patterns of a step or a state rather than of one
/// procedure.
fn scan_witnesses(cfg: &Cfg, pre: &State, step: Step, post: &State, wit: &mut Vec<Wit>) {
    if let Step::CallerSees = step {
        if let Some(c) = pre.caller {
            if pre.cell(c.t).exec == Exec::RunnerFailed
                && pre.incs.get(&c.t).is_some_and(|i| i.end.is_some())
            {
                wit.push(Wit::CallerWaitedForRunnerFailedEnd);
            }
        }
    }
    if let Step::StopChild(c) | Step::TerminateChild(c) = step {
        if let Some(n) = pre.row_token(c) {
            if pre
                .cells
                .get(&n)
                .is_some_and(|x| x.exec == Exec::RunnerFailed)
                && pre.incs.get(&n).is_some_and(|i| i.end.is_some())
            {
                wit.push(Wit::ManualCallOnRunnerFailedEnded);
            }
        }
    }
    if let Step::TeardownStep = step {
        if let Some(Driver {
            current: Some((_, t)),
            ..
        }) = &pre.sup.teardown
        {
            if pre.cell(*t).exec == Exec::RunnerFailed
                && pre.incs.get(t).is_some_and(|i| i.end.is_some())
            {
                wit.push(Wit::TeardownPassedRunnerFailedEnd);
            }
        }
    }
    // An order on an attempt whose incarnation already ended, exempt because
    // the handler wait it is delayed behind is exempt.
    if post.sup_alive() && post.caller.is_some() {
        for w in post.waits() {
            if !post.live(w.token()) && post.exempt_through_handler(cfg, w) {
                wit.push(Wit::ExemptThroughHandlerOnEndedAttempt);
            }
        }
    }
    // A parent stop or Kill while a handler call waits.
    if matches!(step, Step::ParentStop | Step::ParentKill) && pre.caller.is_some() {
        wit.push(Wit::ParentStopWhileHandlerWaits);
    }
    // S's orderly teardown orders a supervisor child under Kill.
    if step == Step::TeardownStep {
        let current = |s: &State| s.sup.teardown.as_ref().and_then(|d| d.current);
        if let Some((_, t)) = current(post) {
            if current(pre).map(|u| u.1) != Some(t) {
                let cell = post.cell(t);
                let kid = cfg.kid_of(cell.sup, cell.key);
                if kid.is_supervisor && kid.policy == Policy::Kill {
                    wit.push(Wit::TeardownOrdersSupervisorChildUnderKill);
                }
            }
        }
    }
    // The guards that fired in this step.
    for t in guards_fired(pre, post) {
        let cell = post.cell(t);
        let kid = cfg.kid_of(cell.sup, cell.key);
        if kid.is_supervisor && post.inc(t).lane == Some(Reason::ParentKilled) {
            wit.push(Wit::SupervisorChildUnderInheritedStop);
        }
        if !kid.is_supervisor && kid.named && pre.inc(t).stage == Stage::Turn {
            wit.push(Wit::BusyNamedWorkerAtSupervisorKill);
        }
    }
    // Attempts waiting on a subtree.
    for cell in post.cells.values() {
        if let (Exec::Attempt(AttemptStage::Wait), Some(w)) = (cell.exec, cell.waits_on) {
            if !post.gone(w) {
                wit.push(Wit::SuccessorWaitingOnLiveSubtree);
                if post.cells.get(&w).is_some_and(|c| c.exec == Exec::Returned) {
                    wit.push(Wit::FailedAttemptSubtreeWaitedFor);
                }
            }
        }
    }
    // A same-id spawn waits on a retired subtree, by the removal that retired it.
    if let (Some(k), None) = (post.caller, pre.caller) {
        if k.kind == CKind::SpawnWait {
            let p_restart = cfg.kids[k.c as usize].restart;
            if p_restart == Restart::Temporary {
                wit.push(Wit::SpawnWaitAfterPruning);
            } else if cfg.is_pool() {
                wit.push(Wit::SpawnWaitAfterPoolBounce);
            } else {
                wit.push(Wit::SpawnWaitAfterAbandonedTerminate);
            }
        }
    }
    // A Kill stored on an incarnation in its hook.
    for (&t, inc) in &pre.incs {
        if let Some(after) = post.incs.get(&t) {
            if inc.lane != Some(Reason::Kill) && after.lane == Some(Reason::Kill) {
                match inc.stage {
                    Stage::Hook => wit.push(Wit::KillDuringHook),
                    Stage::Pending | Stage::PreStart => wit.push(Wit::KillDuringPreStart),
                    _ => {}
                }
            }
        }
    }
    // A stuck member in a barrier.
    if let Some(Group {
        phase: Phase::Stop { awaiting, .. },
        ..
    }) = &post.sup.group
    {
        if awaiting
            .values()
            .any(|t| matches!(post.incs.get(t).map(|i| i.stage), Some(Stage::Stuck(_))))
        {
            wit.push(Wit::StuckMemberInBarrier);
        }
    }
    // A death report for the retired token of a restarting child.
    if post
        .sup
        .deaths
        .iter()
        .any(|d| matches!(post.life(d.0), Some(Life::Restarting { prev, .. }) if prev == d.1))
    {
        wit.push(Wit::ReportForRetiredTokenPending);
    }
    // A child spawned while a group pends joins its retry.
    if let (Some(pg), Some(g)) = (&post.sup.group, &pre.sup.group) {
        if matches!(step, Step::DrainOutcome(_) | Step::DrainDeath(..)) {
            for k in pg.members.keys() {
                if !g.members.contains_key(k)
                    && (*k as usize) >= cfg.initial
                    && !pre.oracle.removed.contains(k)
                {
                    wit.push(Wit::SpawnedDuringGroupJoinsRetry);
                    break;
                }
            }
        }
        // A terminate kept a member down through the retry.
        if matches!(step, Step::DrainOutcome(_) | Step::DrainDeath(..))
            && g.overrides.iter().any(|k| {
                g.members.contains_key(k)
                    && !pg.phase.restart_set_contains(*k)
                    && matches!(
                        pre.sup.group.as_ref().map(|x| &x.phase),
                        Some(Phase::Chain { .. })
                    )
                    && pre.oracle.group_log.contains(k)
            })
        {
            wit.push(Wit::TerminateDuringChainKeptDownThroughRetry);
        }
    }
}

/// The steps that led to a state, newest first.
struct Trail {
    step: Step,
    prev: Option<Rc<Trail>>,
}

/// The path to a failure, replayed: after every step, the live and just-ended
/// incarnations and the ledgers, for reading a first violation.
fn trail_story(cfg: &Cfg, t: &Option<Rc<Trail>>, last: Step) -> String {
    let mut steps = vec![last];
    let mut cur = t.clone();
    while let Some(n) = cur {
        steps.push(n.step);
        cur = n.prev.clone();
    }
    steps.reverse();
    let mut s = State::initial(cfg);
    s.canonicalize();
    let mut out = String::new();
    for st in steps {
        let ap = apply(cfg, &s, st);
        let mut post = ap.post;
        post.canonicalize();
        out.push_str(&format!("\n  {st:?} => {}", post.brief()));
        s = post;
    }
    out
}

impl State {
    /// A one-line summary: the incarnations by owner, then the ledgers.
    fn brief(&self) -> String {
        let who = |x: SupId| match x {
            SupId::S => "S".to_string(),
            SupId::Inc(p) => format!("p{p}"),
        };
        let mut v: Vec<String> = Vec::new();
        for (t, i) in &self.incs {
            let c = self.cells.get(t);
            v.push(format!(
                "{t}={}/{} {:?} lane={:?} end={:?} exec={:?}",
                who(i.sup),
                i.key,
                i.stage,
                i.lane,
                i.end,
                c.map(|c| c.exec)
            ));
        }
        for (t, c) in &self.cells {
            if !self.incs.contains_key(t) {
                v.push(format!(
                    "{t}={}/{} unspawned exec={:?} waits_on={:?}",
                    who(c.sup),
                    c.key,
                    c.exec,
                    c.waits_on
                ));
            }
        }
        let mut led: Vec<String> = Vec::new();
        for x in self.all_sups() {
            if self.has_sup(x) {
                led.push(format!("{}:{:?}", who(x), self.sup_ref(x).ledger));
            }
        }
        format!(
            "{} | {} | held={:?} reaper={:?} sweep={:?} caller={:?} sys={:?} phase={:?}",
            v.join("; "),
            led.join(" "),
            self.held,
            self.reaper,
            self.sweep,
            self.caller,
            self.sys,
            self.s_phase
        )
    }
}

fn trail_text(t: &Option<Rc<Trail>>, last: Step) -> String {
    let mut steps = vec![format!("{last:?}")];
    let mut cur = t.clone();
    while let Some(n) = cur {
        steps.push(format!("{:?}", n.step));
        cur = n.prev.clone();
    }
    steps.reverse();
    steps.join(" > ")
}

fn explore(cfg: &Cfg) -> Outcome2 {
    // A third-party stop of a supervisor child has no row in the table of
    // exit reasons, and no configuration needs one: it is not built.
    assert!(
        !(cfg.budgets.third > 0 && cfg.kids.iter().any(|k| k.is_supervisor)),
        "unsupported configuration {}: a third-party raise (third > 0) with a supervisor child",
        cfg.name
    );
    let mut out = Outcome2 {
        deepest: 0,
        states: 1,
        quiescent: 0,
        wit: BTreeMap::new(),
        failures: Vec::new(),
        limit_reached: false,
        panic: None,
        lane_sites: 0,
        init_sites: 0,
        budget_sites: 0,
        bound_sites: 0,
        wait_step_sites: 0,
        floor_sites: 0,
        manual_sites: 0,
        answer_sites: 0,
        membership_sites: 0,
        released_after_restart: 0,
        released_spawned_since: 0,
        queue_sites: 0,
        pool_sites: 0,
        guard_sites: 0,
    };
    GUARD_SITES.with(|c| c.set(0));
    LANE_SITES.with(|c| c.set(0));
    INIT_SITES.with(|c| c.set(0));
    BUDGET_SITES.with(|c| c.set(0));
    BOUND_SITES.with(|c| c.set(0));
    WAIT_STEP_SITES.with(|c| c.set(0));
    FLOOR_SITES.with(|c| c.set(0));
    MANUAL_SITES.with(|c| c.set(0));
    ANSWER_SITES.with(|c| c.set(0));
    MEMBERSHIP_SITES.with(|c| c.set(0));
    RELEASED_AFTER_RESTART.with(|c| c.set(0));
    RELEASED_SPAWNED_SINCE.with(|c| c.set(0));
    QUEUE_SITES.with(|c| c.set(0));
    POOL_SITES.with(|c| c.set(0));
    let mut conformance: Vec<&'static str> = Vec::new();
    domain_conformance(&mut conformance);
    site_budgets(cfg, &mut conformance);
    for name in conformance {
        record(&mut out.failures, name, || {
            "the production function differs from the model's function over its domain".to_string()
        });
    }
    // The failures found before an assertion of the model itself fires are
    // kept and reported with it.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| search(cfg, &mut out)));
    if let Err(payload) = result {
        let text = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|t| (*t).to_string()))
            .unwrap_or_else(|| "a panic without text".to_string());
        out.panic = Some(text);
    }
    out.lane_sites = LANE_SITES.with(std::cell::Cell::get);
    out.init_sites = INIT_SITES.with(std::cell::Cell::get);
    out.budget_sites = BUDGET_SITES.with(std::cell::Cell::get);
    out.bound_sites = BOUND_SITES.with(std::cell::Cell::get);
    out.wait_step_sites = WAIT_STEP_SITES.with(std::cell::Cell::get);
    out.floor_sites = FLOOR_SITES.with(std::cell::Cell::get);
    out.manual_sites = MANUAL_SITES.with(std::cell::Cell::get);
    out.answer_sites = ANSWER_SITES.with(std::cell::Cell::get);
    out.membership_sites = MEMBERSHIP_SITES.with(std::cell::Cell::get);
    out.released_after_restart = RELEASED_AFTER_RESTART.with(std::cell::Cell::get);
    out.released_spawned_since = RELEASED_SPAWNED_SINCE.with(std::cell::Cell::get);
    out.queue_sites = QUEUE_SITES.with(std::cell::Cell::get);
    out.pool_sites = POOL_SITES.with(std::cell::Cell::get);
    out.guard_sites = GUARD_SITES.with(std::cell::Cell::get);
    out
}

fn search(cfg: &Cfg, out: &mut Outcome2) {
    let mut init = State::initial(cfg);
    init.canonicalize();
    let mut seen: Visited = Visited::default();
    seen.insert(encode(&init));
    let init_steps = init.enabled(cfg);
    let mut stack: Vec<(State, Vec<Step>, Option<Rc<Trail>>)> = vec![(init, init_steps, None)];
    while let Some((s, steps, trail)) = stack.pop() {
        for step in steps {
            let ap = apply(cfg, &s, step);
            let mut wit = ap.wit.clone();
            let mut fails: Vec<&'static str> = ap.fails.clone();
            step_checks(cfg, &s, step, &ap, &mut fails);
            scan_witnesses(cfg, &s, step, &ap.post, &mut wit);
            for w in wit {
                *out.wit.entry(w).or_insert(0) += 1;
            }
            let step_failed = !fails.is_empty();
            for name in fails {
                record(&mut out.failures, name, || {
                    format!(
                        "history {} ; story:{} ; from {s:?}",
                        trail_text(&trail, step),
                        trail_story(cfg, &trail, step)
                    )
                });
            }
            // Conformance of the production decisions is recorded without
            // stopping the search from expanding the state.
            let mut conformance: Vec<&'static str> = Vec::new();
            step_conformance(cfg, &s, step, &ap, &mut conformance);
            for name in conformance {
                record(&mut out.failures, name, || {
                    format!(
                        "history {} ; story:{} ; from {s:?}",
                        trail_text(&trail, step),
                        trail_story(cfg, &trail, step)
                    )
                });
            }
            let mut post = ap.post;
            post.canonicalize();
            // A state reached by a failing step is checked but never expanded
            // nor marked visited, so a clean arrival still expands it.
            if !step_failed && !seen.insert(encode(&post)) {
                continue;
            }
            // The membership check reads the state alone, so it runs once per
            // state the search reaches, not once per transition into it.
            let mut membership: Vec<&'static str> = Vec::new();
            site_membership(cfg, &post, &mut membership);
            for name in membership {
                record(&mut out.failures, name, || {
                    format!(
                        "history {} ; story:{} ; membership of {post:?}",
                        trail_text(&trail, step),
                        trail_story(cfg, &trail, step)
                    )
                });
            }
            let next_steps = post.enabled(cfg);
            let quiescent = !next_steps.iter().any(|st| st.is_internal());
            let mut sf: Vec<&'static str> = Vec::new();
            post.state_checks(cfg, &mut sf, &mut out.deepest);
            if quiescent {
                post.quiescent_checks(cfg, &mut sf);
            }
            let state_failed = !sf.is_empty();
            for name in sf {
                record(&mut out.failures, name, || {
                    format!(
                        "history {} ; story:{} ; state {post:?}",
                        trail_text(&trail, step),
                        trail_story(cfg, &trail, step)
                    )
                });
            }
            if step_failed || state_failed {
                continue;
            }
            out.states += 1;
            if quiescent {
                out.quiescent += 1;
            }
            if out.states > cfg.limit {
                out.limit_reached = true;
                return;
            }
            stack.push((
                post,
                next_steps,
                Some(Rc::new(Trail {
                    step,
                    prev: trail.clone(),
                })),
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Configurations
// ---------------------------------------------------------------------------

const SMALL_LIMIT: usize = 1_000_000;
const FULL_LIMIT: usize = 20_000_000;

fn base(name: &'static str, strategy: Strategy, kids: Vec<ChildCfg>, initial: usize) -> Cfg {
    Cfg {
        name,
        strategy,
        budget_s: 2,
        kids,
        initial,
        budgets: Budgets::default(),
        third_reasons: Vec::new(),
        limit: SMALL_LIMIT,
        workers: Vec::new(),
        budget_p: 1,
        bare_kill_on: None,
        mark_override_asks: false,
    }
}

const PLAIN: ChildCfg = ChildCfg::new();
const VETO: ChildCfg = ChildCfg::new().behavior(Behavior::Vetoing);

/// A copy of `base` with its step budgets set by `f`.
fn with(base: &Cfg, f: impl FnOnce(&mut Budgets)) -> Cfg {
    let mut c = base.clone();
    f(&mut c.budgets);
    c
}

fn small_one_for_one_cfgs() -> Vec<Cfg> {
    let c = base(
        "small_one_for_one",
        Strategy::OneForOne,
        vec![PLAIN, VETO],
        2,
    );
    vec![
        with(&c, |b| {
            b.crash_s = 1;
            b.manual = 1;
            b.parent = 1;
            b.final_fail = 1;
        }),
        with(&c, |b| b.manual = 2),
        with(&c, |b| {
            b.foreign = 1;
            b.crash_s = 1;
            b.manual = 1;
        }),
    ]
}

/// A key registered outside the supervisor: a spawn of the key is refused
/// while a restart of the other child goes on.
fn small_foreign_key_cfgs() -> Vec<Cfg> {
    let c = base(
        "small_foreign_key",
        Strategy::OneForOne,
        vec![PLAIN, PLAIN],
        1,
    );
    vec![with(&c, |b| {
        b.foreign = 1;
        b.new_spawn = 1;
    })]
}

/// A child whose restart is `Transient` among restartable group members:
/// the manual calls on it and the group's restart meet its exemptions.
fn small_non_permanent_cfgs() -> Vec<Cfg> {
    let c = base(
        "small_non_permanent",
        Strategy::OneForAll,
        vec![PLAIN.restart(Restart::Transient), PLAIN],
        2,
    );
    vec![with(&c, |b| {
        b.crash_s = 1;
        b.manual = 2;
    })]
}

fn small_one_for_all_cfgs() -> Vec<Cfg> {
    let c = base(
        "small_one_for_all",
        Strategy::OneForAll,
        vec![PLAIN, VETO],
        2,
    );
    vec![
        with(&c, |b| {
            b.crash_s = 1;
            b.manual = 1;
            b.parent = 1;
        }),
        with(&c, |b| {
            b.crash_s = 1;
            b.manual = 2;
        }),
    ]
}

fn small_rest_for_one_cfgs() -> Vec<Cfg> {
    let c = base(
        "small_rest_for_one",
        Strategy::RestForOne,
        vec![PLAIN, VETO, PLAIN],
        2,
    );
    vec![
        with(&c, |b| {
            b.crash_s = 1;
            b.manual = 1;
            b.parent = 1;
            b.init_fail = 1;
        }),
        with(&c, |b| {
            b.crash_s = 1;
            b.new_spawn = 1;
            b.init_fail = 1;
        }),
        with(&c, |b| {
            b.crash_s = 1;
            b.manual = 2;
            b.init_fail = 1;
        }),
        with(&c, |b| {
            b.crash_s = 1;
            b.manual = 1;
            b.new_spawn = 1;
        }),
    ]
}

fn small_pool_cfgs() -> Vec<Cfg> {
    let c = base("small_pool", Strategy::Pool, vec![PLAIN, VETO], 1);
    vec![with(&c, |b| {
        b.crash_s = 1;
        b.manual = 2;
        b.parent_stop = 1;
        b.new_spawn = 1;
    })]
}

fn small_three_member_group_cfgs() -> Vec<Cfg> {
    let c = base(
        "small_three_member_group",
        Strategy::OneForAll,
        vec![PLAIN, PLAIN, PLAIN.restart(Restart::Temporary)],
        3,
    );
    vec![with(&c, |b| b.crash_s = 2)]
}

fn small_retry_cfgs() -> Vec<Cfg> {
    let c = base(
        "small_retry",
        Strategy::OneForAll,
        vec![PLAIN, PLAIN, PLAIN],
        2,
    );
    vec![
        with(&c, |b| {
            b.crash_s = 2;
            b.init_fail = 1;
        }),
        with(&c, |b| {
            b.crash_s = 1;
            b.manual = 1;
            b.init_fail = 1;
        }),
        with(&c, |b| {
            b.crash_s = 1;
            b.manual = 1;
            b.new_spawn = 1;
        }),
        with(&c, |b| {
            b.crash_s = 2;
            b.manual = 1;
        }),
        // A child spawned while the group of the one initial child is
        // pending, then bounced by a manual call, so its attempt is in
        // flight when the chain's start fails: the retry commits it.
        with(
            &base("small_retry", Strategy::OneForAll, vec![PLAIN, PLAIN], 1),
            |b| {
                b.crash_s = 1;
                b.new_spawn = 1;
                b.manual = 1;
                b.init_fail = 1;
            },
        ),
        // A `Transient` child started while the group of the one initial
        // child pends: its stop is its terminate, kept down through the
        // retry the chain's failed start lays out.
        with(
            &base(
                "small_retry",
                Strategy::OneForAll,
                vec![PLAIN, PLAIN.restart(Restart::Transient)],
                1,
            ),
            |b| {
                b.crash_s = 1;
                b.init_fail = 1;
                b.new_spawn = 1;
                b.manual = 1;
            },
        ),
    ]
}

fn small_lane_cfgs() -> Vec<Cfg> {
    let child_a = ChildCfg::new().behavior(Behavior::ParentOnly);
    let mut a = base("small_lane", Strategy::OneForOne, vec![child_a], 1);
    a.budgets = Budgets {
        third: 1,
        parent_stop: 1,
        ..Budgets::default()
    };
    a.third_reasons = vec![Reason::Graceful];
    let mut b = a.clone();
    b.kids = vec![child_a.policy(Policy::Infinity)];
    let mut c = base("small_lane", Strategy::OneForOne, vec![PLAIN.turns()], 1);
    c.budgets = Budgets {
        third: 2,
        ..Budgets::default()
    };
    c.third_reasons = vec![
        Reason::Graceful,
        Reason::Failure(Cause::Crash),
        Reason::ParentKilled,
        Reason::Cancelled,
    ];
    vec![a, b, c]
}

/// A `Transient` child stopped from above by a party other than its
/// supervisor, with every reason a third party can use: it stays down after
/// an inherited stop and restarts after a crash.
fn small_inherited_transient_cfgs() -> Vec<Cfg> {
    let mut c = base(
        "small_inherited_transient",
        Strategy::OneForOne,
        vec![PLAIN.turns().restart(Restart::Transient)],
        1,
    );
    c.budgets = Budgets {
        third: 1,
        ..Budgets::default()
    };
    c.third_reasons = vec![
        Reason::Graceful,
        Reason::Failure(Cause::Crash),
        Reason::ParentKilled,
        Reason::Cancelled,
    ];
    vec![c]
}

fn small_unresponsive_cfgs() -> Vec<Cfg> {
    let c = base(
        "small_unresponsive",
        Strategy::OneForAll,
        vec![PLAIN.turns().sticks(), PLAIN],
        2,
    );
    vec![
        with(&c, |b| {
            b.crash_s = 1;
            b.parent = 1;
            b.stuck = 1;
            b.runner_fail = 1;
        }),
        with(&c, |b| {
            b.manual = 1;
            b.stuck = 1;
            b.runner_fail = 1;
        }),
        // A second manual call joins a stop whose occupant carries the
        // verdict.
        with(&c, |b| {
            b.manual = 2;
            b.stuck = 1;
        }),
    ]
}

fn small_kill_cfgs() -> Vec<Cfg> {
    let c = base(
        "small_kill",
        Strategy::OneForOne,
        vec![VETO.turns(), PLAIN],
        2,
    );
    let a = with(&c, |b| {
        b.parent_stop = 1;
        b.parent_kill = 1;
        b.bare_kill = 1;
    });
    let mut a_zero = with(&a, |b| b.manual = 1);
    a_zero.kids = vec![VETO.turns().grace(Grace::Zero), PLAIN];
    vec![
        a,
        a_zero,
        with(&c, |b| {
            b.manual = 1;
            b.parent_stop = 1;
            b.parent_kill = 1;
        }),
    ]
}

fn small_attempts_cfgs() -> Vec<Cfg> {
    let mut a = base(
        "small_attempts",
        Strategy::OneForOne,
        vec![PLAIN.sticks().start_timeout()],
        1,
    );
    a.budgets = Budgets {
        crash_s: 1,
        manual: 1,
        parent_stop: 1,
        bare_kill: 1,
        stuck: 1,
        ..Budgets::default()
    };
    let mut b = a.clone();
    b.kids = vec![PLAIN.sticks()];
    vec![a, b]
}

fn small_runner_failure_cfgs() -> Vec<Cfg> {
    let c = base(
        "small_runner_failure",
        Strategy::OneForOne,
        vec![PLAIN, VETO],
        2,
    );
    vec![
        with(&c, |b| {
            b.crash_s = 1;
            b.parent = 1;
            b.runner_fail = 1;
        }),
        with(&c, |b| {
            b.manual = 1;
            b.parent = 1;
            b.runner_fail = 1;
        }),
    ]
}

fn small_manual_restart_cfgs() -> Vec<Cfg> {
    let mut c = base("small_manual_restart", Strategy::OneForOne, vec![PLAIN], 1);
    c.budget_s = 1;
    c.budgets = Budgets {
        manual: 2,
        init_fail: 1,
        crash_s: 1,
        ..Budgets::default()
    };
    vec![c]
}

fn small_override_one_more_ask_cfgs() -> Vec<Cfg> {
    let veto_once = ChildCfg::new()
        .behavior(Behavior::VetoOnce)
        .policy(Policy::Infinity);
    let veto_inf = ChildCfg::new()
        .behavior(Behavior::Vetoing)
        .policy(Policy::Infinity);
    let quick = PLAIN.policy(Policy::Kill);
    let once = base(
        "small_override_one_more_ask",
        Strategy::OneForAll,
        vec![quick, veto_once, quick],
        3,
    );
    let vetoing = base(
        "small_override_one_more_ask",
        Strategy::OneForAll,
        vec![quick, veto_inf, quick],
        3,
    );
    // The first child keeps its default window where the run is small enough.
    let once_a = base(
        "small_override_one_more_ask",
        Strategy::OneForAll,
        vec![PLAIN, veto_once, quick],
        3,
    );
    let vetoing_a = base(
        "small_override_one_more_ask",
        Strategy::OneForAll,
        vec![PLAIN, veto_inf, quick],
        3,
    );
    let pair = base(
        "small_override_one_more_ask",
        Strategy::OneForAll,
        vec![PLAIN, veto_inf],
        2,
    );
    let mut cfgs = vec![
        // The group's queue has not reached the member an override stops.
        with(&once, |b| {
            b.crash_s = 1;
            b.manual = 2;
        }),
        // A second manual stop joins an abandoned one.
        with(&once_a, |b| b.manual = 2),
        // The teardown joins a member the group's queue ordered.
        with(&vetoing_a, |b| {
            b.crash_s = 1;
            b.parent_stop = 1;
        }),
        // The teardown joins a member the group and an override both asked.
        with(&once_a, |b| {
            b.crash_s = 1;
            b.manual = 1;
            b.parent_stop = 1;
        }),
        // Two terminate calls during one group share one ask.
        with(&vetoing, |b| {
            b.crash_s = 1;
            b.manual = 2;
        }),
        // The teardown joins an ordered restart attempt that finished its init.
        with(&pair, |b| {
            b.crash_s = 2;
            b.manual = 2;
            b.parent_stop = 1;
        }),
    ];
    // The witness of a member accepting after an ask counts the asks that an
    // override gave.
    for cfg in &mut cfgs {
        cfg.mark_override_asks = true;
    }
    cfgs
}

/// The supervisor under test with a supervisor child `P` (key 0) and, when
/// `with_x`, a plain child `x` (key 1); every incarnation of `P` supervises
/// the one named worker `w0`.
fn tree_base(
    name: &'static str,
    strategy: Strategy,
    p: ChildCfg,
    w0: ChildCfg,
    with_x: bool,
) -> Cfg {
    let kids = if with_x {
        vec![p.supervisor(), PLAIN]
    } else {
        vec![p.supervisor()]
    };
    let initial = kids.len();
    let mut c = base(name, strategy, kids, initial);
    c.workers = vec![w0];
    c.bare_kill_on = Some(vec![0]);
    c
}

fn small_tree_kill_paths_cfgs() -> Vec<Cfg> {
    let w0 = PLAIN.turns();
    let a = tree_base(
        "small_tree_kill_paths",
        Strategy::OneForOne,
        PLAIN,
        w0,
        true,
    );
    let b = tree_base(
        "small_tree_kill_paths",
        Strategy::OneForAll,
        PLAIN.policy(Policy::Kill),
        w0,
        true,
    );
    let c = tree_base(
        "small_tree_kill_paths",
        Strategy::OneForAll,
        PLAIN.behavior(Behavior::Vetoing),
        w0,
        true,
    );
    let d = tree_base(
        "small_tree_kill_paths",
        Strategy::OneForOne,
        PLAIN.policy(Policy::Kill),
        w0,
        true,
    );
    vec![
        with(&a, |b| {
            b.bare_kill = 1;
            b.parent_stop = 1;
        }),
        with(&b, |b| b.crash_s = 1),
        with(&c, |b| b.crash_s = 1),
        with(&d, |b| b.manual = 1),
    ]
}

/// A group evaluation that meets a restart attempt still waiting for its
/// predecessor's subtree: a supervisor child `P` is stopped by hand while its
/// worker never yields, so `P`'s restart waits, and a sibling's crash then
/// evaluates the group.
fn small_tree_group_wait_cfgs() -> Vec<Cfg> {
    let w0 = PLAIN.turns().sticks();
    let p = PLAIN.window_ms(100);
    let c = tree_base("small_tree_group_wait", Strategy::OneForAll, p, w0, true);
    vec![with(&c, |b| {
        b.manual = 1;
        b.crash_s = 1;
    })]
}

/// OneForAll `[x, P]` with only x started: P, a supervisor child whose
/// worker takes turns, is spawned while x's group pends and then bounced, so
/// its attempt waits for the subtree its first incarnation left; x's chain
/// start then fails and the retry commits the waiting attempt. x and P stop
/// under `Kill`, which keeps the configuration within its share.
fn small_tree_retry_wait_cfgs() -> Vec<Cfg> {
    let w0 = PLAIN.turns();
    let killed = PLAIN.policy(Policy::Kill);
    let mut c = base(
        "small_tree_retry_wait",
        Strategy::OneForAll,
        vec![killed, killed.supervisor()],
        1,
    );
    c.workers = vec![w0];
    c.bare_kill_on = Some(vec![1]);
    vec![with(&c, |b| {
        b.crash_s = 1;
        b.new_spawn = 1;
        b.manual = 1;
        b.init_fail = 1;
    })]
}

/// A bare `Kill` on a supervisor child, then its supervisor killed: the guard
/// that fires finds the `Kill` on the lane and keeps the abort the bare
/// `Kill` armed for itself, under `Timeout` (run 1) and `Infinity` (run 2).
fn small_tree_stored_kill_cfgs() -> Vec<Cfg> {
    let w0 = PLAIN.window_ms(2000);
    let p = PLAIN.window_ms(3000);
    let a = tree_base("small_tree_stored_kill", Strategy::OneForOne, p, w0, false);
    let b = tree_base(
        "small_tree_stored_kill",
        Strategy::OneForOne,
        p.policy(Policy::Infinity),
        w0,
        false,
    );
    vec![
        with(&a, |b| {
            b.bare_kill = 1;
            b.parent_kill = 1;
        }),
        with(&b, |b| {
            b.bare_kill = 1;
            b.parent_kill = 1;
        }),
    ]
}

/// A supervisor child with a start timeout crashes; the start timeout of its
/// restart attempt raises `Kill` on the new incarnation, which arms no reaper
/// entry (the attempt runs its own grace and abort), and then its supervisor
/// is killed: the guard that fires finds that `Kill` on the lane and arms the
/// abort it is owed, under `Infinity` (run 1), where an orderly inherited stop
/// would arm none, and under `Timeout` (run 2).
fn small_tree_stored_ladder_kill_cfgs() -> Vec<Cfg> {
    let w0 = PLAIN.window_ms(2000);
    let p = PLAIN.start_timeout();
    let a = tree_base(
        "small_tree_stored_ladder_kill",
        Strategy::OneForOne,
        p.policy(Policy::Infinity),
        w0,
        false,
    );
    let b = tree_base(
        "small_tree_stored_ladder_kill",
        Strategy::OneForOne,
        p.window_ms(3000),
        w0,
        false,
    );
    vec![
        with(&a, |b| {
            b.crash_s = 1;
            b.parent_kill = 1;
        }),
        with(&b, |b| {
            b.crash_s = 1;
            b.parent_kill = 1;
        }),
    ]
}

fn small_tree_parent_killed_cfgs() -> Vec<Cfg> {
    let w0 = PLAIN.window_ms(2000);
    let p = PLAIN.window_ms(3000);
    let a = tree_base(
        "small_tree_parent_killed",
        Strategy::OneForOne,
        p,
        w0,
        false,
    );
    let b = tree_base(
        "small_tree_parent_killed",
        Strategy::OneForOne,
        p.grace(Grace::Zero),
        w0,
        false,
    );
    vec![
        with(&a, |b| b.parent_kill = 1),
        with(&b, |b| b.parent_kill = 1),
    ]
}

fn small_tree_pairing_cfgs() -> Vec<Cfg> {
    let w0 = PLAIN.behavior(Behavior::Vetoing).window_ms(2000);
    let mk = |p: ChildCfg, w: ChildCfg| {
        with(
            &tree_base("small_tree_pairing", Strategy::OneForOne, p, w, false),
            |b| b.parent = 1,
        )
    };
    vec![
        mk(PLAIN.window_ms(3000), w0),
        mk(PLAIN.window_ms(1000), w0),
        mk(PLAIN.window_ms(2300), w0.grace(Grace::Ns(1_000_000_000))),
    ]
}

fn small_tree_stuck_grandchild_cfgs() -> Vec<Cfg> {
    let w0 = PLAIN.turns().sticks();
    let c = tree_base(
        "small_tree_stuck_grandchild",
        Strategy::OneForOne,
        PLAIN.policy(Policy::Kill),
        w0,
        false,
    );
    // The second run adds a sibling that can die while the wait runs, so the
    // expiry is explored beside a death still pending. The third stops the
    // parent while the attempt waits on the stuck subtree, so an order meets
    // a wait whose bound can expire.
    let beside_a_sibling = tree_base(
        "small_tree_stuck_grandchild",
        Strategy::OneForOne,
        PLAIN.policy(Policy::Kill),
        w0,
        true,
    );
    vec![
        with(&c, |b| {
            b.bare_kill = 1;
            b.stuck = 1;
        }),
        with(&beside_a_sibling, |b| {
            b.bare_kill = 1;
            b.stuck = 1;
            b.crash_s = 1;
        }),
        with(&c, |b| {
            b.bare_kill = 1;
            b.stuck = 1;
            b.parent_stop = 1;
        }),
    ]
}

fn small_tree_infinity_cfgs() -> Vec<Cfg> {
    let a = tree_base(
        "small_tree_infinity",
        Strategy::OneForOne,
        PLAIN,
        PLAIN.policy(Policy::Infinity).behavior(Behavior::Vetoing),
        false,
    );
    let b = tree_base(
        "small_tree_infinity",
        Strategy::OneForOne,
        PLAIN.policy(Policy::Infinity),
        PLAIN.window_ms(2000),
        false,
    );
    // An Infinity worker that never yields: the successor's wait on it is
    // bounded by its kill grace, not by its policy.
    let c = tree_base(
        "small_tree_infinity",
        Strategy::OneForOne,
        PLAIN,
        PLAIN
            .policy(Policy::Infinity)
            .behavior(Behavior::Vetoing)
            .turns()
            .sticks(),
        false,
    );
    vec![
        with(&a, |b| b.bare_kill = 1),
        with(&b, |b| b.parent_kill = 1),
        with(&c, |b| {
            b.bare_kill = 1;
            b.stuck = 1;
        }),
    ]
}

fn small_tree_delete_respawn_cfgs() -> Vec<Cfg> {
    let w0 = PLAIN.turns().sticks();
    let p = PLAIN.policy(Policy::Kill);
    let a = tree_base(
        "small_tree_delete_respawn",
        Strategy::OneForOne,
        p,
        w0,
        false,
    );
    let b = tree_base(
        "small_tree_delete_respawn",
        Strategy::OneForOne,
        p.restart(Restart::Temporary),
        w0,
        false,
    );
    let c = tree_base(
        "small_tree_delete_respawn",
        Strategy::Pool,
        p.restart(Restart::Temporary),
        w0,
        false,
    );
    let d = tree_base("small_tree_delete_respawn", Strategy::Pool, p, w0, false);
    vec![
        with(&a, |b| {
            b.manual = 3;
            b.stuck = 1;
        }),
        with(&b, |b| {
            b.bare_kill = 1;
            b.manual = 1;
        }),
        with(&c, |b| {
            b.bare_kill = 1;
            b.manual = 1;
        }),
        with(&d, |b| b.manual = 2),
    ]
}

fn small_tree_failed_attempt_subtree_cfgs() -> Vec<Cfg> {
    let mut c = tree_base(
        "small_tree_failed_attempt_subtree",
        Strategy::OneForOne,
        PLAIN.start_timeout(),
        PLAIN.turns(),
        false,
    );
    c.budget_s = 3;
    vec![with(&c, |b| {
        b.crash_s = 1;
        b.init_fail = 1;
    })]
}

/// A worker that crashes more often than the supervisor incarnation's
/// restart budget allows: the supervisor incarnation stops itself, and the
/// supervisor under test restarts it.
fn small_tree_worker_budget_cfgs() -> Vec<Cfg> {
    let c = tree_base(
        "small_tree_worker_budget",
        Strategy::OneForOne,
        PLAIN,
        PLAIN.turns(),
        false,
    );
    // Two workers: a report of the second is still pending when the first
    // exhausts the budget, so the supervisor incarnation's final drain has
    // something to collect.
    let mut d = tree_base(
        "small_tree_worker_budget",
        Strategy::OneForOne,
        PLAIN,
        PLAIN.turns(),
        false,
    );
    d.workers.push(PLAIN.turns());
    vec![with(&c, |b| b.crash_w = 2), with(&d, |b| b.crash_w = 3)]
}

/// A stuck worker whose runner failed, under a supervisor incarnation whose
/// own supervisor was killed: the verdict is given by the worker's supervisor.
fn small_tree_lost_runner_cfgs() -> Vec<Cfg> {
    let c = tree_base(
        "small_tree_lost_runner",
        Strategy::OneForOne,
        PLAIN.window_ms(6000),
        PLAIN.turns().sticks(),
        false,
    );
    vec![with(&c, |b| {
        b.parent_kill = 1;
        b.runner_fail = 1;
        b.stuck = 1;
    })]
}

/// A manual call on the supervisor child together with a parent stop or Kill
/// of the supervisor above it.
fn small_tree_manual_parent_cfgs() -> Vec<Cfg> {
    let w0 = PLAIN.turns();
    let a = tree_base(
        "small_tree_manual_parent",
        Strategy::OneForOne,
        PLAIN,
        w0,
        false,
    );
    let b = tree_base(
        "small_tree_manual_parent",
        Strategy::OneForOne,
        PLAIN.policy(Policy::Kill),
        w0,
        false,
    );
    vec![
        with(&a, |b| {
            b.manual = 1;
            b.parent = 1;
        }),
        with(&b, |b| {
            b.manual = 1;
            b.parent = 1;
        }),
    ]
}

fn small_system_shutdown_cfgs() -> Vec<Cfg> {
    let c = base(
        "small_system_shutdown",
        Strategy::OneForOne,
        vec![PLAIN.turns()],
        1,
    );
    let t = tree_base(
        "small_system_shutdown",
        Strategy::OneForOne,
        PLAIN,
        PLAIN.turns(),
        false,
    );
    vec![
        with(&c, |b| {
            b.crash_s = 1;
            b.shut_begin = 1;
            b.shut_abandon = 1;
            b.shut_later = 1;
            b.shut_reactivate = 1;
        }),
        with(&t, |b| {
            b.crash_w = 1;
            b.shut_begin = 1;
            b.shut_abandon = 1;
            b.shut_later = 1;
            b.shut_reactivate = 1;
        }),
    ]
}

/// A shutdown whose call is abandoned after its final sweep took its
/// snapshot: S is killed by the shutdown, its `Infinity` supervisor child `P`
/// gets an orderly stop and no reaper entry of its guard, and `P`'s
/// `Infinity` worker vetoes every request, so only the sweep's deadline
/// entries end the subtree once nobody drives the shutdown.
fn small_shutdown_abandoned_sweep_cfgs() -> Vec<Cfg> {
    let c = tree_base(
        "small_shutdown_abandoned_sweep",
        Strategy::OneForOne,
        PLAIN.policy(Policy::Infinity),
        PLAIN.policy(Policy::Infinity).behavior(Behavior::Vetoing),
        false,
    );
    vec![with(&c, |b| {
        b.shut_begin = 1;
        b.shut_abandon = 1;
    })]
}

fn full_tree_base() -> Cfg {
    let mut c = tree_base("full_tree", Strategy::OneForOne, PLAIN, PLAIN.turns(), true);
    c.workers.push(PLAIN.behavior(Behavior::Vetoing));
    c.budget_p = 2;
    c.limit = FULL_LIMIT;
    c
}

/// A crash at each level, a Kill path and a parent stop or Kill, in the
/// combinations the budget holds.
fn full_tree_cfgs() -> Vec<Cfg> {
    let c = full_tree_base();
    vec![
        with(&c, |b| {
            b.bare_kill = 1;
            b.crash_s = 1;
            b.crash_w = 1;
        }),
        with(&c, |b| {
            b.bare_kill = 1;
            b.crash_w = 1;
            b.parent = 1;
        }),
        with(&c, |b| {
            b.crash_s = 1;
            b.crash_w = 1;
            b.parent = 1;
        }),
        // The manual call on the supervisor child together with a parent stop
        // or Kill.
        with(&c, |b| {
            b.manual = 1;
            b.parent = 1;
        }),
    ]
}

/// The manual call with a worker crash and a Kill path or an S crash: the
/// manual call together with a crash or a Kill path and a parent stop or
/// Kill is out of reach.
fn full_tree_manual_cfgs() -> Vec<Cfg> {
    let mut c = full_tree_base();
    c.name = "full_tree_manual";
    vec![
        with(&c, |b| {
            b.bare_kill = 1;
            b.crash_w = 1;
            b.manual = 1;
        }),
        with(&c, |b| {
            b.crash_s = 1;
            b.crash_w = 1;
            b.manual = 1;
        }),
    ]
}

fn full_children() -> Vec<ChildCfg> {
    vec![
        PLAIN.start_timeout(),
        ChildCfg::new()
            .restart(Restart::Transient)
            .behavior(Behavior::Vetoing)
            .policy(Policy::Infinity)
            .turns(),
        ChildCfg::new()
            .restart(Restart::Temporary)
            .behavior(Behavior::VetoOnce)
            .policy(Policy::Kill)
            .turns()
            .sticks(),
    ]
}

/// One run of a full configuration: a stuck child, a parent stop and a
/// parent kill as separate units (so a Kill reaches every phase of the
/// supervisor), and the given crashes, manual calls, runner failures and
/// spawns of spare keys.
fn full_run(base: &Cfg, crash: u8, manual: u8, runner_fail: u8, spawns: u8) -> Cfg {
    with(base, |b| {
        b.crash_s = crash;
        b.manual = manual;
        b.runner_fail = runner_fail;
        b.new_spawn = spawns;
        b.parent_stop = 1;
        b.parent_kill = 1;
        b.stuck = 1;
    })
}

fn full_cfg(name: &'static str, strategy: Strategy) -> Cfg {
    let mut c = base(name, strategy, full_children(), 3);
    c.budget_s = 3;
    c.limit = FULL_LIMIT;
    c
}

fn full_one_for_one_cfgs() -> Vec<Cfg> {
    let c = full_cfg("full_one_for_one", Strategy::OneForOne);
    vec![
        full_run(&c, 1, 0, 1, 0),
        full_run(&c, 2, 0, 0, 0),
        full_run(&c, 1, 1, 0, 0),
    ]
}

fn full_one_for_all_cfgs() -> Vec<Cfg> {
    let c = full_cfg("full_one_for_all", Strategy::OneForAll);
    vec![full_run(&c, 1, 0, 1, 0), full_run(&c, 2, 1, 0, 0)]
}

fn full_rest_for_one_cfgs() -> Vec<Cfg> {
    let mut c = full_cfg("full_rest_for_one", Strategy::RestForOne);
    c.kids.push(PLAIN);
    vec![
        full_run(&c, 1, 0, 1, 0),
        full_run(&c, 2, 0, 0, 1),
        full_run(&c, 1, 1, 0, 0),
    ]
}

fn full_pool_cfgs() -> Vec<Cfg> {
    let mut c = full_cfg("full_pool", Strategy::Pool);
    c.initial = 1;
    vec![
        full_run(&c, 1, 0, 1, 1),
        full_run(&c, 2, 1, 0, 1),
        full_run(&c, 1, 0, 0, 2),
    ]
}

/// Three members, the middle one vetoing every request under `Infinity`: a
/// restart attempt of a member can end while the supervisor's handler wait
/// on that member's stop is exempt.
fn full_override_three_child_cfgs() -> Vec<Cfg> {
    let veto_inf = ChildCfg::new()
        .behavior(Behavior::Vetoing)
        .policy(Policy::Infinity);
    let quick = PLAIN.policy(Policy::Kill);
    let mut c = base(
        "full_override_three_child",
        Strategy::OneForAll,
        vec![quick, veto_inf, quick],
        3,
    );
    c.limit = FULL_LIMIT;
    vec![with(&c, |b| {
        b.crash_s = 2;
        b.manual = 2;
    })]
}

fn full_lane_cfgs() -> Vec<Cfg> {
    let mut c = base("full_lane", Strategy::OneForOne, vec![PLAIN.turns()], 1);
    c.budgets = Budgets {
        third: 3,
        bare_kill: 1,
        parent_stop: 1,
        ..Budgets::default()
    };
    c.third_reasons = vec![
        Reason::Graceful,
        Reason::ParentRequest,
        Reason::Failure(Cause::Crash),
        Reason::ParentKilled,
        Reason::Cancelled,
    ];
    c.limit = FULL_LIMIT;
    vec![c]
}

// ---------------------------------------------------------------------------
// Running a configuration and reporting
// ---------------------------------------------------------------------------

/// Explore each run of a configuration, print the counts and the witnesses in
/// words, and fail once, listing every failing check, if anything failed or
/// a witness that must be reached was not.
fn run_configuration(cfgs: &[Cfg], required: &[Wit]) {
    run_configuration_with(cfgs, required, &[]);
}

/// As `run_configuration`, and each run `i` must also reach the witnesses
/// `per_run[i]` lists.
fn run_configuration_with(cfgs: &[Cfg], required: &[Wit], per_run: &[&[Wit]]) {
    let mut report = String::new();
    let mut failed: Vec<(&'static str, String)> = Vec::new();
    let mut reached: BTreeMap<Wit, u64> = BTreeMap::new();
    let mut limit_note = None;
    let mut panics: Vec<String> = Vec::new();
    let mut missing_per_run: Vec<String> = Vec::new();
    // Every run of a configuration is an independent search.
    let outs: Vec<(Outcome2, f64)> = std::thread::scope(|sc| {
        let handles: Vec<_> = cfgs
            .iter()
            .map(|cfg| {
                std::thread::Builder::new()
                    .stack_size(256 << 20)
                    .spawn_scoped(sc, move || {
                        let started = std::time::Instant::now();
                        let out = explore(cfg);
                        (out, started.elapsed().as_secs_f64())
                    })
                    .expect("spawn a search thread")
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("search thread"))
            .collect()
    });
    for (i, (cfg, (out, elapsed))) in cfgs.iter().zip(outs).enumerate() {
        report.push_str(&format!(
            "{} run {}: {} states, {} quiescent, {:.1} s, deepest bounded search {}\n",
            cfg.name,
            i + 1,
            out.states,
            out.quiescent,
            elapsed,
            out.deepest
        ));
        report.push_str(&format!(
            "  conformance sites compared: lane_accept {}, init_kill_check {}, \
             own_budget and own_fin {}, successor_bound {}, successor_wait_step {}\n",
            out.lane_sites, out.init_sites, out.budget_sites, out.bound_sites, out.wait_step_sites
        ));
        report.push_str(&format!(
            "  conformance sites compared: manual_commit {}, manual_answer {}, group_membership {} (of which released at another token: chain-restarted keys {}, keys spawned since {})\n",
            out.manual_sites,
            out.answer_sites,
            out.membership_sites,
            out.released_after_restart,
            out.released_spawned_since
        ));
        report.push_str(&format!(
            "  conformance sites compared: stop_queue_next {}, teardown_walk pool {}\n",
            out.queue_sites, out.pool_sites
        ));
        report.push_str(&format!(
            "  conformance sites compared: inherited_stop {}, verdict_floor {}\n",
            out.guard_sites, out.floor_sites
        ));
        for (w, n) in &out.wit {
            report.push_str(&format!("  witness: {} ({n})\n", w.label()));
            *reached.entry(*w).or_insert(0) += n;
        }
        for w in per_run.get(i).copied().unwrap_or(&[]) {
            if !out.wit.contains_key(w) {
                missing_per_run.push(format!("run {}: {}", i + 1, w.label()));
            }
        }
        for f in out.failures {
            if !failed.iter().any(|x| x.0 == f.check) {
                failed.push((f.check, f.detail));
            }
        }
        if let Some(text) = out.panic {
            panics.push(format!("run {}: {text}", i + 1));
        }
        if out.limit_reached {
            limit_note = Some(format!(
                "state limit {} reached after {} states in run {}",
                cfg.limit,
                out.states,
                i + 1
            ));
        }
    }
    println!("{report}");
    let mut message = String::new();
    for (name, detail) in &failed {
        let head: String = detail.chars().take(16000).collect();
        message.push_str(&format!(
            "check failed: {name}\n  first violation: {head}\n"
        ));
    }
    if let Some(n) = limit_note {
        message.push_str(&format!("{n}\n"));
    }
    for text in &panics {
        message.push_str(&format!("search panicked, {text}\n"));
    }
    for w in required {
        if !reached.contains_key(w) {
            message.push_str(&format!("witness not reached: {}\n", w.label()));
        }
    }
    for text in &missing_per_run {
        message.push_str(&format!("witness not reached, {text}\n"));
    }
    assert!(message.is_empty(), "{message}");
}

#[test]
fn small_one_for_one() {
    run_configuration(
        &small_one_for_one_cfgs(),
        &[
            Wit::ReportForRetiredTokenPending,
            Wit::CallerAbandonsMidLadder,
            Wit::ManualCallOnEndedUndrained,
            Wit::FinalDelivery,
            Wit::FailedFinalDeliveryRestStillDelivered,
        ],
    );
}

#[test]
fn small_one_for_all() {
    run_configuration(
        &small_one_for_all_cfgs(),
        &[
            Wit::MemberStoppingAtEvaluation,
            Wit::TerminateCommittedBeforeEvaluation,
            Wit::ManualCallOnReleased,
            Wit::UnspawnedAttemptOrderedAtEvaluation,
            Wit::StopChildAbsorbed,
            Wit::StopOrRestartRefusedEarlierChain,
            Wit::StopChildRefusedOverriddenAwaited,
        ],
    );
}

#[test]
fn small_rest_for_one() {
    run_configuration(
        &small_rest_for_one_cfgs(),
        &[
            Wit::SpawnedDuringGroupJoinsRetry,
            Wit::ManualAttemptFailsAsMember,
            Wit::ReportQueuedCollectedByFinalDrain,
            Wit::StopOrRestartRefusedEarlierChain,
            Wit::KeepRecordsKeyOutsideRetryPlan,
        ],
    );
}

#[test]
fn small_foreign_key() {
    run_configuration(&small_foreign_key_cfgs(), &[]);
}

#[test]
fn small_non_permanent() {
    run_configuration(&small_non_permanent_cfgs(), &[]);
}

#[test]
fn small_tree_stored_kill() {
    run_configuration_with(
        &small_tree_stored_kill_cfgs(),
        &[],
        &[
            &[Wit::GuardKeepsBareKillAbortUnderBoundedPolicy],
            &[Wit::GuardKeepsBareKillAbortUnderInfinity],
        ],
    );
}

#[test]
fn small_shutdown_abandoned_sweep() {
    run_configuration(
        &small_shutdown_abandoned_sweep_cfgs(),
        &[Wit::DeadlineEntryFiresAfterAbandon],
    );
}

#[test]
fn small_tree_stored_ladder_kill() {
    run_configuration_with(
        &small_tree_stored_ladder_kill_cfgs(),
        &[],
        &[
            &[Wit::GuardKeepsLadderKillAbortUnderInfinity],
            &[Wit::GuardKeepsLadderKillAbortUnderBoundedPolicy],
        ],
    );
}

#[test]
fn small_tree_kill_paths() {
    run_configuration_with(
        &small_tree_kill_paths_cfgs(),
        &[
            Wit::BusyNamedWorkerAtSupervisorKill,
            Wit::SuccessorWaitingOnLiveSubtree,
            Wit::AttemptOrderedWait,
        ],
        &[
            &[Wit::BusyNamedWorkerAtSupervisorKill],
            &[Wit::BusyNamedWorkerAtSupervisorKill],
            &[Wit::BusyNamedWorkerAtSupervisorKill],
            &[Wit::BusyNamedWorkerAtSupervisorKill],
        ],
    );
}

#[test]
fn small_tree_parent_killed() {
    run_configuration(
        &small_tree_parent_killed_cfgs(),
        &[
            Wit::SupervisorChildUnderInheritedStop,
            Wit::ZeroGraceOrderlyTeardownCompletes,
        ],
    );
}

#[test]
fn small_tree_pairing() {
    run_configuration(&small_tree_pairing_cfgs(), &[]);
}

#[test]
fn small_tree_stuck_grandchild() {
    run_configuration_with(
        &small_tree_stuck_grandchild_cfgs(),
        &[Wit::SuccessorWaitExpired],
        &[
            &[],
            &[Wit::ReportCollectedByEscalationDrain],
            &[Wit::OrderBesideAnExpirableWait],
        ],
    );
}

#[test]
fn small_tree_infinity() {
    run_configuration(&small_tree_infinity_cfgs(), &[]);
}

#[test]
fn small_tree_delete_respawn() {
    run_configuration(
        &small_tree_delete_respawn_cfgs(),
        &[
            Wit::SpawnWaitAfterAbandonedTerminate,
            Wit::SpawnWaitAfterPruning,
            Wit::SpawnWaitAfterPoolBounce,
            Wit::TerminateOfDownChildWaitsOnSubtree,
        ],
    );
}

#[test]
fn small_tree_group_wait() {
    run_configuration(
        &small_tree_group_wait_cfgs(),
        &[Wit::WaitingAttemptOrderedAtEvaluation],
    );
}

#[test]
fn small_tree_retry_wait() {
    run_configuration(
        &small_tree_retry_wait_cfgs(),
        &[Wit::WaitingAttemptOrderedAtRetry],
    );
}

#[test]
fn small_tree_failed_attempt_subtree() {
    run_configuration(
        &small_tree_failed_attempt_subtree_cfgs(),
        &[Wit::FailedAttemptSubtreeWaitedFor],
    );
}

#[test]
fn small_tree_worker_budget() {
    run_configuration(
        &small_tree_worker_budget_cfgs(),
        &[Wit::SupervisorIncarnationExhaustedItsBudget],
    );
}

#[test]
fn small_tree_lost_runner() {
    run_configuration(
        &small_tree_lost_runner_cfgs(),
        &[Wit::LostRunnerVerdictAfterGrandparentKill],
    );
}

#[test]
fn small_tree_manual_parent() {
    run_configuration(
        &small_tree_manual_parent_cfgs(),
        &[
            Wit::ParentStopWhileHandlerWaits,
            Wit::TeardownOrdersSupervisorChildUnderKill,
        ],
    );
}

#[test]
fn small_system_shutdown() {
    run_configuration(
        &small_system_shutdown_cfgs(),
        &[
            Wit::ChildCrashAfterAbandonedShutdown,
            Wit::RestartRefusedWhileShuttingDown,
            Wit::RestartRefusedAtSupervisorLevel,
            Wit::ShutdownKillReachesSupervisorChild,
        ],
    );
}

#[test]
#[ignore = "full bounds: run in release with --ignored"]
fn full_tree() {
    run_configuration(
        &full_tree_cfgs(),
        &[
            Wit::BusyNamedWorkerAtSupervisorKill,
            Wit::SuccessorWaitingOnLiveSubtree,
            Wit::ParentStopWhileHandlerWaits,
        ],
    );
}

#[test]
#[ignore = "full bounds: run in release with --ignored"]
fn full_tree_manual() {
    run_configuration(
        &full_tree_manual_cfgs(),
        &[
            Wit::BusyNamedWorkerAtSupervisorKill,
            Wit::SuccessorWaitingOnLiveSubtree,
        ],
    );
}

#[test]
fn small_pool() {
    run_configuration(&small_pool_cfgs(), &[]);
}

#[test]
fn small_three_member_group() {
    run_configuration(&small_three_member_group_cfgs(), &[]);
}

#[test]
fn small_retry() {
    // Run 5 is built to commit and order an attempt in flight at a retry, and
    // run 6 to keep a child started since the group began down through the
    // retry: each must reach its own witnesses.
    run_configuration_with(
        &small_retry_cfgs(),
        &[
            Wit::QueuedDeathAtRetry,
            Wit::TerminateOfUnsignalledMember,
            Wit::OwedEvaluationAbsorbedIntoSnapshot,
            Wit::TerminateDuringChainKeptDownThroughRetry,
            Wit::StopChildAbsorbedInRetry,
            Wit::AttemptCommittedAtRetry,
            Wit::UnspawnedAttemptOrderedAtRetry,
            Wit::StopChildKeptDownSpawnedSince,
            Wit::SpawnedSinceOverrideLeftOutOfTheRetry,
        ],
        &[
            &[],
            &[],
            &[],
            &[],
            &[
                Wit::AttemptCommittedAtRetry,
                Wit::UnspawnedAttemptOrderedAtRetry,
            ],
            &[
                Wit::StopChildKeptDownSpawnedSince,
                Wit::SpawnedSinceOverrideLeftOutOfTheRetry,
                Wit::AsTerminateTransient,
            ],
        ],
    );
}

#[test]
fn small_lane() {
    run_configuration(&small_lane_cfgs(), &[Wit::ThirdGracefulOverParentRequest]);
}

#[test]
fn small_inherited_transient() {
    run_configuration(&small_inherited_transient_cfgs(), &[]);
}

#[test]
fn small_unresponsive() {
    // The third run is the one built to reach a join on a verdict-carrying
    // occupant: it must reach it itself. The second run reaches the override
    // of a member that carries a verdict.
    run_configuration_with(
        &small_unresponsive_cfgs(),
        &[
            Wit::StuckMemberInBarrier,
            Wit::StuckMemberRunnerFailedVerdict,
            Wit::ReportCollectedByEscalationDrain,
            Wit::ManualJoinAnsweredUnresponsive,
            Wit::OverrideAnsweredUnresponsive,
        ],
        &[
            &[],
            &[Wit::OverrideAnsweredUnresponsive],
            &[Wit::ManualJoinAnsweredUnresponsive],
        ],
    );
}

#[test]
fn small_kill() {
    run_configuration(
        &small_kill_cfgs(),
        &[
            Wit::KillDuringHook,
            Wit::KillDuringPreStart,
            Wit::KillDuringTeardown,
            Wit::KillDuringSupervisorHook,
            Wit::KillStopsFinalDrain,
        ],
    );
}

#[test]
fn small_attempts() {
    run_configuration(
        &small_attempts_cfgs(),
        &[
            Wit::AttemptOrderedFactory,
            Wit::AttemptOrderedInit,
            Wit::AttemptOrderedInTransit,
            Wit::InitOutlivesStartTimeoutAndLadder,
        ],
    );
}

#[test]
fn small_runner_failure() {
    run_configuration(
        &small_runner_failure_cfgs(),
        &[
            Wit::RunnerFailureBeforeSpawn,
            Wit::RunnerFailureAfterSpawn,
            Wit::ManualCallOnRunnerFailedEnded,
            Wit::CallerWaitedForRunnerFailedEnd,
            Wit::TeardownPassedRunnerFailedEnd,
        ],
    );
}

#[test]
fn small_manual_restart() {
    run_configuration(
        &small_manual_restart_cfgs(),
        &[Wit::ManualRestartInitFails, Wit::StartAckedThenDied],
    );
}

#[test]
fn small_override_one_more_ask() {
    run_configuration(
        &small_override_one_more_ask_cfgs(),
        &[
            Wit::OverrideOneMoreAsk,
            Wit::AcceptsAfterTheOneMoreAsk,
            Wit::TeardownAskAfterGroupStopping,
            Wit::TeardownAskAfterGroupRunning,
            Wit::ManualJoinOneMoreAsk,
            Wit::SecondOverrideAsk,
            Wit::TeardownAskToOrderedAttempt,
            Wit::KeepJoinSharedAsk,
        ],
    );
}

#[test]
#[ignore = "full bounds: run in release with --ignored"]
fn full_one_for_one() {
    run_configuration(&full_one_for_one_cfgs(), &[]);
}

#[test]
#[ignore = "full bounds: run in release with --ignored"]
fn full_one_for_all() {
    run_configuration(
        &full_one_for_all_cfgs(),
        &[Wit::AsTerminateTransient, Wit::AsTerminateTemporary],
    );
}

#[test]
#[ignore = "full bounds: run in release with --ignored"]
fn full_rest_for_one() {
    run_configuration(
        &full_rest_for_one_cfgs(),
        &[Wit::AsTerminateTransient, Wit::AsTerminateTemporary],
    );
}

#[test]
#[ignore = "full bounds: run in release with --ignored"]
fn full_pool() {
    run_configuration(&full_pool_cfgs(), &[]);
}

#[test]
#[ignore = "full bounds: run in release with --ignored"]
fn full_override_three_child() {
    run_configuration(
        &full_override_three_child_cfgs(),
        &[Wit::ExemptThroughHandlerOnEndedAttempt],
    );
}

#[test]
#[ignore = "full bounds: run in release with --ignored"]
fn full_lane() {
    run_configuration(&full_lane_cfgs(), &[]);
}

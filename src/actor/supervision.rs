//! Supervision configuration, restart budgets, and child registry.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::Instant;

use crate::types::{
    ActorId, ChildInfo, ChildStoppedInternal, RestartStrategy, RestartType, Shutdown, SystemMessage,
};

/// Type-erased restart function stored per child.
///
/// The closure captures the child's original [`ActorId`], name, and the full
/// resolved [`ActorConfig`](crate::actor::runtime::ActorConfig) by value at
/// `spawn_child` time (OTP child-spec immutability), so every restart reuses
/// the exact spec. Given the restart sequence token, it spawns a new instance
/// and reports back to the parent's system channel: `RestartComplete` on
/// success, a synthesized `ChildStopped` on factory panic or spawn failure.
pub(crate) type RestartFn =
    Box<dyn Fn(u64) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Grace given to a cooperative Kill signal before the abort() backstop fires.
/// Kill is processed with biased priority, so a responsive actor dies in
/// microseconds; the grace exists only for an actor mid-callback.
pub(crate) const KILL_GRACE: Duration = Duration::from_millis(100);

/// How a manual stop was requested, recorded on the child so its death event
/// bypasses strategy evaluation (manual stops are never failures).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManualStop {
    /// `stop_child`: restart per policy afterwards, budget-free.
    Bounce,
    /// `terminate_child`: stay down until restart_child/delete_child (OTP).
    Terminate,
}

// ---------------------------------------------------------------------------
// SupervisionConfig
// ---------------------------------------------------------------------------

/// Configuration for an actor acting as a supervisor.
///
/// Maps to OTP supervisor child specs: strategy, intensity (max_restarts),
/// and period (restart_window).
#[derive(Debug, Clone)]
pub struct SupervisionConfig {
    /// The restart strategy to use.
    pub strategy: RestartStrategy,
    /// Maximum number of restarts allowed within `restart_window`.
    pub max_restarts: u32,
    /// The sliding window for counting restarts.
    pub restart_window: Duration,
}

impl Default for SupervisionConfig {
    fn default() -> Self {
        Self {
            strategy: RestartStrategy::OneForOne,
            max_restarts: 3,
            restart_window: Duration::from_secs(5),
        }
    }
}

impl SupervisionConfig {
    /// OneForOne strategy with default budget.
    pub fn one_for_one() -> Self {
        Self::default()
    }

    /// OneForAll strategy with default budget.
    pub fn one_for_all() -> Self {
        Self {
            strategy: RestartStrategy::OneForAll,
            ..Self::default()
        }
    }

    /// RestForOne strategy with default budget.
    pub fn rest_for_one() -> Self {
        Self {
            strategy: RestartStrategy::RestForOne,
            ..Self::default()
        }
    }

    /// SimpleOneForOne strategy with default budget.
    pub fn simple_one_for_one() -> Self {
        Self {
            strategy: RestartStrategy::SimpleOneForOne,
            ..Self::default()
        }
    }

    /// Sets the restart budget (max restarts within a sliding window).
    pub fn max_restarts(mut self, max: u32, window: Duration) -> Self {
        self.max_restarts = max;
        self.restart_window = window;
        self
    }
}

// ---------------------------------------------------------------------------
// RestartBudget
// ---------------------------------------------------------------------------

/// Sliding-window restart budget tracker.
///
/// Uses a `VecDeque<Instant>` to record restart timestamps.
/// Expired entries (outside the window) are pruned on each check.
pub(crate) struct RestartBudget {
    max_restarts: u32,
    restart_window: Duration,
    timestamps: VecDeque<Instant>,
}

impl RestartBudget {
    pub fn new(max_restarts: u32, restart_window: Duration) -> Self {
        Self {
            max_restarts,
            restart_window,
            timestamps: VecDeque::new(),
        }
    }

    /// Checks if a restart is allowed. If yes, records it and returns `true`.
    /// If the budget is exhausted, returns `false`.
    pub fn check_and_record(&mut self) -> bool {
        let now = Instant::now();
        // checked_sub, never `now - self.restart_window`: a paused or
        // simulated clock (tokio::time::pause) can sit close to its own
        // epoch, so a window wider than elapsed time would underflow a raw
        // subtraction and panic. `None` here just means nothing is old
        // enough to prune yet - a safe no-op, so the count-based check below
        // still runs correctly.
        let cutoff = now.checked_sub(self.restart_window);

        // Prune expired entries
        if let Some(cutoff) = cutoff {
            while let Some(&front) = self.timestamps.front() {
                if front < cutoff {
                    self.timestamps.pop_front();
                } else {
                    break;
                }
            }
        }

        if self.timestamps.len() >= self.max_restarts as usize {
            return false;
        }

        self.timestamps.push_back(now);
        true
    }
}

// ---------------------------------------------------------------------------
// ChildSpec
// ---------------------------------------------------------------------------

/// Per-child supervision specification.
pub(crate) struct ChildSpec {
    pub restart_type: RestartType,
    pub shutdown: Shutdown,
}

// ---------------------------------------------------------------------------
// ChildState
// ---------------------------------------------------------------------------

/// Runtime state for a supervised child.
pub(crate) struct ChildState {
    pub id: ActorId,
    pub name: Option<String>,
    pub spec: ChildSpec,
    /// Handle of the child's WATCHER task. The watcher owns the child's real
    /// `JoinHandle<StopReason>`; watcher completion therefore implies the
    /// child task has fully terminated (all its drops have run).
    pub watcher_handle: JoinHandle<()>,
    /// Abort handle of the child's OWN task (taken before the watcher consumed
    /// the JoinHandle). The Kill -> abort escalation backstop. Never exposed
    /// outside the supervision tree.
    pub abort: AbortHandle,
    pub system_tx: mpsc::Sender<SystemMessage>,
    pub is_alive: bool,
    pub pending_restart_seq: Option<u64>,
    /// Incarnation token of the instance currently owning this spec:
    /// 0 for the initial spawn, the restart seq after each accepted restart.
    /// Death events from superseded incarnations are ignored.
    pub current_incarnation: u64,
    /// Set while a manual stop (stop_child / terminate_child) is in flight;
    /// the resulting death event bypasses strategy evaluation.
    pub manual_stop: Option<ManualStop>,
}

impl ChildState {
    /// True if a death event with this incarnation belongs to the instance
    /// the registry currently tracks (current, or the in-flight restart).
    pub fn accepts_incarnation(&self, incarnation: u64) -> bool {
        self.current_incarnation == incarnation || self.pending_restart_seq == Some(incarnation)
    }
}

// ---------------------------------------------------------------------------
// ChildRegistry
// ---------------------------------------------------------------------------

/// Ordered child registry with O(1) lookup by ID.
///
/// Children are stored in insertion order (start order).
/// `Vec<ChildState>` preserves ordering for RestForOne/OneForAll.
/// `HashMap<ActorId, usize>` provides O(1) lookup.
pub(crate) struct ChildRegistry {
    children: Vec<ChildState>,
    index: HashMap<ActorId, usize>,
    restart_seq: u64,
}

impl ChildRegistry {
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
            index: HashMap::new(),
            restart_seq: 0,
        }
    }

    pub fn register(&mut self, state: ChildState) {
        let idx = self.children.len();
        self.index.insert(state.id.clone(), idx);
        self.children.push(state);
    }

    pub fn remove(&mut self, id: &ActorId) -> Option<ChildState> {
        let idx = self.index.remove(id)?;
        let child = self.children.remove(idx);
        // Rebuild index for items after the removed one
        for (new_idx, child) in self.children.iter().enumerate().skip(idx) {
            self.index.insert(child.id.clone(), new_idx);
        }
        Some(child)
    }

    pub fn get(&self, id: &ActorId) -> Option<&ChildState> {
        self.index.get(id).map(|&idx| &self.children[idx])
    }

    pub fn get_mut(&mut self, id: &ActorId) -> Option<&mut ChildState> {
        self.index
            .get(id)
            .copied()
            .map(|idx| &mut self.children[idx])
    }

    pub fn children_info(&self) -> Vec<ChildInfo> {
        self.children
            .iter()
            .map(|c| ChildInfo {
                id: c.id.clone(),
                name: c.name.clone(),
                restart_type: c.spec.restart_type,
                shutdown: c.spec.shutdown,
                is_alive: c.is_alive,
                restart_pending: c.pending_restart_seq.is_some(),
            })
            .collect()
    }

    /// Sequence tokens start at 1 so that 0 is reserved for the initial
    /// incarnation of every child.
    pub fn next_seq(&mut self) -> u64 {
        self.restart_seq += 1;
        self.restart_seq
    }

    /// Returns IDs of children started after the given child (for RestForOne).
    pub fn children_after(&self, id: &ActorId) -> Vec<ActorId> {
        if let Some(&idx) = self.index.get(id) {
            self.children[idx + 1..]
                .iter()
                .map(|c| c.id.clone())
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Returns all child IDs (for OneForAll).
    pub fn all_ids(&self) -> Vec<ActorId> {
        self.children.iter().map(|c| c.id.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.children.len()
    }

    /// Drain all children (for parent shutdown).
    pub fn drain_all(&mut self) -> Vec<ChildState> {
        self.index.clear();
        std::mem::take(&mut self.children)
    }

    /// Adopts a restarted child instance after the parent accepted its
    /// `RestartComplete` (seq matches the pending restart).
    pub fn update_restarted(
        &mut self,
        child_id: &ActorId,
        seq: u64,
        new_system_tx: mpsc::Sender<SystemMessage>,
        new_watcher_handle: JoinHandle<()>,
        new_abort: AbortHandle,
    ) -> bool {
        if let Some(child) = self.get_mut(child_id) {
            if child.pending_restart_seq == Some(seq) {
                child.system_tx = new_system_tx;
                child.watcher_handle = new_watcher_handle;
                child.abort = new_abort;
                child.is_alive = true;
                child.pending_restart_seq = None;
                child.current_incarnation = seq;
                child.manual_stop = None;
                return true;
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Group restart state (OneForAll / RestForOne)
// ---------------------------------------------------------------------------

/// In-flight group restart: the affected live members have been told to stop
/// (reverse start order); once every awaited death arrives, the members are
/// restarted SEQUENTIALLY in start order. The supervisor's loop never blocks
/// on this; deaths arrive through the ordinary watcher channel.
pub(crate) struct GroupRestart {
    /// Members whose deaths we are still waiting for.
    pub awaiting: HashSet<ActorId>,
    /// Members to restart (start order) once `awaiting` empties. Excludes
    /// Temporary children (OTP: terminated with the group, never restarted).
    pub restart_order: Vec<ActorId>,
}

/// Phase of an in-flight OneForAll/RestForOne group restart.
pub(crate) enum GroupPhase {
    /// Affected members told to stop; awaiting their deaths.
    Stopping(GroupRestart),
    /// Restarting members one at a time in start order (OTP left-to-right).
    /// The FRONT of the queue is the member whose restart is in flight; the
    /// next member is initiated only when the front's `RestartComplete` is
    /// adopted, giving sequential registration. (Sequential init COMPLETION
    /// would additionally need the spawn ack wired into the restart path.)
    Restarting(VecDeque<ActorId>),
}

// ---------------------------------------------------------------------------
// Supervision state (held by ActorContext)
// ---------------------------------------------------------------------------

/// Internal supervision state stored in the actor context.
pub(crate) struct SupervisionState {
    pub config: SupervisionConfig,
    pub registry: ChildRegistry,
    pub budget: RestartBudget,
    /// Type-erased restart functions keyed by child ID.
    pub restart_fns: HashMap<ActorId, RestartFn>,
    /// In-flight OneForAll/RestForOne group restart, if any.
    pub pending_group: Option<GroupPhase>,
    /// Failure events that arrived while a group restart was pending;
    /// evaluated FIFO after the group completes.
    pub queued_triggers: VecDeque<ChildStoppedInternal>,
}

impl SupervisionState {
    pub fn new(config: SupervisionConfig) -> Self {
        let budget = RestartBudget::new(config.max_restarts, config.restart_window);
        Self {
            config,
            registry: ChildRegistry::new(),
            budget,
            restart_fns: HashMap::new(),
            pending_group: None,
            queued_triggers: VecDeque::new(),
        }
    }

    /// Initiates a restart for a child using its stored restart closure:
    /// bumps the seq, marks the pending restart, and spawns the closure.
    /// Does NOT touch the budget (callers decide whether the restart is
    /// budget-charged - strategy restarts are, manual bounces are not).
    pub fn initiate(&mut self, child_id: &ActorId) -> bool {
        let seq = self.registry.next_seq();
        match self.registry.get_mut(child_id) {
            Some(child) => {
                child.pending_restart_seq = Some(seq);
                child.is_alive = false;
            }
            None => return false,
        }
        if let Some(restart_fn) = self.restart_fns.get(child_id) {
            let fut = restart_fn(seq);
            tokio::spawn(fut);
            true
        } else {
            false
        }
    }

    /// True if the child is a member of the in-flight group restart (either
    /// phase). Manual child-management APIs refuse to touch such members.
    pub fn in_pending_group(&self, child_id: &ActorId) -> bool {
        match self.pending_group.as_ref() {
            Some(GroupPhase::Stopping(group)) => {
                group.awaiting.contains(child_id) || group.restart_order.contains(child_id)
            }
            Some(GroupPhase::Restarting(queue)) => queue.contains(child_id),
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Strategy helpers (used by runtime)
// ---------------------------------------------------------------------------

/// Result of applying a supervision strategy to a child's death.
pub(crate) enum StrategyOutcome {
    /// Restart exactly this child (OneForOne / SimpleOneForOne).
    RestartOne(ActorId),
    /// Group restart (OneForAll / RestForOne): stop the live members in
    /// `stop_reverse` (already in reverse start order), await their deaths,
    /// then restart `restart_order` in start order.
    RestartGroup {
        /// Live members to stop, reverse start order. Excludes the failed
        /// child (already dead).
        stop_reverse: Vec<ActorId>,
        /// Members to restart in start order (Temporary members excluded).
        restart_order: Vec<ActorId>,
    },
    /// The child is not restarted (temporary, or transient after a clean stop).
    Remove,
    /// The restart budget is exhausted; the supervisor must stop.
    BudgetExhausted,
}

/// Determines the supervision action for a stopped child.
///
/// Charges the budget ONCE per triggering failure regardless of group size
/// (OTP counts supervisor restarts, not per-child spawns).
pub(crate) fn evaluate_strategy(
    sup: &mut SupervisionState,
    failed_child_id: &ActorId,
    reason: &crate::types::StopReason,
) -> StrategyOutcome {
    let child = match sup.registry.get(failed_child_id) {
        Some(c) => c,
        None => return StrategyOutcome::Remove,
    };

    let restart_type = child.spec.restart_type;
    let should_restart = match restart_type {
        RestartType::Permanent => true,
        // OTP transient parity: restart only on ABNORMAL exits. The clean
        // reasons are normal and shutdown - our Graceful and ParentRequest
        // (which is also the exit reason of a budget-exhausted supervisor).
        // Kill (OTP `killed`) is abnormal and does restart.
        RestartType::Transient => !matches!(
            reason,
            crate::types::StopReason::Graceful | crate::types::StopReason::ParentRequest
        ),
        RestartType::Temporary => false,
    };

    if !should_restart {
        return StrategyOutcome::Remove;
    }

    // Check budget
    if !sup.budget.check_and_record() {
        return StrategyOutcome::BudgetExhausted;
    }

    // Determine the affected set based on strategy
    match sup.config.strategy {
        RestartStrategy::OneForOne | RestartStrategy::SimpleOneForOne => {
            StrategyOutcome::RestartOne(failed_child_id.clone())
        }
        RestartStrategy::OneForAll => {
            let members = sup.registry.all_ids();
            group_outcome(&sup.registry, members, failed_child_id)
        }
        RestartStrategy::RestForOne => {
            let mut members = vec![failed_child_id.clone()];
            members.extend(sup.registry.children_after(failed_child_id));
            group_outcome(&sup.registry, members, failed_child_id)
        }
    }
}

/// Builds the group outcome from the affected member set (start order).
fn group_outcome(
    registry: &ChildRegistry,
    members: Vec<ActorId>,
    failed: &ActorId,
) -> StrategyOutcome {
    let stop_reverse: Vec<ActorId> = members
        .iter()
        .rev()
        .filter(|id| *id != failed)
        .filter(|id| registry.get(id).map(|c| c.is_alive).unwrap_or(false))
        .cloned()
        .collect();
    // OTP: temporary siblings are terminated with the group but never restarted.
    let restart_order: Vec<ActorId> = members
        .into_iter()
        .filter(|id| {
            registry
                .get(id)
                .map(|c| !matches!(c.spec.restart_type, RestartType::Temporary))
                .unwrap_or(false)
        })
        .collect();
    StrategyOutcome::RestartGroup {
        stop_reverse,
        restart_order,
    }
}

// ---------------------------------------------------------------------------
// Unit tests (pub(crate) internals, Rust Book Ch 11.3)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::StopReason;

    // - RestartBudget -------------------------------------------------------

    #[tokio::test]
    async fn budget_allows_within_limit() {
        let mut budget = RestartBudget::new(3, Duration::from_secs(60));
        assert!(budget.check_and_record());
        assert!(budget.check_and_record());
        assert!(budget.check_and_record());
    }

    #[tokio::test]
    async fn budget_denies_when_exhausted() {
        let mut budget = RestartBudget::new(2, Duration::from_secs(60));
        assert!(budget.check_and_record());
        assert!(budget.check_and_record());
        assert!(!budget.check_and_record());
    }

    #[tokio::test]
    async fn budget_recovers_after_window() {
        let mut budget = RestartBudget::new(1, Duration::from_millis(50));
        assert!(budget.check_and_record());
        assert!(!budget.check_and_record());
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(budget.check_and_record());
    }

    // - ChildRegistry -------------------------------------------------------

    fn dummy_child_with(id: &str, restart_type: RestartType, alive: bool) -> ChildState {
        let (tx, _rx) = mpsc::channel(1);
        ChildState {
            id: ActorId::from(id),
            name: Some(id.to_string()),
            spec: ChildSpec {
                restart_type,
                shutdown: Shutdown::default(),
            },
            watcher_handle: tokio::spawn(async {}),
            abort: tokio::spawn(async {}).abort_handle(),
            system_tx: tx,
            is_alive: alive,
            pending_restart_seq: None,
            current_incarnation: 0,
            manual_stop: None,
        }
    }

    fn dummy_child(id: &str) -> ChildState {
        dummy_child_with(id, RestartType::Permanent, true)
    }

    #[tokio::test]
    async fn registry_register_and_get() {
        let mut reg = ChildRegistry::new();
        reg.register(dummy_child("a"));
        reg.register(dummy_child("b"));
        assert_eq!(reg.len(), 2);
        assert!(reg.get(&ActorId::from("a")).is_some());
        assert!(reg.get(&ActorId::from("b")).is_some());
        assert!(reg.get(&ActorId::from("c")).is_none());
    }

    #[tokio::test]
    async fn registry_remove_reindexes() {
        let mut reg = ChildRegistry::new();
        reg.register(dummy_child("a"));
        reg.register(dummy_child("b"));
        reg.register(dummy_child("c"));

        reg.remove(&ActorId::from("a"));
        assert_eq!(reg.len(), 2);
        assert!(reg.get(&ActorId::from("a")).is_none());
        assert!(reg.get(&ActorId::from("b")).is_some());
        assert!(reg.get(&ActorId::from("c")).is_some());
    }

    #[tokio::test]
    async fn registry_children_after() {
        let mut reg = ChildRegistry::new();
        reg.register(dummy_child("a"));
        reg.register(dummy_child("b"));
        reg.register(dummy_child("c"));

        let after_a = reg.children_after(&ActorId::from("a"));
        assert_eq!(after_a.len(), 2);
        assert_eq!(after_a[0].as_str(), "b");
        assert_eq!(after_a[1].as_str(), "c");

        let after_c = reg.children_after(&ActorId::from("c"));
        assert!(after_c.is_empty());
    }

    #[tokio::test]
    async fn registry_all_ids() {
        let mut reg = ChildRegistry::new();
        reg.register(dummy_child("x"));
        reg.register(dummy_child("y"));
        let ids: Vec<String> = reg
            .all_ids()
            .iter()
            .map(|id| id.as_str().to_string())
            .collect();
        assert_eq!(ids, vec!["x", "y"]);
    }

    #[tokio::test]
    async fn registry_drain_all() {
        let mut reg = ChildRegistry::new();
        reg.register(dummy_child("a"));
        reg.register(dummy_child("b"));
        let drained = reg.drain_all();
        assert_eq!(drained.len(), 2);
        assert_eq!(reg.len(), 0);
    }

    #[tokio::test]
    async fn registry_update_restarted_accepts_pending_seq_only() {
        let mut reg = ChildRegistry::new();
        reg.register(dummy_child("a"));
        let seq = reg.next_seq();
        reg.get_mut(&ActorId::from("a"))
            .unwrap()
            .pending_restart_seq = Some(seq);
        reg.get_mut(&ActorId::from("a")).unwrap().is_alive = false;

        // Stale seq rejected
        let (tx, _rx) = mpsc::channel(1);
        assert!(!reg.update_restarted(
            &ActorId::from("a"),
            seq + 99,
            tx,
            tokio::spawn(async {}),
            tokio::spawn(async {}).abort_handle()
        ));

        // Matching seq accepted; incarnation adopted
        let (tx, _rx) = mpsc::channel(1);
        assert!(reg.update_restarted(
            &ActorId::from("a"),
            seq,
            tx,
            tokio::spawn(async {}),
            tokio::spawn(async {}).abort_handle()
        ));
        let child = reg.get(&ActorId::from("a")).unwrap();
        assert!(child.is_alive);
        assert_eq!(child.current_incarnation, seq);
        assert_eq!(child.pending_restart_seq, None);
        assert!(child.accepts_incarnation(seq));
        assert!(!child.accepts_incarnation(0));
    }

    // - evaluate_strategy ---------------------------------------------------

    fn make_sup_state(strategy: RestartStrategy) -> SupervisionState {
        SupervisionState::new(SupervisionConfig {
            strategy,
            max_restarts: 3,
            restart_window: Duration::from_secs(60),
        })
    }

    #[tokio::test]
    async fn strategy_permanent_restarts_on_any_reason() {
        let mut sup = make_sup_state(RestartStrategy::OneForOne);
        sup.registry.register(dummy_child("child"));
        match evaluate_strategy(&mut sup, &ActorId::from("child"), &StopReason::Graceful) {
            StrategyOutcome::RestartOne(id) => assert_eq!(id.as_str(), "child"),
            _ => panic!("expected RestartOne"),
        }
    }

    #[tokio::test]
    async fn strategy_transient_removes_on_clean_reasons_restarts_on_abnormal() {
        let mut sup = make_sup_state(RestartStrategy::OneForOne);
        sup.registry
            .register(dummy_child_with("child", RestartType::Transient, true));
        let id = ActorId::from("child");
        // Clean: normal (Graceful) and shutdown (ParentRequest) stay down.
        assert!(matches!(
            evaluate_strategy(&mut sup, &id, &StopReason::Graceful),
            StrategyOutcome::Remove
        ));
        assert!(matches!(
            evaluate_strategy(&mut sup, &id, &StopReason::ParentRequest),
            StrategyOutcome::Remove
        ));
        // Abnormal: killed restarts (OTP `killed` is abnormal).
        assert!(matches!(
            evaluate_strategy(&mut sup, &id, &StopReason::Kill),
            StrategyOutcome::RestartOne(_)
        ));
    }

    #[tokio::test]
    async fn strategy_temporary_always_removes() {
        let mut sup = make_sup_state(RestartStrategy::OneForOne);
        sup.registry
            .register(dummy_child_with("child", RestartType::Temporary, true));
        assert!(matches!(
            evaluate_strategy(&mut sup, &ActorId::from("child"), &StopReason::Kill),
            StrategyOutcome::Remove
        ));
    }

    #[tokio::test]
    async fn strategy_one_for_all_stops_live_reverse_restarts_forward() {
        let mut sup = make_sup_state(RestartStrategy::OneForAll);
        sup.registry.register(dummy_child("a"));
        sup.registry.register(dummy_child("b"));
        sup.registry.register(dummy_child("c"));
        // b failed (already dead)
        sup.registry.get_mut(&ActorId::from("b")).unwrap().is_alive = false;
        match evaluate_strategy(&mut sup, &ActorId::from("b"), &StopReason::Kill) {
            StrategyOutcome::RestartGroup {
                stop_reverse,
                restart_order,
            } => {
                let stops: Vec<&str> = stop_reverse.iter().map(|i| i.as_str()).collect();
                assert_eq!(stops, vec!["c", "a"], "live members, reverse start order");
                let restarts: Vec<&str> = restart_order.iter().map(|i| i.as_str()).collect();
                assert_eq!(restarts, vec!["a", "b", "c"], "all members, start order");
            }
            _ => panic!("expected RestartGroup"),
        }
    }

    #[tokio::test]
    async fn strategy_rest_for_one_affects_failed_and_later() {
        let mut sup = make_sup_state(RestartStrategy::RestForOne);
        sup.registry.register(dummy_child("a"));
        sup.registry.register(dummy_child("b"));
        sup.registry.register(dummy_child("c"));
        sup.registry.get_mut(&ActorId::from("a")).unwrap().is_alive = false;
        match evaluate_strategy(&mut sup, &ActorId::from("a"), &StopReason::Kill) {
            StrategyOutcome::RestartGroup {
                stop_reverse,
                restart_order,
            } => {
                let stops: Vec<&str> = stop_reverse.iter().map(|i| i.as_str()).collect();
                assert_eq!(stops, vec!["c", "b"]);
                let restarts: Vec<&str> = restart_order.iter().map(|i| i.as_str()).collect();
                assert_eq!(restarts, vec!["a", "b", "c"]);
            }
            _ => panic!("expected RestartGroup"),
        }
    }

    #[tokio::test]
    async fn strategy_group_excludes_temporary_from_restart_but_stops_it() {
        let mut sup = make_sup_state(RestartStrategy::OneForAll);
        sup.registry.register(dummy_child("a"));
        sup.registry
            .register(dummy_child_with("tmp", RestartType::Temporary, true));
        sup.registry.get_mut(&ActorId::from("a")).unwrap().is_alive = false;
        match evaluate_strategy(&mut sup, &ActorId::from("a"), &StopReason::Kill) {
            StrategyOutcome::RestartGroup {
                stop_reverse,
                restart_order,
            } => {
                let stops: Vec<&str> = stop_reverse.iter().map(|i| i.as_str()).collect();
                assert_eq!(stops, vec!["tmp"], "temporary sibling is stopped");
                let restarts: Vec<&str> = restart_order.iter().map(|i| i.as_str()).collect();
                assert_eq!(restarts, vec!["a"], "temporary sibling is not restarted");
            }
            _ => panic!("expected RestartGroup"),
        }
    }

    #[tokio::test]
    async fn strategy_budget_exhausted() {
        let mut sup = SupervisionState::new(SupervisionConfig {
            strategy: RestartStrategy::OneForOne,
            max_restarts: 1,
            restart_window: Duration::from_secs(60),
        });
        sup.registry.register(dummy_child("child"));
        // First restart - uses the budget
        assert!(matches!(
            evaluate_strategy(&mut sup, &ActorId::from("child"), &StopReason::Kill),
            StrategyOutcome::RestartOne(_)
        ));
        // Second restart - budget exhausted
        assert!(matches!(
            evaluate_strategy(&mut sup, &ActorId::from("child"), &StopReason::Kill),
            StrategyOutcome::BudgetExhausted
        ));
    }

    // A restart_window wider than any representable Instant forces
    // checked_sub to return None on every single check - the same underflow
    // a paused/simulated clock sitting close to its own epoch can trigger
    // (this crate does not depend on tokio's test-util feature, so the
    // window itself is used to force the underflow deterministically rather
    // than driving it through tokio::time::pause()). Two restarts fired
    // back-to-back with no await between them (genuinely zero elapsed
    // wall-clock time) must still be accounted for correctly, and the
    // arithmetic must never panic.
    #[tokio::test]
    async fn strategy_survives_zero_elapsed_restarts_with_underflowing_window() {
        let mut sup = SupervisionState::new(SupervisionConfig {
            strategy: RestartStrategy::OneForOne,
            max_restarts: 2,
            restart_window: Duration::MAX,
        });
        sup.registry.register(dummy_child("child"));
        let id = ActorId::from("child");

        // Two immediate restarts consume the budget, no panic on either.
        assert!(matches!(
            evaluate_strategy(&mut sup, &id, &StopReason::Kill),
            StrategyOutcome::RestartOne(_)
        ));
        assert!(matches!(
            evaluate_strategy(&mut sup, &id, &StopReason::Kill),
            StrategyOutcome::RestartOne(_)
        ));
        // A third restart, still with zero elapsed time, is correctly
        // denied: the count-based gate works even though the time-based
        // prune step above never fires (cutoff is None on every call).
        assert!(matches!(
            evaluate_strategy(&mut sup, &id, &StopReason::Kill),
            StrategyOutcome::BudgetExhausted
        ));
    }
}

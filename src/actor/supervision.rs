//! Supervision configuration, restart budgets, and child registry.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::types::{ActorId, ChildInfo, RestartStrategy, RestartType, Shutdown, SystemMessage};

/// Type-erased restart function stored per child.
///
/// Given a sequence token and optional name, spawns a new instance of the child
/// and sends `RestartComplete` back to the parent's system channel.
pub(crate) type RestartFn =
    Box<dyn Fn(u64, Option<String>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

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
        let cutoff = now - self.restart_window;

        // Prune expired entries
        while let Some(&front) = self.timestamps.front() {
            if front < cutoff {
                self.timestamps.pop_front();
            } else {
                break;
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
    pub join_handle: JoinHandle<()>,
    pub system_tx: mpsc::Sender<SystemMessage>,
    pub is_alive: bool,
    pub pending_restart_seq: Option<u64>,
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

    pub fn next_seq(&mut self) -> u64 {
        self.restart_seq += 1;
        self.restart_seq
    }

    /// Returns children in reverse insertion order (for shutdown).
    #[allow(dead_code)]
    pub fn iter_reverse(&self) -> impl Iterator<Item = &ChildState> {
        self.children.iter().rev()
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

    /// Updates a child entry after a successful restart.
    pub fn update_restarted(
        &mut self,
        child_id: &ActorId,
        seq: u64,
        new_system_tx: mpsc::Sender<SystemMessage>,
        new_join_handle: JoinHandle<()>,
    ) -> bool {
        if let Some(child) = self.get_mut(child_id) {
            if child.pending_restart_seq == Some(seq) {
                child.system_tx = new_system_tx;
                child.join_handle = new_join_handle;
                child.is_alive = true;
                child.pending_restart_seq = None;
                return true;
            }
        }
        false
    }
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
}

impl SupervisionState {
    pub fn new(config: SupervisionConfig) -> Self {
        let budget = RestartBudget::new(config.max_restarts, config.restart_window);
        Self {
            config,
            registry: ChildRegistry::new(),
            budget,
            restart_fns: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Strategy helpers (used by runtime)
// ---------------------------------------------------------------------------

/// Result of applying a supervision strategy.
pub(crate) enum StrategyOutcome {
    /// The child should be restarted. Contains IDs of children to restart.
    Restart(Vec<ActorId>),
    /// The child should be removed (temporary/transient-graceful).
    Remove,
    /// The restart budget is exhausted, supervisor must stop.
    BudgetExhausted,
}

/// Determines the supervision action for a stopped child.
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
        RestartType::Transient => !matches!(reason, crate::types::StopReason::Graceful),
        RestartType::Temporary => false,
    };

    if !should_restart {
        return StrategyOutcome::Remove;
    }

    // Check budget
    if !sup.budget.check_and_record() {
        return StrategyOutcome::BudgetExhausted;
    }

    // Determine which children to restart based on strategy
    let to_restart = match sup.config.strategy {
        RestartStrategy::OneForOne | RestartStrategy::SimpleOneForOne => {
            vec![failed_child_id.clone()]
        }
        RestartStrategy::OneForAll => sup.registry.all_ids(),
        RestartStrategy::RestForOne => {
            let mut ids = vec![failed_child_id.clone()];
            ids.extend(sup.registry.children_after(failed_child_id));
            ids
        }
    };

    StrategyOutcome::Restart(to_restart)
}

// ---------------------------------------------------------------------------
// Unit tests (pub(crate) internals, Rust Book Ch 11.3)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::StopReason;

    // -- RestartBudget -------------------------------------------------------

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

    // -- ChildRegistry -------------------------------------------------------

    fn dummy_child(id: &str) -> ChildState {
        let (tx, _rx) = mpsc::channel(1);
        ChildState {
            id: ActorId::from(id),
            name: Some(id.to_string()),
            spec: ChildSpec {
                restart_type: RestartType::Permanent,
                shutdown: Shutdown::default(),
            },
            join_handle: tokio::spawn(async {}),
            system_tx: tx,
            is_alive: true,
            pending_restart_seq: None,
        }
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

    // -- evaluate_strategy ---------------------------------------------------

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
            StrategyOutcome::Restart(ids) => assert_eq!(ids.len(), 1),
            other => panic!("expected Restart, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[tokio::test]
    async fn strategy_transient_removes_on_graceful() {
        let mut sup = make_sup_state(RestartStrategy::OneForOne);
        let (tx, _rx) = mpsc::channel(1);
        sup.registry.register(ChildState {
            id: ActorId::from("child"),
            name: None,
            spec: ChildSpec {
                restart_type: RestartType::Transient,
                shutdown: Shutdown::default(),
            },
            join_handle: tokio::spawn(async {}),
            system_tx: tx,
            is_alive: true,
            pending_restart_seq: None,
        });
        assert!(matches!(
            evaluate_strategy(&mut sup, &ActorId::from("child"), &StopReason::Graceful),
            StrategyOutcome::Remove
        ));
    }

    #[tokio::test]
    async fn strategy_temporary_always_removes() {
        let mut sup = make_sup_state(RestartStrategy::OneForOne);
        let (tx, _rx) = mpsc::channel(1);
        sup.registry.register(ChildState {
            id: ActorId::from("child"),
            name: None,
            spec: ChildSpec {
                restart_type: RestartType::Temporary,
                shutdown: Shutdown::default(),
            },
            join_handle: tokio::spawn(async {}),
            system_tx: tx,
            is_alive: true,
            pending_restart_seq: None,
        });
        assert!(matches!(
            evaluate_strategy(&mut sup, &ActorId::from("child"), &StopReason::Kill),
            StrategyOutcome::Remove
        ));
    }

    #[tokio::test]
    async fn strategy_one_for_all_restarts_all() {
        let mut sup = make_sup_state(RestartStrategy::OneForAll);
        sup.registry.register(dummy_child("a"));
        sup.registry.register(dummy_child("b"));
        sup.registry.register(dummy_child("c"));
        match evaluate_strategy(&mut sup, &ActorId::from("b"), &StopReason::Kill) {
            StrategyOutcome::Restart(ids) => assert_eq!(ids.len(), 3),
            other => panic!("expected Restart, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[tokio::test]
    async fn strategy_rest_for_one_restarts_after() {
        let mut sup = make_sup_state(RestartStrategy::RestForOne);
        sup.registry.register(dummy_child("a"));
        sup.registry.register(dummy_child("b"));
        sup.registry.register(dummy_child("c"));
        match evaluate_strategy(&mut sup, &ActorId::from("a"), &StopReason::Kill) {
            StrategyOutcome::Restart(ids) => {
                let names: Vec<&str> = ids.iter().map(|id| id.as_str()).collect();
                assert_eq!(names, vec!["a", "b", "c"]);
            }
            other => panic!("expected Restart, got {:?}", std::mem::discriminant(&other)),
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
            StrategyOutcome::Restart(_)
        ));
        // Second restart - budget exhausted
        assert!(matches!(
            evaluate_strategy(&mut sup, &ActorId::from("child"), &StopReason::Kill),
            StrategyOutcome::BudgetExhausted
        ));
    }
}

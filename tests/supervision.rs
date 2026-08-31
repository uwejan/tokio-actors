use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::sleep;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, ChildEvent, RestartStrategy, RestartType, Shutdown, StopReason,
    SupervisionAction, SupervisionConfig,
};

// ---------------------------------------------------------------------------
// Helper actors
// ---------------------------------------------------------------------------

/// A child that fails when told to.
struct CrashOnCommand;

#[derive(Clone)]
enum CrashMsg {
    Ping,
}

impl Actor for CrashOnCommand {
    type Message = CrashMsg;
    type Response = ();

    async fn handle(&mut self, msg: CrashMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            CrashMsg::Ping => Ok(()),
        }
    }
}

/// An actor whose handler blocks until released, giving a test a
/// deterministic window in which the actor's task cannot service its
/// `select!` at all.
struct Hangable;

enum HangMsg {
    Hang(Arc<Notify>, Arc<Notify>),
}

impl Actor for Hangable {
    type Message = HangMsg;
    type Response = ();

    async fn handle(&mut self, msg: HangMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let HangMsg::Hang(started, release) = msg;
        started.notify_one();
        release.notified().await;
        Ok(())
    }
}

/// A supervisor that records child events.
struct Supervisor {
    events: Arc<tokio::sync::Mutex<Vec<ChildEvent>>>,
}

impl Actor for Supervisor {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.events.lock().await.push(event.clone());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests: System channel
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stop_bypasses_full_mailbox() {
    // Create actor with tiny mailbox
    let config = ActorConfig::default().with_mailbox_capacity(1);
    let handle = CrashOnCommand
        .spawn()
        .named("sup-stop-bypass")
        .with_config(config)
        .await
        .unwrap();

    // Fill the mailbox
    let _ = handle.try_notify(CrashMsg::Ping);

    // Stop should still work via system channel
    handle.stop(StopReason::Graceful).await.unwrap();
    sleep(Duration::from_millis(50)).await;
    assert!(!handle.is_alive());
}

#[tokio::test]
async fn get_status_returns_info() {
    let handle = CrashOnCommand
        .spawn()
        .named("sup-status")
        .with_config(ActorConfig::default().with_mailbox_capacity(32))
        .await
        .unwrap();

    let status = handle.get_status().await.unwrap();
    assert_eq!(status.id.as_str(), "sup-status");
    assert_eq!(status.name, Some("sup-status".to_string()));
    assert_eq!(status.mailbox_capacity, 32);
    assert_eq!(status.child_count, 0);

    handle.stop(StopReason::Graceful).await.unwrap();
}

#[tokio::test]
async fn get_status_bypasses_mailbox_queue() {
    let config = ActorConfig::default().with_mailbox_capacity(1);
    let handle = CrashOnCommand
        .spawn()
        .named("sup-status-bypass")
        .with_config(config)
        .await
        .unwrap();

    // Fill the mailbox
    let _ = handle.try_notify(CrashMsg::Ping);

    // get_status should still respond
    let status = handle.get_status().await.unwrap();
    assert_eq!(status.id.as_str(), "sup-status-bypass");

    handle.stop(StopReason::Kill).await.unwrap();
}

/// The stop lane outranks the system channel too, not just the mailbox: with
/// a `get_status()` request already queued on the system channel and a
/// `Kill` raised on the lane, the biased `select!` checks the lane first, so
/// the queued `get_status` is abandoned rather than answered.
#[tokio::test]
async fn kill_on_lane_outranks_a_pending_get_status_on_system_channel() {
    let handle = Hangable
        .spawn()
        .named("sup-lane-outranks-system")
        .await
        .unwrap();

    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    handle
        .notify(HangMsg::Hang(started.clone(), release.clone()))
        .await
        .unwrap();
    started.notified().await; // the actor's task is now fully occupied

    // Queue a get_status request on the system channel while the actor
    // cannot service its select! at all.
    let handle_for_status = handle.clone();
    let status_call = tokio::spawn(async move { handle_for_status.get_status().await });
    sleep(Duration::from_millis(20)).await; // let the send land before Kill

    // Raise Kill on the lane - both are now pending when the handler releases.
    handle.stop(StopReason::Kill).await.unwrap();
    release.notify_one();

    let result = status_call.await.unwrap();
    assert!(
        result.is_err(),
        "a get_status queued alongside a pending Kill must be abandoned, \
         not answered - the lane is checked first"
    );
    handle.wait_stopped().await;
}

// ---------------------------------------------------------------------------
// Tests: OneForOne restart
// ---------------------------------------------------------------------------

#[tokio::test]
async fn supervised_actor_responds_to_get_status() {
    let events = Arc::new(tokio::sync::Mutex::new(Vec::new()));

    let sup_handle = Supervisor {
        events: events.clone(),
    }
    .spawn()
    .named("sup-o4o")
    .with_config(ActorConfig::default().supervisor())
    .await
    .unwrap();

    let status = sup_handle.get_status().await.unwrap();
    assert_eq!(status.child_count, 0);
    assert_eq!(status.name, Some("sup-o4o".to_string()));

    sup_handle.stop(StopReason::Graceful).await.unwrap();
    sleep(Duration::from_millis(50)).await;
}

// ---------------------------------------------------------------------------
// Tests: Restart budget
// ---------------------------------------------------------------------------

#[tokio::test]
async fn supervision_config_accessible_via_get_status() {
    let sup = SupervisionConfig::one_for_one().max_restarts(5, Duration::from_secs(10));
    let handle = Supervisor {
        events: Arc::new(tokio::sync::Mutex::new(Vec::new())),
    }
    .spawn()
    .named("sup-budget")
    .with_config(ActorConfig::default().with_supervision(sup))
    .await
    .unwrap();

    let status = handle.get_status().await.unwrap();
    assert_eq!(status.child_count, 0);

    handle.stop(StopReason::Graceful).await.unwrap();
}

// ---------------------------------------------------------------------------
// Tests: SpawnBuilder integration with supervision
// ---------------------------------------------------------------------------

#[tokio::test]
async fn supervised_actor_has_zero_children_initially() {
    let handle = Supervisor {
        events: Arc::new(tokio::sync::Mutex::new(Vec::new())),
    }
    .spawn()
    .named("sup-zero-children")
    .supervisor()
    .await
    .unwrap();

    let status = handle.get_status().await.unwrap();
    assert_eq!(status.child_count, 0);

    handle.stop(StopReason::Graceful).await.unwrap();
}

#[tokio::test]
async fn supervision_config_strategies() {
    // Test all strategy constructors work
    let configs = vec![
        SupervisionConfig::one_for_one(),
        SupervisionConfig::one_for_all(),
        SupervisionConfig::rest_for_one(),
        SupervisionConfig::simple_one_for_one(),
    ];

    for (i, config) in configs.into_iter().enumerate() {
        let handle = Supervisor {
            events: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
        .spawn()
        .named(format!("sup-strategy-{i}"))
        .with_config(ActorConfig::default().with_supervision(config))
        .await
        .unwrap();

        assert!(handle.is_alive());
        handle.stop(StopReason::Graceful).await.unwrap();
        sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests: ChildEvent structure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn child_event_fields() {
    // Verify ChildEvent has correct structure
    let event = ChildEvent {
        child_id: "test-child".into(),
        child_name: Some("test-child".to_string()),
        reason: StopReason::Graceful,
        action: SupervisionAction::Removed,
    };

    assert_eq!(event.child_id.as_str(), "test-child");
    assert_eq!(event.child_name, Some("test-child".to_string()));
    assert_eq!(event.action, SupervisionAction::Removed);
}

#[tokio::test]
async fn supervision_action_variants() {
    assert_eq!(
        SupervisionAction::RestartInitiated,
        SupervisionAction::RestartInitiated
    );
    assert_eq!(SupervisionAction::Removed, SupervisionAction::Removed);
    assert_eq!(
        SupervisionAction::NotSupervised,
        SupervisionAction::NotSupervised
    );
    assert_eq!(
        SupervisionAction::BudgetExhausted,
        SupervisionAction::BudgetExhausted
    );
    assert_ne!(
        SupervisionAction::RestartInitiated,
        SupervisionAction::Removed
    );
}

// ---------------------------------------------------------------------------
// Tests: Shutdown types
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_default_is_timeout_5s() {
    let shutdown = Shutdown::default();
    assert_eq!(shutdown, Shutdown::Timeout(Duration::from_secs(5)));
}

#[tokio::test]
async fn restart_type_variants() {
    assert_eq!(RestartType::Permanent, RestartType::Permanent);
    assert_eq!(RestartType::Transient, RestartType::Transient);
    assert_eq!(RestartType::Temporary, RestartType::Temporary);
    assert_ne!(RestartType::Permanent, RestartType::Temporary);
}

// ---------------------------------------------------------------------------
// Tests: SupervisionConfig builders (moved from in-source unit tests)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn supervision_config_one_for_one_defaults() {
    let cfg = SupervisionConfig::one_for_one();
    assert_eq!(cfg.strategy, RestartStrategy::OneForOne);
    assert_eq!(cfg.max_restarts, 3);
    assert_eq!(cfg.restart_window, Duration::from_secs(5));
}

#[tokio::test]
async fn supervision_config_one_for_all_with_custom_budget() {
    let cfg = SupervisionConfig::one_for_all().max_restarts(10, Duration::from_secs(60));
    assert_eq!(cfg.strategy, RestartStrategy::OneForAll);
    assert_eq!(cfg.max_restarts, 10);
    assert_eq!(cfg.restart_window, Duration::from_secs(60));
}

#[tokio::test]
async fn supervision_config_rest_for_one() {
    let cfg = SupervisionConfig::rest_for_one();
    assert_eq!(cfg.strategy, RestartStrategy::RestForOne);
}

#[tokio::test]
async fn supervision_config_simple_one_for_one() {
    let cfg = SupervisionConfig::simple_one_for_one();
    assert_eq!(cfg.strategy, RestartStrategy::SimpleOneForOne);
}

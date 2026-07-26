use std::time::Duration;

use tokio::time::sleep;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ChildEvent, RestartType, Shutdown, StopReason, SupervisionConfig,
};

/// A worker that crashes on the 3rd message.
///
/// Returning `Err` from `handle` stops the actor and triggers the
/// supervisor's restart logic, on the `notify` path exactly as on the `send`
/// path (cast-exception parity). This example uses `send()` so the call site
/// also gets to see the failure come back as a response, not just the
/// restart.
#[derive(Default)]
struct UnstableWorker {
    processed: u32,
}

#[derive(Clone)]
enum WorkerMsg {
    DoWork,
}

impl Actor for UnstableWorker {
    type Message = WorkerMsg;
    type Response = String;

    async fn handle(
        &mut self,
        _msg: WorkerMsg,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<String> {
        self.processed += 1;
        if self.processed >= 3 {
            println!("  Worker: crashing on message #{}", self.processed);
            return Err(tokio_actors::ActorError::user("simulated crash"));
        }
        println!("  Worker: processed message #{}", self.processed);
        Ok(format!("done-{}", self.processed))
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        println!("  Worker: started (fresh state, processed=0)");
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        println!("  Worker: stopped ({reason})");
        Ok(())
    }
}

/// A supervisor that spawns and monitors an UnstableWorker.
struct WorkerSupervisor;

impl Actor for WorkerSupervisor {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(UnstableWorker::default)
            .named("worker")
            .restart_type(RestartType::Permanent)
            .shutdown(Shutdown::Timeout(Duration::from_secs(5)))
            .await?;
        println!("Supervisor: spawned worker child");
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        println!(
            "Supervisor: child '{}' stopped -> {:?}",
            event.child_id, event.action
        );
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Supervision Example ===\n");

    // Supervisor: OneForOne strategy, allows 5 restarts in 30s window
    let sup_config = SupervisionConfig::one_for_one().max_restarts(5, Duration::from_secs(30));

    let sup = WorkerSupervisor
        .spawn()
        .named("worker-supervisor")
        .with_supervision(sup_config)
        .await?;

    // Let the supervisor spawn the child
    sleep(Duration::from_millis(50)).await;

    let status = sup.get_status().await?;
    println!("Supervisor status: {} child(ren)\n", status.child_count);

    // --- Round 1: send 3 messages, worker crashes on #3 ---
    println!("--- Round 1: sending work (worker crashes on #3) ---\n");
    let sys = tokio_actors::ActorSystem::default();
    for i in 1..=3 {
        if let Some(worker) = sys.get::<UnstableWorker>("worker") {
            match worker.send(WorkerMsg::DoWork).await {
                Ok(resp) => println!("  [main] Message {i} response: {resp}"),
                Err(e) => println!("  [main] Message {i} error: {e}"),
            }
        } else {
            println!("  [main] Worker not found for message {i}");
        }
        sleep(Duration::from_millis(20)).await;
    }

    // Wait for the restart to complete
    sleep(Duration::from_millis(200)).await;

    // --- Round 2: worker was restarted with fresh state ---
    println!("\n--- Round 2: worker restarted, sending 2 more ---\n");
    for i in 1..=2 {
        if let Some(worker) = sys.get::<UnstableWorker>("worker") {
            match worker.send(WorkerMsg::DoWork).await {
                Ok(resp) => println!("  [main] Message {i} response: {resp}"),
                Err(e) => println!("  [main] Message {i} error: {e}"),
            }
        }
        sleep(Duration::from_millis(20)).await;
    }

    // Final status
    sleep(Duration::from_millis(50)).await;
    let status = sup.get_status().await?;
    println!(
        "\nFinal: {} child(ren), {} timer(s)",
        status.child_count, status.timer_count
    );

    // Graceful shutdown
    sup.stop(StopReason::Graceful).await?;
    sleep(Duration::from_millis(100)).await;
    println!("\nDone.");
    Ok(())
}

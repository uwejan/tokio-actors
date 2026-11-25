//! The supervisor reference and auto shutdown: children are started and
//! listed through a `SupervisorRef`, and the supervisor shuts itself down when
//! its significant child ends gracefully.

use std::time::Duration;

use tokio::time::timeout;
use tokio_actors::actor::{context::ActorContext, Actor, ActorExt};
use tokio_actors::{
    ActorResult, AutoShutdown, RestartType, StopReason, SupervisionConfig, SupervisorRef,
};

struct Worker;

impl Actor for Worker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = SupervisionConfig::one_for_one().with_auto_shutdown(AutoShutdown::AnySignificant);
    let boss = Worker
        .spawn()
        .named("sref-example-boss")
        .with_supervision(config)
        .await?;
    let sup = SupervisorRef::of(&boss);

    let significant = sup
        .start_child(
            || Worker,
            |child| {
                child
                    .named("sref-example-significant")
                    .restart_type(RestartType::Transient)
                    .significant(true)
            },
        )
        .await?;
    sup.start_child(|| Worker, |child| child.named("sref-example-plain"))
        .await?;

    let mut children = sup.which_children().await?;
    children.sort_by(|a, b| a.name.cmp(&b.name));
    for info in &children {
        println!(
            "child {}: {:?}, significant {}",
            info.name.as_deref().unwrap_or("?"),
            info.restart_type,
            info.significant
        );
    }
    let counts = sup.count_children().await?;
    println!("{} specs, {} active", counts.specs, counts.active);

    // A graceful end of a significant transient child is not restarted, and
    // the supervisor ends because of it.
    significant.stop(StopReason::Graceful).await?;
    let reason = timeout(Duration::from_secs(20), boss.wait_stopped()).await?;
    println!("supervisor ended: {reason}");
    Ok(())
}

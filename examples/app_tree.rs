//! An application: one named system with a top supervisor and two workers,
//! started and stopped as a unit.

use std::sync::Arc;

use tokio_actors::application::{AppRoot, Application, Applications, StartError};
use tokio_actors::{
    Actor, ActorContext, ActorExt, ActorResult, ActorSystem, RestartType, SupervisionConfig,
};

struct Worker;

impl Actor for Worker {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

struct Top;

impl Actor for Top {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(|| Worker).named("worker-a").await?;
        ctx.spawn_child(|| Worker).named("worker-b").await?;
        Ok(())
    }
}

struct Shop;

impl Application for Shop {
    fn name(&self) -> &str {
        "shop"
    }

    async fn start(&mut self, system: &Arc<ActorSystem>) -> Result<AppRoot, StartError> {
        let top = Top
            .spawn()
            .on_system(system)
            .named("top")
            .with_supervision(SupervisionConfig::one_for_one())
            .await?;
        Ok(AppRoot::of(&top))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let apps = Applications::spawn().await?;
    apps.load(Shop).await?;
    apps.start("shop", RestartType::Permanent).await?;

    let actors = ActorSystem::get_named("shop").map_or(0, |system| system.snapshot().len());
    let rows = apps.shutdown().await?;
    if rows.iter().all(|(_, s)| matches!(s, Some(s) if !s.held)) {
        println!("application stopped, {actors} actors ended");
    }
    Ok(())
}

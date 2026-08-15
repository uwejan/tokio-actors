use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem,
};

#[derive(Default)]
struct Echo;

impl Actor for Echo {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

// A bare anonymous spawn (no name, no explicit system) now registers with
// `ActorSystem::default()` as a root, matching the behavior named spawns
// already had.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_spawn_registers() {
    let handle = Echo.spawn().await.expect("spawn succeeds");
    let id = handle.id().clone();

    let sys = ActorSystem::default();
    assert!(
        sys.get_by_id::<Echo>(&id).is_some(),
        "bare anonymous spawn must register with the default system"
    );

    drop(handle);

    let recovered = sys
        .get_by_id::<Echo>(&id)
        .expect("registration outlives the original handle");
    let _ = recovered.status();
}

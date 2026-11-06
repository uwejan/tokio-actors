//! A handle in a log shows who it addresses and nothing of its internals,
//! and printing one never asks the actor type to be `Debug`.

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem,
};

/// An actor type that does not implement `Debug`.
struct Quiet;

impl Actor for Quiet {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_handle_prints_its_id_and_its_system_and_nothing_else() {
    let system = ActorSystem::create("handle-debug-system").unwrap();
    let handle = Quiet
        .spawn()
        .named("handle-debug-actor")
        .on_system(&system)
        .await
        .unwrap();

    assert_eq!(
        format!("{handle:?}"),
        r#"ActorHandle { id: ActorId("handle-debug-actor"), system: "handle-debug-system", .. }"#
    );
    assert_eq!(
        format!("{handle:#?}"),
        concat!(
            "ActorHandle {\n",
            "    id: ActorId(\n",
            "        \"handle-debug-actor\",\n",
            "    ),\n",
            "    system: \"handle-debug-system\",\n",
            "    ..\n",
            "}"
        )
    );
}

#[tokio::test]
async fn an_unnamed_actors_handle_prints_its_generated_id() {
    let system = ActorSystem::create("handle-debug-anonymous").unwrap();
    let handle = Quiet.spawn().on_system(&system).await.unwrap();

    let text = format!("{handle:?}");
    let expected = format!(
        r#"ActorHandle {{ id: ActorId("{}"), system: "handle-debug-anonymous", .. }}"#,
        handle.id().as_str()
    );
    assert_eq!(text, expected);
}

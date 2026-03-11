use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult, ActorSystem, SupervisionConfig,
};

#[derive(Default)]
struct Noop;

impl Actor for Noop {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[tokio::test]
async fn anonymous_spawn() {
    let h = Noop.spawn().await.unwrap();
    assert!(h.is_alive());
}

#[tokio::test]
async fn named_spawn() {
    let h = Noop.spawn().named("sb-named").await.unwrap();
    assert!(h.is_alive());
    assert_eq!(h.id().as_str(), "sb-named");
}

#[tokio::test]
async fn named_with_config() {
    let config = ActorConfig::default().with_mailbox_capacity(128);
    let h = Noop
        .spawn()
        .named("sb-named-cfg")
        .with_config(config)
        .await
        .unwrap();
    assert_eq!(h.mailbox_capacity(), 128);
}

#[tokio::test]
async fn on_system() {
    let sys = ActorSystem::create(format!("sb-sys-{}", uuid::Uuid::new_v4())).unwrap();
    let h = Noop.spawn().on_system(&sys).await.unwrap();
    assert!(h.is_alive());
}

#[tokio::test]
async fn named_on_system() {
    let sys = ActorSystem::create(format!("sb-nsys-{}", uuid::Uuid::new_v4())).unwrap();
    let h = Noop.spawn().named("target").on_system(&sys).await.unwrap();
    assert!(h.is_alive());
    assert_eq!(h.id().as_str(), "target");
    assert!(sys.get::<Noop>("target").is_some());
}

#[tokio::test]
async fn named_on_system_with_config() {
    let sys = ActorSystem::create(format!("sb-full-{}", uuid::Uuid::new_v4())).unwrap();
    let config = ActorConfig::default().with_mailbox_capacity(32);
    let h = Noop
        .spawn()
        .named("full-chain")
        .on_system(&sys)
        .with_config(config)
        .await
        .unwrap();
    assert_eq!(h.mailbox_capacity(), 32);
    assert!(sys.get::<Noop>("full-chain").is_some());
}

#[tokio::test]
async fn builder_chain_order_independent() {
    let sys = ActorSystem::create(format!("sb-order-{}", uuid::Uuid::new_v4())).unwrap();
    // named().on_system()
    let h1 = Noop.spawn().named("order-a").on_system(&sys).await.unwrap();
    // on_system().named()
    let h2 = Noop.spawn().on_system(&sys).named("order-b").await.unwrap();
    assert!(h1.is_alive());
    assert!(h2.is_alive());
    assert!(sys.get::<Noop>("order-a").is_some());
    assert!(sys.get::<Noop>("order-b").is_some());
}

#[tokio::test]
async fn supervised_parent_default() {
    let h = Noop.spawn().named("sb-sup").supervised().await.unwrap();
    assert!(h.is_alive());
}

#[tokio::test]
async fn supervised_parent_custom_config() {
    let sup = SupervisionConfig::one_for_all().max_restarts(10, std::time::Duration::from_secs(60));
    let h = Noop
        .spawn()
        .named("sb-sup-custom")
        .with_supervision(sup)
        .await
        .unwrap();
    assert!(h.is_alive());
}

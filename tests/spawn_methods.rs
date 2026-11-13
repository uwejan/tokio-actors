use tokio_actors::actor::context::ActorContext;
use tokio_actors::actor::{Actor, ActorExt};
use tokio_actors::system::ActorSystem;
use tokio_actors::{ActorConfig, ActorResult, SpawnBuilder, SupervisionConfig, SupervisorRef};

#[derive(Debug, Default)]
struct Ping;

impl Actor for Ping {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// `named` and `on_system` may be chained in either order, and `with_config`
/// keeps a supervisor role chosen on the builder in either order.
#[tokio::test]
async fn builder_chain_order_independent() {
    let sys = ActorSystem::create(format!("sb-order-{}", uuid::Uuid::new_v4())).unwrap();
    // named().on_system()
    let h1 = Ping.spawn().named("order-a").on_system(&sys).await.unwrap();
    // on_system().named()
    let h2 = Ping.spawn().on_system(&sys).named("order-b").await.unwrap();
    assert!(h1.is_alive());
    assert!(h2.is_alive());
    assert!(sys.get::<Ping>("order-a").is_some());
    assert!(sys.get::<Ping>("order-b").is_some());

    // `with_config` keeps the supervisor role chosen on the builder, before
    // or after it, unless the config carries its own.
    let cap = || ActorConfig::default().with_mailbox_capacity(32);
    type Chain = fn(SpawnBuilder<Ping>, ActorConfig) -> SpawnBuilder<Ping>;
    let rows: [(&str, Chain, bool); 5] = [
        ("config alone", |b, c| b.with_config(c), false),
        (
            "supervisor, then config",
            |b, c| b.supervisor().with_config(c),
            true,
        ),
        (
            "config, then supervisor",
            |b, c| b.with_config(c).supervisor(),
            true,
        ),
        (
            "with_supervision, then config",
            |b, c| {
                b.with_supervision(SupervisionConfig::one_for_all())
                    .with_config(c)
            },
            true,
        ),
        (
            "supervisor, then a config with its own",
            |b, c| {
                b.supervisor()
                    .with_config(c.with_supervision(SupervisionConfig::one_for_all()))
            },
            true,
        ),
    ];
    for (row, chain, supervisor) in rows {
        let h = chain(Ping.spawn().on_system(&sys), cap()).await.unwrap();
        assert_eq!(h.mailbox_capacity(), 32, "{row}");
        assert_eq!(
            SupervisorRef::of(&h).count_children().await.is_ok(),
            supervisor,
            "{row}"
        );
    }
}

//! A panic raised while another panic unwinds ends the whole process, like a
//! stack overflow or `process::abort`: the actor's `on_stopped` does not run
//! and its supervisor is not told. The scenario runs in a child process of this
//! test binary and the parent checks how the child ended and what it wrote.

#![cfg(unix)]

use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::time::Duration;

use tokio::time::timeout;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorResult, ActorSystem, ChildEvent, RestartType, StopReason,
};

const CHILD_VAR: &str = "TOKIO_ACTORS_TEST_DOUBLE_PANIC_CHILD";
const SIGABRT: i32 = 6;

/// How long the child waits for the process to end before it reports that it
/// survived.
const SURVIVAL_BOUND: Duration = Duration::from_secs(30);

/// Panics when dropped, which happens while the handler's own panic unwinds.
struct PanicsOnDrop;

impl Drop for PanicsOnDrop {
    fn drop(&mut self) {
        panic!("second panic, raised during the unwind of the first");
    }
}

struct Victim;

impl Actor for Victim {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        println!("MARK handler entered");
        let _guard = PanicsOnDrop;
        panic!("first panic");
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        println!("MARK victim on_stopped ran");
        Ok(())
    }
}

struct Supervisor;

impl Actor for Supervisor {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        ctx.spawn_child(|| Victim)
            .named("victim")
            .restart_type(RestartType::Permanent)
            .await?;
        Ok(())
    }

    async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        _event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        println!("MARK supervisor was told");
        Ok(())
    }
}

fn the_scenario() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let sys = ActorSystem::create("double-panic-child").unwrap();
        Supervisor
            .spawn()
            .named("supervisor")
            .on_system(&sys)
            .supervisor()
            .await
            .unwrap();
        let victim = loop {
            if let Some(victim) = sys.get::<Victim>("victim") {
                break victim;
            }
            tokio::task::yield_now().await;
        };
        victim.notify(()).await.unwrap();
        let _ = timeout(SURVIVAL_BOUND, std::future::pending::<()>()).await;
        println!("MARK process survived");
    });
}

#[test]
fn a_panic_during_an_unwind_aborts_the_process_with_no_on_stopped_and_no_supervisor_report() {
    if std::env::var_os(CHILD_VAR).is_some() {
        the_scenario();
        return;
    }
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_panic_during_an_unwind_aborts_the_process_with_no_on_stopped_and_no_supervisor_report",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(CHILD_VAR, "1")
        .output()
        .expect("the child process runs");
    let stdout = String::from_utf8_lossy(&child.stdout);
    assert!(
        stdout.contains("MARK handler entered"),
        "the scenario reached the handler: {stdout}"
    );
    assert_eq!(
        child.status.signal(),
        Some(SIGABRT),
        "the process ended by SIGABRT, got {}: {stdout}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(
        !stdout.contains("MARK victim on_stopped ran"),
        "no on_stopped ran: {stdout}"
    );
    assert!(
        !stdout.contains("MARK supervisor was told"),
        "the supervisor was told nothing: {stdout}"
    );
}

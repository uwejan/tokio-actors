#![cfg(feature = "tracing")]
//! A crash report costs the user's `format_status` call only where a
//! subscriber takes it: with the crash target muted an actor that fails
//! formats nothing and writes no report, and the supervisor's reports, which
//! share the crate's other targets, are unaffected.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorError, ActorResult, ActorSystem,
};

#[path = "common/capture.rs"]
mod capture;
mod common;

struct Fragile(Arc<AtomicUsize>);

impl Actor for Fragile {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Err(ActorError::user("fragile"))
    }

    fn format_status(&self) -> Option<String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Some("state".to_string())
    }
}

#[test]
fn a_muted_crash_target_costs_no_format_status() {
    capture::install_muting(&["tokio_actors::crash"]);
    common::run(2, async {
        let sys = ActorSystem::create("report-sites-muted").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let handle = Fragile(calls.clone())
            .spawn()
            .named("report-sites-fragile")
            .on_system(&sys)
            .await
            .unwrap();
        handle.notify(()).await.unwrap();
        capture::until("the actor's end line", || {
            capture::ended("report-sites-fragile")
        })
        .await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "format_status ran with no subscriber"
        );
        assert!(
            capture::events(|seen| seen.target == "tokio_actors::crash").is_empty(),
            "a report was written for a muted target"
        );
    });
}

//! What the crate does in a build with `panic = "abort"`.
//!
//! The libtest harness needs unwinding, so this target has no harness: its
//! `main` runs the cases itself, and it builds and runs on stable under
//! `RUSTFLAGS="-C panic=abort"`:
//!
//! ```text
//! RUSTFLAGS="-C panic=abort" cargo test --all-features \
//!     --target x86_64-unknown-linux-gnu --test abort_profile
//! ```
//!
//! Under the default profile the target only prints a note and exits 0.
//!
//! A panic in a closure given to `Sys::inspect`, `Sys::replace_state` or
//! `Sys::install`, or in an actor's `format_status`, ends the process under
//! this profile too: the catch that turns it into `SysError::Callback` or a
//! placeholder needs unwinding.
//!
//! A telemetry handler attached with `attach_telemetry` is called without a
//! catch under this profile: a panic of the handler ends the process, as a
//! panic of any hook does.
//!
//! A case that says the process ends runs as a child process: the binary runs
//! itself again with an environment variable that names the case, and the
//! parent checks how the child ended (killed by SIGABRT on Unix, or the exit
//! status the case promises), what it wrote before it ended, and that no actor
//! ran `on_stopped` and no supervisor was told. A case that must not end the
//! process runs in this process, on a thread of its own under the same bound,
//! and checks the outcome it observed; a case that does not end within the
//! bound fails the target.

#[cfg(panic = "unwind")]
fn main() {
    println!("abort_profile: skipped, these checks need a build with `panic = \"abort\"`");
}

#[cfg(panic = "abort")]
fn main() {
    checks::main();
}

#[cfg(panic = "abort")]
mod checks {
    use std::future::{Future, IntoFuture};
    use std::io::Read;
    use std::pin::pin;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll, Waker};
    use std::time::{Duration, Instant};

    use tokio::runtime::{Builder, Runtime};
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
    use tokio::time::timeout;
    use tokio_actors::{
        actor::{context::ActorContext, Actor, ActorExt},
        ActorError, ActorResult, ActorStatus, ActorSystem, ChildEvent, RestartType, Shutdown,
        SpawnError, StopReason, SubtreeObserver, SupervisionAction, SupervisionConfig,
        PANIC_CAPTURE_AVAILABLE,
    };

    /// Names the case a child process runs.
    const CASE_VAR: &str = "TOKIO_ACTORS_ABORT_PROFILE_CASE";
    /// How long the parent waits for a child before it stops it, and for a
    /// case that runs in this process before it fails it.
    const CHILD_BOUND: Duration = Duration::from_secs(90);
    /// How long a case waits for an event it expects.
    const EVENT_BOUND: Duration = Duration::from_secs(20);

    /// The text Tokio panics with when a timer is created on a runtime built
    /// without the time driver.
    const TOKIO_NO_TIMERS: &str = "timers are disabled";
    /// The start of the messages the crate's own entry checks panic with.
    const CRATE_ENTRY_CHECK: &str = "needs the Tokio time driver";
    const CRATE_NO_RUNTIME: &str =
        "needs a Tokio runtime with the time driver on the calling thread";

    type CaseResult = Result<(), String>;

    macro_rules! ensure {
        ($cond:expr, $($arg:tt)+) => {
            if !$cond {
                return Err(format!($($arg)+));
            }
        };
    }

    /// How a child process must end.
    #[derive(Clone, Copy)]
    enum Ends {
        /// Killed by SIGABRT (any abnormal end where signals do not exist).
        Aborted,
        /// Exited with this status code, not by a signal.
        Code(i32),
        /// Exited with status 0.
        #[cfg_attr(not(feature = "tracing"), allow(dead_code))]
        Cleanly,
    }

    struct Child {
        ends: Ends,
        /// Text the child must have written to stderr.
        wrote: &'static [&'static str],
        /// Text the child must not have written to stderr.
        did_not_write: &'static [&'static str],
    }

    enum Kind {
        /// Runs in the test process.
        Here,
        /// Runs in a child process of this binary.
        InChild(Child),
    }

    struct Case {
        name: &'static str,
        kind: Kind,
        run: fn() -> CaseResult,
    }

    /// What no actor and no supervisor may have written when the process ends
    /// under a callback.
    const NO_REPORT: &[&str] = &["MARK victim on_stopped", "MARK supervisor was told"];

    fn cases() -> Vec<Case> {
        #[cfg_attr(not(feature = "tracing"), allow(unused_mut))]
        let mut cases = vec![
            Case {
                name: "panic_capture_is_reported_unavailable",
                kind: Kind::Here,
                run: panic_capture_is_reported_unavailable,
            },
            Case {
                name: "err_based_supervision_restarts_a_failed_child",
                kind: Kind::Here,
                run: err_based_supervision_restarts_a_failed_child,
            },
            Case {
                name: "a_child_placed_on_a_runtime_without_the_driver_is_not_refused",
                kind: Kind::Here,
                run: a_child_placed_on_a_runtime_without_the_driver_is_not_refused,
            },
            Case {
                name: "a_spawn_with_a_start_timeout_and_no_runtime_is_refused_as_any_spawn_is",
                kind: Kind::Here,
                run: a_spawn_with_a_start_timeout_and_no_runtime_is_refused_as_any_spawn_is,
            },
            Case {
                name: "a_handler_panic_ends_the_process_and_reports_to_no_actor",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["MARK trigger", "handler panic"],
                    did_not_write: NO_REPORT,
                }),
                run: a_handler_panic_ends_the_process,
            },
            Case {
                name: "an_inspect_closure_panic_ends_the_process_and_reports_to_no_actor",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["MARK trigger", "inspect panic"],
                    did_not_write: NO_REPORT,
                }),
                run: an_inspect_closure_panic_ends_the_process,
            },
            Case {
                name: "a_replace_state_closure_panic_ends_the_process_and_reports_to_no_actor",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["MARK trigger", "replace_state panic"],
                    did_not_write: NO_REPORT,
                }),
                run: a_replace_state_closure_panic_ends_the_process,
            },
            Case {
                name: "an_installed_function_panic_ends_the_process_and_reports_to_no_actor",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["install panic"],
                    did_not_write: NO_REPORT,
                }),
                run: an_installed_function_panic_ends_the_process,
            },
            Case {
                name: "a_format_status_panic_ends_the_process_and_reports_to_no_actor",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["MARK closure", "format_status panic"],
                    did_not_write: NO_REPORT,
                }),
                run: a_format_status_panic_ends_the_process,
            },
            Case {
                name: "a_stack_overflow_ends_the_process_and_reports_to_no_actor",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["MARK trigger", "overflowed its stack"],
                    did_not_write: NO_REPORT,
                }),
                run: a_stack_overflow_ends_the_process,
            },
            Case {
                name: "an_allocation_failure_ends_the_process_and_reports_to_no_actor",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["MARK trigger", "memory allocation of"],
                    did_not_write: NO_REPORT,
                }),
                run: an_allocation_failure_ends_the_process,
            },
            Case {
                name: "process_abort_ends_the_process_and_reports_to_no_actor",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["MARK trigger"],
                    did_not_write: NO_REPORT,
                }),
                run: process_abort_ends_the_process,
            },
            Case {
                name: "process_exit_ends_the_process_and_reports_to_no_actor",
                kind: Kind::InChild(Child {
                    ends: Ends::Code(7),
                    wrote: &["MARK trigger"],
                    did_not_write: NO_REPORT,
                }),
                run: process_exit_ends_the_process,
            },
            Case {
                name: "a_recurring_actor_timer_on_a_runtime_without_the_driver_ends_the_process",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["MARK timer registered", TOKIO_NO_TIMERS],
                    did_not_write: &[CRATE_ENTRY_CHECK],
                }),
                run: a_recurring_actor_timer_ends_the_process,
            },
            Case {
                name: "a_one_shot_actor_timer_on_a_runtime_without_the_driver_ends_the_process",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["MARK timer registered", TOKIO_NO_TIMERS],
                    did_not_write: &[CRATE_ENTRY_CHECK],
                }),
                run: a_one_shot_actor_timer_ends_the_process,
            },
            Case {
                name: "a_kill_on_a_runtime_without_the_driver_ends_the_process",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["MARK kill requested", TOKIO_NO_TIMERS],
                    did_not_write: &[CRATE_ENTRY_CHECK],
                }),
                run: a_kill_without_the_driver_ends_the_process,
            },
            Case {
                name:
                    "a_start_timeout_on_a_runtime_without_the_driver_ends_the_process_at_its_timer",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["MARK call made", TOKIO_NO_TIMERS],
                    did_not_write: &[CRATE_ENTRY_CHECK],
                }),
                run: a_start_timeout_without_the_driver_ends_the_process,
            },
            Case {
                name: "stop_within_on_a_runtime_without_the_driver_ends_the_process_at_its_timer",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["MARK call made", TOKIO_NO_TIMERS],
                    did_not_write: &[CRATE_ENTRY_CHECK],
                }),
                run: stop_within_without_the_driver_ends_the_process,
            },
            Case {
                name: "kill_by_id_on_a_runtime_without_the_driver_ends_the_process_at_its_timer",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["MARK call made", TOKIO_NO_TIMERS],
                    did_not_write: &[CRATE_ENTRY_CHECK],
                }),
                run: kill_by_id_without_the_driver_ends_the_process,
            },
            Case {
                name: "stop_within_with_no_runtime_current_ends_the_process_at_its_entry",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["MARK call made", CRATE_NO_RUNTIME],
                    did_not_write: &[],
                }),
                run: stop_within_with_no_runtime_ends_the_process,
            },
            Case {
                name: "wait_ended_on_a_runtime_without_the_driver_ends_the_process_at_its_timer",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["MARK call made", TOKIO_NO_TIMERS],
                    did_not_write: &[CRATE_ENTRY_CHECK],
                }),
                run: wait_ended_without_the_driver_ends_the_process,
            },
            Case {
                name: "wait_ended_with_no_runtime_current_ends_the_process_at_its_entry",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["MARK call made", CRATE_NO_RUNTIME],
                    did_not_write: &[],
                }),
                run: wait_ended_with_no_runtime_ends_the_process,
            },
        ];
        #[cfg(feature = "tracing")]
        {
            cases.push(Case {
                name: "a_turn_warning_threshold_on_a_runtime_without_the_driver_ends_the_process",
                kind: Kind::InChild(Child {
                    ends: Ends::Aborted,
                    wrote: &["MARK threshold set", TOKIO_NO_TIMERS],
                    did_not_write: &[CRATE_ENTRY_CHECK],
                }),
                run: a_turn_warning_threshold_ends_the_process,
            });
            cases.push(Case {
                name: "the_abort_warning_is_logged_once_per_process",
                kind: Kind::InChild(Child {
                    ends: Ends::Cleanly,
                    wrote: &["MARK warning logged once"],
                    did_not_write: &[],
                }),
                run: the_abort_warning_is_logged_once_per_process,
            });
        }
        cases
    }

    pub fn main() {
        if let Ok(name) = std::env::var(CASE_VAR) {
            run_in_child(&name);
        }
        // Names given on the command line (after `--`) select the cases to run.
        let wanted: Vec<String> = std::env::args()
            .skip(1)
            .filter(|arg| !arg.starts_with('-'))
            .collect();
        let cases = cases();
        let mut passed = 0usize;
        for case in &cases {
            if !wanted.is_empty() && !wanted.iter().any(|name| case.name.contains(name.as_str())) {
                continue;
            }
            let outcome = match &case.kind {
                Kind::Here => run_here(case.name, case.run),
                Kind::InChild(child) => run_as_child(case.name, child),
            };
            match outcome {
                Ok(()) => {
                    passed += 1;
                    println!("abort_profile: ok   {}", case.name);
                }
                Err(message) => {
                    println!("abort_profile: FAIL {}\n{message}", case.name);
                    std::process::exit(1);
                }
            }
        }
        println!("abort_profile: {passed} cases passed");
    }

    /// Runs a case in this process on a thread of its own and fails it when it
    /// does not end within `CHILD_BOUND`. The target exits right after the
    /// failure is reported, which ends the thread that did not.
    fn run_here(name: &str, run: fn() -> CaseResult) -> CaseResult {
        let (outcome_tx, outcome_rx) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let _ = outcome_tx.send(run());
            })
            .map_err(|err| format!("could not start the case's thread: {err}"))?;
        match outcome_rx.recv_timeout(CHILD_BOUND) {
            Ok(outcome) => {
                let _ = worker.join();
                outcome
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                Err(format!("the case did not end within {CHILD_BOUND:?}"))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err("the case's thread ended without an outcome".to_string())
            }
        }
    }

    // The child process side

    /// Runs the case a parent named. Returning from a case that should have
    /// ended the process exits 0, which the parent reports as a survival.
    fn run_in_child(name: &str) -> ! {
        let Some(case) = cases().into_iter().find(|case| case.name == name) else {
            eprintln!("unknown case {name}");
            std::process::exit(3);
        };
        match (case.run)() {
            Ok(()) => {
                eprintln!("MARK the process is still running");
                std::process::exit(0)
            }
            Err(message) => {
                eprintln!("case failed: {message}");
                std::process::exit(2)
            }
        }
    }

    // The parent side

    fn run_as_child(name: &str, child: &Child) -> CaseResult {
        let exe = std::env::current_exe().map_err(|err| format!("no path to itself: {err}"))?;
        #[cfg(unix)]
        let mut command = {
            // No core file for a child that ends on purpose.
            let mut command = Command::new("sh");
            command
                .args(["-c", "ulimit -c 0 2>/dev/null; exec \"$0\""])
                .arg(&exe);
            command
        };
        #[cfg(not(unix))]
        let mut command = Command::new(&exe);
        let mut process = command
            .env(CASE_VAR, name)
            .env("RUST_BACKTRACE", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("could not start the child: {err}"))?;
        let mut out = process.stdout.take().expect("piped");
        let mut err = process.stderr.take().expect("piped");
        let out_reader = std::thread::spawn(move || {
            let mut text = String::new();
            let mut bytes = Vec::new();
            let _ = out.read_to_end(&mut bytes);
            text.push_str(&String::from_utf8_lossy(&bytes));
            text
        });
        let err_reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = err.read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        });
        let deadline = Instant::now() + CHILD_BOUND;
        let status = loop {
            match process.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    let _ = process.kill();
                    let _ = process.wait();
                    return Err(format!("the child did not end within {CHILD_BOUND:?}"));
                }
                Err(err) => return Err(format!("could not wait for the child: {err}")),
            }
        };
        let stdout = out_reader.join().unwrap_or_default();
        let stderr = err_reader.join().unwrap_or_default();
        let report = format!("status: {status:?}\nstdout:\n{stdout}\nstderr:\n{stderr}");
        let ended_as_promised = match child.ends {
            Ends::Aborted => ended_by_abort(&status),
            Ends::Code(code) => status.code() == Some(code),
            Ends::Cleanly => status.success(),
        };
        ensure!(
            ended_as_promised,
            "the child did not end as promised\n{report}"
        );
        for text in child.wrote {
            ensure!(
                stderr.contains(text),
                "the child did not write {text:?}\n{report}"
            );
        }
        for text in child.did_not_write {
            ensure!(!stderr.contains(text), "the child wrote {text:?}\n{report}");
        }
        Ok(())
    }

    #[cfg(unix)]
    fn ended_by_abort(status: &std::process::ExitStatus) -> bool {
        use std::os::unix::process::ExitStatusExt;
        const SIGABRT: i32 = 6;
        status.signal() == Some(SIGABRT)
    }

    #[cfg(not(unix))]
    fn ended_by_abort(status: &std::process::ExitStatus) -> bool {
        !status.success()
    }

    // Helpers

    fn mark(what: &str) {
        eprintln!("MARK {what}");
    }

    fn unique(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4())
    }

    fn with_timers() -> Runtime {
        Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a runtime with the time driver")
    }

    fn without_timers() -> Runtime {
        Builder::new_current_thread()
            .build()
            .expect("a runtime without the time driver")
    }

    async fn next<T>(receiver: &mut UnboundedReceiver<T>, what: &str) -> Result<T, String> {
        match timeout(EVENT_BOUND, receiver.recv()).await {
            Ok(Some(value)) => Ok(value),
            Ok(None) => Err(format!("the channel for {what} closed")),
            Err(_) => Err(format!("{what} did not arrive within {EVENT_BOUND:?}")),
        }
    }

    // Actors

    struct Idle;

    impl Actor for Idle {
        type Message = ();
        type Response = ();

        async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            Ok(())
        }
    }

    /// Refuses every graceful stop, so a bounded stop of it needs its timer.
    struct Stubborn;

    impl Actor for Stubborn {
        type Message = ();
        type Response = ();

        async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
            false
        }

        async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    enum Trigger {
        Panic,
        StackOverflow,
        AllocationFailure,
        Abort,
        Exit,
        /// Ends the turn at once, so an installed debug function sees it.
        Nothing,
    }

    /// What the parent does to a supervised `Victim` to make it run a closure
    /// that panics.
    #[derive(Clone, Copy)]
    enum Action {
        Send(Trigger),
        Inspect,
        ReplaceState,
        Install,
        FormatStatus,
    }

    /// Set to make the victim's `format_status` panic.
    static FORMAT_STATUS_PANICS: AtomicBool = AtomicBool::new(false);

    /// A supervised child whose handler ends the process in the way a message
    /// names, and that would write a line if its `on_stopped` ran.
    struct Victim;

    #[inline(never)]
    fn recurse(depth: u64) -> u64 {
        let pad = [depth as u8; 4096];
        let below = if std::hint::black_box(true) {
            recurse(depth + 1)
        } else {
            0
        };
        below + u64::from(std::hint::black_box(pad)[0])
    }

    fn overflow_the_stack() -> ! {
        let reached = recurse(std::hint::black_box(0));
        unreachable!("the recursion returned {reached}")
    }

    impl Actor for Victim {
        type Message = Trigger;
        type Response = ();

        async fn handle(
            &mut self,
            trigger: Trigger,
            _ctx: &mut ActorContext<Self>,
        ) -> ActorResult<()> {
            mark("trigger");
            match trigger {
                Trigger::Panic => panic!("handler panic"),
                Trigger::StackOverflow => overflow_the_stack(),
                Trigger::AllocationFailure => {
                    std::alloc::handle_alloc_error(std::alloc::Layout::new::<u64>())
                }
                Trigger::Abort => std::process::abort(),
                Trigger::Exit => std::process::exit(7),
                Trigger::Nothing => Ok(()),
            }
        }

        fn format_status(&self) -> Option<String> {
            if FORMAT_STATUS_PANICS.load(Ordering::SeqCst) {
                mark("closure");
                panic!("format_status panic");
            }
            None
        }

        async fn on_stopped(
            &mut self,
            _reason: &StopReason,
            _ctx: &mut ActorContext<Self>,
        ) -> ActorResult<()> {
            mark("victim on_stopped");
            Ok(())
        }
    }

    /// Supervises a `Victim` and writes a line when it is told of a stop.
    struct VictimSupervisor;

    impl Actor for VictimSupervisor {
        type Message = ();
        type Response = ();

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            ctx.spawn_child(|| Victim)
                .named("victim")
                .restart_type(RestartType::Permanent)
                .shutdown(Shutdown::Timeout(Duration::from_secs(5)))
                .allow_state_access()
                .await?;
            Ok(())
        }

        async fn on_child_stopped(
            &mut self,
            _event: &ChildEvent,
            _ctx: &mut ActorContext<Self>,
        ) -> ActorResult<()> {
            mark("supervisor was told");
            Ok(())
        }

        async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            Ok(())
        }
    }

    /// Does `action` to a supervised `Victim` and waits for the victim to end.
    /// Returns only when the process was not ended, with `Ok` when the victim
    /// ended by itself.
    fn end_the_process_from(action: Action) -> CaseResult {
        let runtime = with_timers();
        runtime.block_on(async {
            let system = ActorSystem::create(unique("victim")).map_err(|err| err.to_string())?;
            VictimSupervisor
                .spawn()
                .named("supervisor")
                .on_system(&system)
                .with_supervision(
                    SupervisionConfig::one_for_one().max_restarts(5, Duration::from_secs(30)),
                )
                .await
                .map_err(|err| format!("the supervisor did not start: {err}"))?;
            let victim = system
                .get::<Victim>("victim")
                .ok_or("the victim is not registered")?;
            let refused = |err: tokio_actors::sys::SysError| format!("the call was refused: {err}");
            match action {
                Action::Send(trigger) => victim
                    .notify(trigger)
                    .await
                    .map_err(|err| format!("the trigger was not accepted: {err}"))?,
                Action::Inspect => {
                    mark("trigger");
                    victim
                        .sys()
                        .inspect(|_| -> () { panic!("inspect panic") })
                        .await
                        .map_err(refused)?;
                }
                Action::ReplaceState => {
                    mark("trigger");
                    victim
                        .sys()
                        .replace_state(|_| panic!("replace_state panic"))
                        .await
                        .map_err(refused)?;
                }
                Action::Install => {
                    victim
                        .sys()
                        .install("boom", |_| panic!("install panic"))
                        .await
                        .map_err(refused)?;
                    victim
                        .notify(Trigger::Nothing)
                        .await
                        .map_err(|err| format!("the trigger was not accepted: {err}"))?;
                }
                Action::FormatStatus => {
                    FORMAT_STATUS_PANICS.store(true, Ordering::SeqCst);
                    victim.get_status().await.map_err(|err| err.to_string())?;
                }
            }
            timeout(EVENT_BOUND, victim.wait_stopped())
                .await
                .map_err(|_| "the victim neither ended the process nor stopped".to_string())?;
            Ok(())
        })
    }

    fn a_handler_panic_ends_the_process() -> CaseResult {
        end_the_process_from(Action::Send(Trigger::Panic))
    }

    fn a_stack_overflow_ends_the_process() -> CaseResult {
        end_the_process_from(Action::Send(Trigger::StackOverflow))
    }

    fn an_allocation_failure_ends_the_process() -> CaseResult {
        end_the_process_from(Action::Send(Trigger::AllocationFailure))
    }

    fn process_abort_ends_the_process() -> CaseResult {
        end_the_process_from(Action::Send(Trigger::Abort))
    }

    fn process_exit_ends_the_process() -> CaseResult {
        end_the_process_from(Action::Send(Trigger::Exit))
    }

    fn an_inspect_closure_panic_ends_the_process() -> CaseResult {
        end_the_process_from(Action::Inspect)
    }

    fn a_replace_state_closure_panic_ends_the_process() -> CaseResult {
        end_the_process_from(Action::ReplaceState)
    }

    fn an_installed_function_panic_ends_the_process() -> CaseResult {
        end_the_process_from(Action::Install)
    }

    fn a_format_status_panic_ends_the_process() -> CaseResult {
        end_the_process_from(Action::FormatStatus)
    }

    // Cases that run in this process

    fn panic_capture_is_reported_unavailable() -> CaseResult {
        ensure!(
            !PANIC_CAPTURE_AVAILABLE,
            "PANIC_CAPTURE_AVAILABLE is true in a build that aborts on panic"
        );
        Ok(())
    }

    /// A worker that fails with an `Err` on request and reports each start.
    struct Worker {
        instance: usize,
        started: tokio::sync::mpsc::UnboundedSender<usize>,
    }

    enum WorkerMessage {
        Fail,
        Instance,
    }

    impl Actor for Worker {
        type Message = WorkerMessage;
        type Response = usize;

        async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            let _ = self.started.send(self.instance);
            Ok(())
        }

        async fn handle(
            &mut self,
            message: WorkerMessage,
            _ctx: &mut ActorContext<Self>,
        ) -> ActorResult<usize> {
            match message {
                WorkerMessage::Fail => Err(ActorError::user("requested failure")),
                WorkerMessage::Instance => Ok(self.instance),
            }
        }
    }

    struct WorkerSupervisor {
        started: tokio::sync::mpsc::UnboundedSender<usize>,
        events: tokio::sync::mpsc::UnboundedSender<ChildEvent>,
    }

    impl Actor for WorkerSupervisor {
        type Message = ();
        type Response = ();

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            let started = self.started.clone();
            let built = Arc::new(AtomicUsize::new(0));
            ctx.spawn_child(move || Worker {
                instance: built.fetch_add(1, Ordering::SeqCst),
                started: started.clone(),
            })
            .named("worker")
            .await?;
            Ok(())
        }

        async fn on_child_stopped(
            &mut self,
            event: &ChildEvent,
            _ctx: &mut ActorContext<Self>,
        ) -> ActorResult<()> {
            let _ = self.events.send(event.clone());
            Ok(())
        }

        async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            Ok(())
        }
    }

    fn err_based_supervision_restarts_a_failed_child() -> CaseResult {
        let runtime = with_timers();
        runtime.block_on(async {
            let system =
                ActorSystem::create(unique("err-restart")).map_err(|err| err.to_string())?;
            let (started_tx, mut started) = unbounded_channel();
            let (events_tx, mut events) = unbounded_channel();
            let _supervisor = WorkerSupervisor {
                started: started_tx,
                events: events_tx,
            }
            .spawn()
            .named("supervisor")
            .on_system(&system)
            .with_supervision(
                SupervisionConfig::one_for_one().max_restarts(5, Duration::from_secs(30)),
            )
            .await
            .map_err(|err| format!("the supervisor did not start: {err}"))?;
            let first = next(&mut started, "the first start").await?;
            ensure!(first == 0, "the first incarnation is {first}, not 0");
            let worker = system
                .get::<Worker>("worker")
                .ok_or("the worker is not registered")?;
            let answer = worker.send(WorkerMessage::Fail).await;
            ensure!(answer.is_err(), "the failing request answered {answer:?}");
            let event = next(&mut events, "the supervisor's report").await?;
            ensure!(
                matches!(event.reason, StopReason::Failure(_)),
                "the child stopped with {:?}, not a failure",
                event.reason
            );
            ensure!(
                event.action == SupervisionAction::RestartInitiated,
                "the supervisor answered {:?}, not a restart",
                event.action
            );
            let second = next(&mut started, "the restarted worker's start").await?;
            ensure!(second == 1, "the second incarnation is {second}, not 1");
            let restarted = system
                .get::<Worker>("worker")
                .ok_or("the restarted worker is not registered")?;
            let instance = restarted
                .send(WorkerMessage::Instance)
                .await
                .map_err(|err| format!("the restarted worker did not answer: {err}"))?;
            ensure!(instance == 1, "the restarted worker is instance {instance}");
            Ok(())
        })
    }

    /// Places a child on `target` and reports how the spawn was answered.
    struct Placer {
        target: tokio::runtime::Handle,
    }

    impl Actor for Placer {
        type Message = ();
        type Response = ();

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            ctx.spawn_child(|| Idle)
                .named("placed")
                .on_runtime(&self.target)
                .await?;
            Ok(())
        }

        async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            Ok(())
        }
    }

    fn a_child_placed_on_a_runtime_without_the_driver_is_not_refused() -> CaseResult {
        let host = with_timers();
        let target = Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("a runtime without the time driver");
        let outcome = host.block_on(async {
            let system = ActorSystem::create(unique("placed")).map_err(|err| err.to_string())?;
            let supervisor = Placer {
                target: target.handle().clone(),
            }
            .spawn()
            .named("placer")
            .on_system(&system)
            .with_supervision(SupervisionConfig::one_for_one())
            .await
            .map_err(|err| format!("the placement was refused: {err}"))?;
            let placed = system
                .get::<Idle>("placed")
                .ok_or("the placed child is not registered")?;
            timeout(EVENT_BOUND, placed.send(()))
                .await
                .map_err(|_| "the placed child did not answer".to_string())?
                .map_err(|err| format!("the placed child refused a request: {err}"))?;
            // A graceful stop sleeps on the supervisor's runtime, not on the
            // target, so the child ends without a timer there.
            supervisor
                .stop(StopReason::Graceful)
                .await
                .map_err(|err| format!("the supervisor refused a stop: {err}"))?;
            timeout(EVENT_BOUND, supervisor.wait_stopped())
                .await
                .map_err(|_| "the supervisor did not stop".to_string())?;
            ensure!(
                placed.status() == ActorStatus::Stopped,
                "the placed child reads {:?} after its supervisor stopped",
                placed.status()
            );
            Ok::<(), String>(())
        });
        drop(host);
        drop(target);
        outcome
    }

    fn a_spawn_with_a_start_timeout_and_no_runtime_is_refused_as_any_spawn_is() -> CaseResult {
        let mut spawn = pin!(Idle
            .spawn()
            .start_timeout(Duration::from_secs(5))
            .into_future());
        let mut cx = Context::from_waker(Waker::noop());
        match spawn.as_mut().poll(&mut cx) {
            Poll::Ready(result) => {
                let error = result.err();
                ensure!(
                    error == Some(SpawnError::MissingRuntime),
                    "the spawn answered {error:?}"
                );
                Ok(())
            }
            Poll::Pending => Err("a spawn with no runtime current did not answer at once".into()),
        }
    }

    // Cases that end the process on a runtime without the time driver

    struct Ticker {
        one_shot: bool,
    }

    impl Actor for Ticker {
        type Message = ();
        type Response = ();

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            if self.one_shot {
                ctx.schedule(()).after(Duration::from_millis(1)).await?;
            } else {
                ctx.every_with(Duration::from_millis(1), || ()).await?;
            }
            mark("timer registered");
            Ok(())
        }

        async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            Ok(())
        }
    }

    /// Spawns a timer actor on a runtime without the driver and keeps that
    /// runtime busy long enough for the timer's task to be polled.
    fn a_timer_on_a_runtime_without_the_driver(one_shot: bool) -> CaseResult {
        let runtime = without_timers();
        runtime.block_on(async {
            Ticker { one_shot }
                .spawn()
                .await
                .map_err(|err| format!("the actor did not start: {err}"))?;
            for _ in 0..1000 {
                tokio::task::yield_now().await;
            }
            Ok(())
        })
    }

    fn a_recurring_actor_timer_ends_the_process() -> CaseResult {
        a_timer_on_a_runtime_without_the_driver(false)
    }

    fn a_one_shot_actor_timer_ends_the_process() -> CaseResult {
        a_timer_on_a_runtime_without_the_driver(true)
    }

    fn a_kill_without_the_driver_ends_the_process() -> CaseResult {
        let runtime = without_timers();
        runtime.block_on(async {
            let actor = Idle
                .spawn()
                .await
                .map_err(|err| format!("the actor did not start: {err}"))?;
            mark("kill requested");
            actor
                .stop(StopReason::Kill)
                .await
                .map_err(|err| format!("the kill was refused: {err}"))?;
            actor.wait_stopped().await;
            Ok(())
        })
    }

    fn a_start_timeout_without_the_driver_ends_the_process() -> CaseResult {
        let runtime = without_timers();
        runtime.block_on(async {
            mark("call made");
            Idle.spawn()
                .start_timeout(Duration::from_secs(30))
                .await
                .map_err(|err| format!("the spawn answered {err}"))?;
            Ok(())
        })
    }

    fn stop_within_without_the_driver_ends_the_process() -> CaseResult {
        let host = with_timers();
        let actor = host
            .block_on(Stubborn.spawn().into_future())
            .map_err(|err| format!("the actor did not start: {err}"))?;
        let caller = without_timers();
        caller.block_on(async {
            mark("call made");
            let outcome = actor
                .stop_within(StopReason::Graceful, Duration::from_secs(30))
                .await;
            Err(format!("the call returned {outcome:?}"))
        })
    }

    fn kill_by_id_without_the_driver_ends_the_process() -> CaseResult {
        let host = with_timers();
        let system = ActorSystem::create(unique("kill-by-id")).map_err(|err| err.to_string())?;
        let actor = host
            .block_on(Idle.spawn().on_system(&system).into_future())
            .map_err(|err| format!("the actor did not start: {err}"))?;
        let caller = without_timers();
        caller.block_on(async {
            mark("call made");
            let report = system.kill_by_id(actor.id()).await;
            Err(format!("the call returned {report:?}"))
        })
    }

    fn stop_within_with_no_runtime_ends_the_process() -> CaseResult {
        let host = with_timers();
        let actor = host
            .block_on(Stubborn.spawn().into_future())
            .map_err(|err| format!("the actor did not start: {err}"))?;
        mark("call made");
        let mut call = pin!(actor.stop_within(StopReason::Graceful, Duration::from_secs(30)));
        let mut cx = Context::from_waker(Waker::noop());
        let polled = call.as_mut().poll(&mut cx);
        Err(format!(
            "the call was polled and answered {}",
            match polled {
                Poll::Ready(_) => "ready",
                Poll::Pending => "pending",
            }
        ))
    }

    /// Parks in its handler for good once it gets a message.
    struct Wedged;

    static WEDGED_ENTERED: AtomicBool = AtomicBool::new(false);

    impl Actor for Wedged {
        type Message = ();
        type Response = ();

        async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            WEDGED_ENTERED.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
            Ok(())
        }
    }

    /// A supervisor with one named child, which gets a long kill grace.
    struct Overseer {
        child: String,
    }

    impl Actor for Overseer {
        type Message = ();
        type Response = ();

        async fn handle(&mut self, _: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            Ok(())
        }

        async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
            ctx.spawn_child(|| Wedged)
                .named(self.child.clone())
                .kill_grace(Duration::from_secs(300))
                .await?;
            Ok(())
        }
    }

    /// A supervisor that has been killed while its child is parked in its
    /// handler: the observer of the supervisor's subtree, and the runtime that
    /// hosts the actors.
    fn an_ended_actor_with_a_live_child() -> Result<(SubtreeObserver, Runtime), String> {
        let host = with_timers();
        let system = ActorSystem::create(unique("wait-ended")).map_err(|err| err.to_string())?;
        let observer = host.block_on(async {
            let child = unique("wedged");
            let boss = Overseer {
                child: child.clone(),
            }
            .spawn()
            .on_system(&system)
            .supervisor()
            .await
            .map_err(|err| format!("the supervisor did not start: {err}"))?;
            let wedged = system
                .get::<Wedged>(&child)
                .ok_or_else(|| "the child is not registered".to_string())?;
            wedged
                .notify(())
                .await
                .map_err(|err| format!("the child did not take its message: {err}"))?;
            let started = Instant::now();
            while !WEDGED_ENTERED.load(Ordering::SeqCst) {
                ensure!(
                    started.elapsed() < EVENT_BOUND,
                    "the child did not enter its handler"
                );
                tokio::task::yield_now().await;
            }
            let observer = boss.observe_subtree();
            system
                .kill_by_id(boss.id())
                .await
                .ok_or_else(|| "the supervisor was not registered".to_string())?;
            Ok::<_, String>(observer)
        })?;
        Ok((observer, host))
    }

    fn wait_ended_without_the_driver_ends_the_process() -> CaseResult {
        let (observer, _host) = an_ended_actor_with_a_live_child()?;
        let caller = without_timers();
        caller.block_on(async {
            mark("call made");
            let end = observer.wait_ended().await;
            Err(format!("the call returned {end:?}"))
        })
    }

    fn wait_ended_with_no_runtime_ends_the_process() -> CaseResult {
        let (observer, _host) = an_ended_actor_with_a_live_child()?;
        mark("call made");
        let mut call = pin!(observer.wait_ended());
        let mut cx = Context::from_waker(Waker::noop());
        // The supervisor's task may still be letting go of its status plane
        // and its hold on the subtree at the first poll, and the call then
        // waits for that in its first part, which needs no timer: poll until
        // it reaches the part that does.
        let started = Instant::now();
        loop {
            if let Poll::Ready(_) = call.as_mut().poll(&mut cx) {
                return Err("the call was polled and answered ready".to_string());
            }
            ensure!(
                started.elapsed() < EVENT_BOUND,
                "the call was still pending after {EVENT_BOUND:?}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    // Cases that need the tracing feature

    #[cfg(feature = "tracing")]
    fn a_turn_warning_threshold_ends_the_process() -> CaseResult {
        let runtime = without_timers();
        runtime.block_on(async {
            let system = ActorSystem::create(unique("sweep")).map_err(|err| err.to_string())?;
            system.set_turn_warning_threshold(Some(Duration::from_millis(100)));
            mark("threshold set");
            for _ in 0..1000 {
                tokio::task::yield_now().await;
            }
            Ok(())
        })
    }

    /// Records the message of every event.
    #[cfg(feature = "tracing")]
    struct Recorder {
        lines: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[cfg(feature = "tracing")]
    struct MessageOf<'a>(&'a mut String);

    #[cfg(feature = "tracing")]
    impl tracing::field::Visit for MessageOf<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                *self.0 = format!("{value:?}");
            }
        }
    }

    #[cfg(feature = "tracing")]
    impl tracing::Subscriber for Recorder {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut message = String::new();
            event.record(&mut MessageOf(&mut message));
            self.lines
                .lock()
                .expect("recorder lock")
                .push(format!("{} {message}", event.metadata().level()));
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Runs in a child process of its own, because the warning is logged once
    /// per process and any earlier system would have used it up.
    #[cfg(feature = "tracing")]
    fn the_abort_warning_is_logged_once_per_process() -> CaseResult {
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        tracing::subscriber::set_global_default(Recorder {
            lines: lines.clone(),
        })
        .map_err(|err| format!("a subscriber is already set: {err}"))?;
        let _first = ActorSystem::create(unique("warn-a")).map_err(|err| err.to_string())?;
        let _second = ActorSystem::create(unique("warn-b")).map_err(|err| err.to_string())?;
        let _default = ActorSystem::default();
        let lines = lines.lock().expect("recorder lock").clone();
        let warnings: Vec<_> = lines
            .iter()
            .filter(|line| line.contains("terminate the process"))
            .collect();
        ensure!(
            warnings.len() == 1,
            "the warning was logged {} times: {lines:?}",
            warnings.len()
        );
        ensure!(
            warnings[0].starts_with("WARN ")
                && warnings[0].contains("panic = \"abort\"")
                && warnings[0].contains("crash-visible supervision are unavailable"),
            "the warning reads {:?}",
            warnings[0]
        );
        mark("warning logged once");
        Ok(())
    }
}

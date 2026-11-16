//! Applications: a tree started and stopped as one unit.
//!
//! An application is a top actor, usually a supervisor, in a named
//! [`ActorSystem`]. A controller starts applications in dependency order,
//! stops them in reverse start order, and
//! says when the end of one application should end the program. The crate never
//! exits the process: the binary awaits [`Applications::node_shutdown`].
//!
//! What belongs to an application is its top actor's subtree and, in a
//! system of its own ([`AppSystem::Own`]), every actor registered in that
//! system. An application in a shared system owns its top actor's subtree
//! alone. An actor spawned without `on_system` joins the default system and is
//! part of no application: there is no group leader.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Waker};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{oneshot, watch};
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::actor::context::ActorContext;
use crate::actor::handle::{ActorHandle, SubtreeObserver};
use crate::actor::incarnation::status_cell::StatusTx;
use crate::actor::incarnation::IncarnationRef;
use crate::actor::incarnation::StatusRx;
use crate::actor::monitor::{signal_plane, PlaneTarget};
use crate::actor::panic::{catch_hook_t, catch_sync, discard, payload_into_string, send_contained};
use crate::actor::runtime::{end_value, saturating_deadline};
use crate::actor::supervision::{ABORT_SETTLE, DEFAULT_KILL_GRACE};
use crate::actor::{Actor, ActorExt};
use crate::error::{ActorResult, SpawnError};
use crate::helper::{HelperKind, HelperOwner};
use crate::system::{read_status, terminal_report, wait_closed, wait_terminal, ActorSystem};
use crate::time_probe::runtime_has_timers;
use crate::types::{
    ActorId, ActorStatus, DownReason, RestartType, ShutdownReport, StopOutcome, StopReason,
    StopReport, SubtreeEnd,
};

/// What an application's `start` fails with.
pub type StartError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// The system an application runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppSystem {
    /// A system named as the application, created by the controller with the
    /// default policy (a name another system holds fails the start with
    /// [`AppError::SystemTaken`]), shut down at each stop and after a failed
    /// start, reactivated at the next start.
    Own,
    /// The system of this name, which other applications and the consumer's
    /// own actors may use; one that does not exist is created with the default
    /// configuration, and the system of an application of its own that this
    /// controller created fails the start with [`AppError::SystemTaken`].
    /// Never shut down by the controller; after a failed start
    /// it stops nothing there: `start` stops what it spawned before it returns
    /// an error, and after a refused handle what it spawned is left running.
    Shared(String),
}

/// One application: how its tree starts and what runs around its stop.
pub trait Application: Send + 'static {
    /// The application's name.
    fn name(&self) -> &str;

    /// The system the application runs in: one of its own by default.
    fn system(&self) -> AppSystem {
        AppSystem::Own
    }

    /// The applications that must be running before this one starts.
    fn dependencies(&self) -> Vec<String> {
        Vec::new()
    }

    /// Starts the top actor in `system` and returns it. The actor given
    /// to [`AppRoot::of`] is a root spawned in `system` that no start has
    /// taken, one spawned before the call included. A panic, in the call or
    /// in its future, fails the start with [`AppError::StartFailed`].
    fn start(
        &mut self,
        system: &Arc<ActorSystem>,
    ) -> impl Future<Output = Result<AppRoot, StartError>> + Send;

    /// Runs before the top is stopped, or after the top ended on its own. A
    /// panic, in the call or in its future, is caught and the stop goes on.
    /// A stop lost to a fault of the crate runs no hook it had not reached;
    /// `prep_stop` has already run when the loss comes after it. It does not
    /// run for an application at rest when its controller is killed; the crate
    /// bounds no hook.
    fn prep_stop(&mut self) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// Runs once the top's subtree has ended or the stop stopped waiting for
    /// it ([`AppStop::held`]); in a system of its own before that system's
    /// shutdown, so its other roots may still run. A panic, in the call or in
    /// its future, is caught and the stop goes on. A stop lost to a fault of
    /// the crate runs no hook it had not reached. It does not run for an
    /// application at rest when its controller is killed; the crate bounds no
    /// hook.
    fn stop(&mut self) -> impl Future<Output = ()> + Send {
        async {}
    }
}

/// The top actor of a started application.
#[derive(Debug)]
pub struct AppRoot {
    id: ActorId,
    /// The incarnation's plane, which carries its block.
    plane: StatusRx,
}

impl AppRoot {
    /// Names one incarnation, the one behind `handle`. The start succeeds when
    /// it is a root of the application's system that no start took before,
    /// whatever its status then; a top already ended gives a start that
    /// succeeds and its end at once, with its own reason. Anything else fails
    /// the start with [`AppError::StartFailed`].
    pub fn of<S: Actor>(handle: &ActorHandle<S>) -> Self {
        Self {
            id: handle.id().clone(),
            plane: handle.status_rx(),
        }
    }
}

/// One row of [`Applications::which_applications`].
#[derive(Debug, Clone, PartialEq)]
pub struct AppInfo {
    /// The application's name.
    pub name: String,
    /// The system it runs in.
    pub system: String,
    /// What its end means for the others: the kind of the latest start the
    /// controller issued, `None` until one is; a start refused as it begins (no
    /// time driver, a name taken) sets it too.
    pub kind: Option<RestartType>,
    /// Where it is in its life.
    pub state: AppState,
}

/// Where a loaded application is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppState {
    /// Loaded and not running.
    Loaded,
    /// Its start is in flight.
    Starting,
    /// Its top actor is running.
    Running,
    /// Its stop is in flight.
    Stopping,
    /// Not running, and something it owns (its top's subtree, or any actor of
    /// its own system) has not released its hold; every call reads it again
    /// and finds it `Loaded` once that has: poll
    /// [`Applications::which_applications`] to wait.
    Held,
}

/// The top's stop, for an own system its shutdown, and `held`.
#[derive(Debug, Clone, PartialEq)]
pub struct AppStop {
    /// The report of the top's stop.
    pub top: StopReport,
    /// The report of the system's shutdown, for an application in a system of
    /// its own.
    pub system: Option<ShutdownReport>,
    /// The application was left [`AppState::Held`], something it owns not
    /// having ended within the stop's bounds.
    pub held: bool,
}

/// What happened to the application that set the verdict.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeCause {
    /// The application's top ended on its own with this reason.
    Ended(StopReason),
    /// The application's start failed. The text is its error: the verdict is
    /// cloned to every waiter, and the error itself is not `Clone`.
    StartFailed(String),
}

/// The verdict [`Applications::node_shutdown`] resolves with.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeShutdown {
    /// The application whose end or failed start set the verdict.
    pub application: String,
    /// What happened to it.
    pub cause: NodeCause,
    /// What the node drain stopped, in its order, `None` for a stop lost to a
    /// fault of the crate.
    pub stopped: Vec<(String, Option<AppStop>)>,
}

/// Why an application call did not succeed. It is neither `Clone` nor
/// `PartialEq`, because a failed start carries the application's own error.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// No application of that name is loaded.
    #[error("application {0} is not loaded")]
    NotLoaded(String),
    /// An application of that name is loaded already.
    #[error("application {0} is already loaded")]
    AlreadyLoaded(String),
    /// The application is running already.
    #[error("application {0} is already started")]
    AlreadyStarted(String),
    /// The application is not running.
    #[error("application {0} is not started")]
    NotStarted(String),
    /// A start is in flight in the application's system, of this application
    /// or of another one there: one start or stop runs per system at a time.
    /// The call answers at once and does not wait for that start's reply.
    #[error("a start is in flight in the system of application {0}")]
    StartInFlight(String),
    /// A stop is in flight in the application's system, or a stop of every
    /// running application (`stop_all`, `shutdown` or a node verdict) is queued
    /// or running.
    #[error("a stop is in flight in the system of application {0}")]
    StopInFlight(String),
    /// The application asks for a system of its own, and a system of its name
    /// exists that this controller did not make for it; or it asks to share the
    /// system of an application of its own that this controller created. It
    /// refuses with no node effect.
    #[error("application {0} names a system it cannot have: that name is taken")]
    SystemTaken(String),
    /// A dependency is not running, and `start` starts none. It refuses with
    /// no node effect.
    #[error("application {app} needs {dependency}, which is not running")]
    DependencyNotRunning {
        /// The application asked for.
        app: String,
        /// The dependency that is not running.
        dependency: String,
    },
    /// `start` failed or panicked, returned a handle that is no untaken root
    /// of its system, its system was shutting down, or the start was lost;
    /// `lost` names the applications this call started whose rollback stop
    /// was lost.
    #[error("application {app} failed to start: {error}")]
    StartFailed {
        /// The application whose `start` failed, which may be a dependency of
        /// the one asked for.
        app: String,
        /// What its `start` returned, or why the start failed.
        error: StartError,
        /// The applications this call started whose rollback stop was lost.
        lost: Vec<String>,
    },
    /// The caller's bound passed; a request queued before it is still served.
    #[error("the call timed out")]
    Timeout,
    /// The call is bounded, by [`Applications::timeout`] or by the default of
    /// [`Applications::which_applications`], on a runtime without a time
    /// driver; [`Applications::unbounded`] lifts the bound. A start on such a
    /// runtime is refused with it as well, since the start's and the stop's
    /// steps wait under timers; the application stays loaded, with no node
    /// effect.
    #[error("no time driver")]
    NoTimeDriver,
    /// The application is [`AppState::Held`]: a start, stop or unload of a
    /// held application answers at once.
    #[error("application {0} is held: something it owns has not ended")]
    Held(String),
    /// The stop was lost to a fault of the crate: the top killed (its `Kill`
    /// raised, or its abort armed when it was already ending) and, for an own
    /// system, its shutdown begun; hooks still owed do not run; the
    /// application reads [`AppState::Held`] until what it owns has ended, then
    /// `Loaded`, and can be started again.
    #[error("the stop of application {0} was lost")]
    StopLost(String),
    /// The controller has ended with this reason.
    #[error("the application controller has ended: {0:?}")]
    Ended(StopReason),
    /// The call was made from the controller's own task.
    #[error("called from the controller's own task")]
    CallingSelf,
}

/// A reference to one application controller.
///
/// A controller belongs to whoever spawned it: a program usually has one, and
/// each test has its own. Waits are unbounded by default, except
/// [`Self::which_applications`], which waits 5 seconds; [`Self::timeout`] puts
/// one bound on every call made through the reference but
/// [`Self::node_shutdown`], and [`Self::unbounded`] lifts every bound.
///
/// One start or stop runs per system at a time. The controller is ended with
/// [`Self::shutdown`]. Ended by any other reason but `Kill`, it moves its
/// state to one wind-down task before it waits on anything: the wind-down
/// waits for every start or stop in flight, stops every running application
/// in reverse start order and forgets every application, and an abort of the
/// controller's task cuts only the controller's wait for it. Ended by `Kill`,
/// by a panic of its own turn, aborted before that stop began, or at its
/// runtime's end, the controller drops its state: the top of every running
/// application is killed and each system of an application's own is shut
/// down (not at the runtime's end), and no hook of an application at rest
/// runs. A start or stop in flight keeps what it holds on every path but the end
/// of its runtime, and runs to its end, hooks included; at the end of its
/// runtime its hold fires: the top of a stop in flight, or the top a start has
/// already returned, is killed, and no hook runs. A call made while the
/// controller ends waits for its wind-down, then answers
/// [`AppError::Ended`], so a hook that calls its own controller then, or calls
/// [`Self::stop_all`] or [`Self::shutdown`] at any time, does not end unless
/// bounded.
#[derive(Debug, Clone)]
pub struct Applications {
    timeout: Option<Duration>,
    unbounded: bool,
    controller: ActorHandle<Controller>,
    verdict: watch::Receiver<Verdict>,
}

impl Applications {
    /// Spawns a new controller on the current runtime.
    pub async fn spawn() -> Result<Self, SpawnError> {
        let (verdict_tx, verdict) = watch::channel(Verdict::Pending);
        let controller = ActorExt::spawn(Controller::new(verdict_tx)).await?;
        Ok(Self {
            timeout: None,
            unbounded: false,
            controller,
            verdict,
        })
    }

    /// Bounds every call made through this reference but
    /// [`Self::node_shutdown`].
    #[must_use]
    pub fn timeout(mut self, bound: Duration) -> Self {
        self.timeout = Some(bound);
        self.unbounded = false;
        self
    }

    /// Lifts every bound of this reference, the default of
    /// [`Self::which_applications`] included, so no call needs a time driver
    /// for its bound; a start still needs one.
    #[must_use]
    pub fn unbounded(mut self) -> Self {
        self.timeout = None;
        self.unbounded = true;
        self
    }

    /// Makes an application known to the controller without starting it.
    pub async fn load<A: Application>(&self, app: A) -> Result<(), AppError> {
        let call = Call::Load {
            name: app.name().to_owned(),
            system: app.system(),
            deps: app.dependencies(),
        };
        self.ask(call, Some(Box::new(app)), None).await?.unit()
    }

    /// Forgets a loaded application and drops its value. A running one answers
    /// [`AppError::AlreadyStarted`], a held one [`AppError::Held`] and one with
    /// a start or stop in flight [`AppError::StartInFlight`] or
    /// [`AppError::StopInFlight`].
    pub async fn unload(&self, name: &str) -> Result<(), AppError> {
        self.ask(Call::Unload(name.to_owned()), None, None)
            .await?
            .unit()
    }

    /// Starts a loaded application. Every dependency must be running. A running
    /// one answers [`AppError::AlreadyStarted`], a held one
    /// [`AppError::Held`], and a start or stop in flight in its system answers
    /// [`AppError::StartInFlight`] or [`AppError::StopInFlight`] at once. Its
    /// unbounded part is the application's `start`.
    pub async fn start(&self, name: &str, kind: RestartType) -> Result<(), AppError> {
        self.ask(Call::Start(name.to_owned(), kind), None, None)
            .await?
            .unit()
    }

    /// Starts a loaded application and, first, each loaded dependency that is
    /// not running, in order and with the same `kind`. Returns the names it
    /// started, so one that already runs is left out. On a failure it stops what
    /// it started, in reverse. Its unbounded parts are the `start` of each
    /// application it starts and, in the stops of a failure, the hooks and a
    /// subtree under a `Shutdown::Infinity` child or a trapping worker under
    /// one.
    pub async fn ensure_all_started(
        &self,
        name: &str,
        kind: RestartType,
    ) -> Result<Vec<String>, AppError> {
        match self
            .ask(Call::Ensure(name.to_owned(), kind), None, None)
            .await?
        {
            Answer::Names(result) => result,
            other => other.mismatch(),
        }
    }

    /// Stops a running application: `prep_stop`; the top's stop, as a
    /// system's shutdown stops a root (a `ParentRequest` its `pre_stop` may
    /// veto until the per-root timeout of its system's `ShutdownPolicy`, then
    /// `Kill`, then an abort); a wait for the top's subtree; `stop`; then, for
    /// a system of its own, the shutdown of that system and a wait for every
    /// actor of it to let go of what it held. No other application is
    /// stopped. Its unbounded parts are the hooks and a subtree under a
    /// `Shutdown::Infinity` child or a trapping worker under one. What has not
    /// ended within the bounds sets [`AppStop::held`]: the steps after it
    /// still run, and the application reads [`AppState::Held`] until it has
    /// ended.
    ///
    /// When an application's top ends on its own, both hooks run at that end:
    /// while they run its stop answers [`AppError::StopInFlight`], then
    /// [`AppError::NotStarted`], or [`AppError::Held`] while what it owns has
    /// not ended.
    pub async fn stop(&self, name: &str) -> Result<AppStop, AppError> {
        match self.ask(Call::Stop(name.to_owned()), None, None).await? {
            Answer::Stop(result) => result,
            other => other.mismatch(),
        }
    }

    /// Stops every running application in reverse start order, after waiting
    /// for every start or stop in flight. A row is `None` for a stop lost to a
    /// fault of the crate. Its unbounded parts are the hooks and a subtree
    /// under a `Shutdown::Infinity` child or a trapping worker under one.
    pub async fn stop_all(&self) -> Result<Vec<(String, Option<AppStop>)>, AppError> {
        self.ask(Call::StopAll, None, None).await?.stops()
    }

    /// Stops every running application as [`Self::stop_all`] does, forgets
    /// every loaded one, held ones included, dropping its value, and ends the
    /// controller. Later calls answer [`AppError::Ended`]. Its unbounded parts
    /// are those of [`Self::stop_all`].
    pub async fn shutdown(&self) -> Result<Vec<(String, Option<AppStop>)>, AppError> {
        self.ask(Call::Shutdown, None, None).await?.stops()
    }

    /// The loaded applications, in load order, each with its state.
    pub async fn which_applications(&self) -> Result<Vec<AppInfo>, AppError> {
        match self.ask(Call::Which, None, Some(WHICH_BOUND)).await? {
            Answer::Which(result) => result,
            other => other.mismatch(),
        }
    }

    /// Resolves with the node verdict: the first application whose end or
    /// failed start, by its kind, ends the node, once per controller. The
    /// controller then stops the other running applications in reverse start
    /// order, after every start or stop in flight, and lists them in
    /// [`NodeShutdown::stopped`].
    ///
    /// | What happened | Permanent | Transient | Temporary |
    /// |---|---|---|---|
    /// | `start` failed or panicked, returned a handle that is no untaken root of its system, found its system shutting down, or was lost | ends the node | ends the node | no |
    /// | the top ended on its own with `StopReason::Graceful` | ends the node | no | no |
    /// | the top ended on its own with any other reason | ends the node | ends the node | no |
    /// | a start refused before it began (a dependency, a start or stop in flight, a drain, a held application, no time driver, a system name taken) | no | no | no |
    /// | a stop the controller ordered, an end seen after it was accepted, or a lost stop | no | no | no |
    ///
    /// Nothing that happens after the controller began to close (it accepted
    /// a [`Self::shutdown`], or the wind-down of its own stop began) sets it;
    /// a top's end that is delivered to the controller as it stops, before
    /// that wind-down begins, still does. A
    /// user's shutdown of the application's system reads as the top's own end.
    ///
    /// No bound applies, the reference's included. It resolves with the
    /// verdict, or with [`AppError::Ended`] and the controller's own end
    /// reason once the controller has ended and its wind-down has finished,
    /// while a start or stop in flight may still run.
    ///
    /// A binary awaits it, beside its own stop signal:
    ///
    /// ```no_run
    /// # use tokio_actors::application::Applications;
    /// # async fn signal() {}
    /// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// let apps = Applications::spawn().await?;
    /// // load and start the applications here
    /// tokio::select! {
    ///     verdict = apps.node_shutdown() => eprintln!("node shutdown: {verdict:?}"),
    ///     () = signal() => {}
    /// }
    /// apps.shutdown().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn node_shutdown(
        &self,
    ) -> impl Future<Output = Result<NodeShutdown, AppError>> + Send + 'static {
        let mut verdict = self.verdict.clone();
        let controller = self.controller.clone();
        async move {
            loop {
                let now = verdict.borrow_and_update().clone();
                if let Some(answer) = now.answer() {
                    return answer;
                }
                if verdict.changed().await.is_err() {
                    let last = verdict.borrow().clone();
                    return match last.answer() {
                        Some(answer) => answer,
                        None => Err(AppError::Ended(controller.wait_stopped().await)),
                    };
                }
            }
        }
    }

    /// Queues `call` with the controller and waits for its answer, under the
    /// bound of this reference or `default`. A closed mailbox or an answer
    /// that never comes means the controller has ended: the call then waits
    /// for the end of its wind-down.
    async fn ask(
        &self,
        call: Call,
        value: Option<Box<dyn DynApp>>,
        default: Option<Duration>,
    ) -> Result<Answer, AppError> {
        if self.controller.sys_record().is_self_call() {
            return Err(AppError::CallingSelf);
        }
        let bound = if self.unbounded {
            None
        } else {
            self.timeout.or(default)
        };
        let exchange = async {
            let (reply, answer) = oneshot::channel();
            if self
                .controller
                .notify(Msg::Call(call, value, reply))
                .await
                .is_err()
            {
                return Err(self.ended().await);
            }
            match answer.await {
                Ok(answer) => Ok(answer),
                Err(_) => Err(self.ended().await),
            }
        };
        let Some(within) = bound else {
            return exchange.await;
        };
        if !Handle::try_current().is_ok_and(|runtime| runtime_has_timers(&runtime)) {
            return Err(AppError::NoTimeDriver);
        }
        tokio::time::timeout_at(saturating_deadline(Instant::now(), within), exchange)
            .await
            .unwrap_or(Err(AppError::Timeout))
    }

    /// The controller's end, read once its state is gone: the state holds the
    /// only verdict sender and drops at the end of the wind-down, or at once
    /// when the controller drops it, so an abort of the controller's task
    /// does not answer a call while its wind-down still stops applications.
    async fn ended(&self) -> AppError {
        let mut verdict = self.verdict.clone();
        while verdict.changed().await.is_ok() {}
        AppError::Ended(self.controller.wait_stopped().await)
    }
}

/// The default bound of [`Applications::which_applications`].
const WHICH_BOUND: Duration = Duration::from_secs(5);

/// What [`Applications::node_shutdown`] reads: nothing yet, or the
/// controller's verdict. The controller's own end is read from its plane once
/// the sender has dropped, so one end gives one reason to every reader.
#[derive(Debug, Clone)]
enum Verdict {
    Pending,
    Shutdown(NodeShutdown),
}

impl Verdict {
    fn answer(self) -> Option<Result<NodeShutdown, AppError>> {
        match self {
            Self::Pending => None,
            Self::Shutdown(verdict) => Some(Ok(verdict)),
        }
    }
}

// - The controller core: one event in, effects out, no I/O ------------------

/// The rows of a drain: each application it stopped, `None` for a lost stop.
type Rows = Vec<(String, Option<AppStop>)>;
type ReqId = u64;
type SeqId = u64;

/// A request to the controller.
#[derive(Debug)]
enum Call {
    Load {
        name: String,
        system: AppSystem,
        deps: Vec<String>,
    },
    Unload(String),
    Start(String, RestartType),
    Ensure(String, RestartType),
    Stop(String),
    StopAll,
    Shutdown,
    Which,
}

impl Call {
    /// The answer of this kind of call that carries `error`.
    fn refuse(&self, error: AppError) -> Answer {
        match self {
            Self::Load { .. } | Self::Unload(_) | Self::Start(..) => Answer::Unit(Err(error)),
            Self::Ensure(..) => Answer::Names(Err(error)),
            Self::Stop(_) => Answer::Stop(Err(error)),
            Self::StopAll | Self::Shutdown => Answer::Stops(Err(error)),
            Self::Which => Answer::Which(Err(error)),
        }
    }
}

/// The answer to one call; each call gets the kind it asked for.
#[derive(Debug)]
enum Answer {
    Unit(Result<(), AppError>),
    Names(Result<Vec<String>, AppError>),
    Stop(Result<AppStop, AppError>),
    Stops(Result<Rows, AppError>),
    Which(Result<Vec<AppInfo>, AppError>),
}

impl Answer {
    fn unit(self) -> Result<(), AppError> {
        match self {
            Self::Unit(result) => result,
            other => other.mismatch(),
        }
    }

    fn stops(self) -> Result<Rows, AppError> {
        match self {
            Self::Stops(result) => result,
            other => other.mismatch(),
        }
    }

    fn mismatch<T>(self) -> T {
        unreachable!("the controller answers each call with its own kind: {self:?}")
    }
}

/// One event the core takes. `free` says whether everything the application
/// owns has released its hold, read by the shell when the sequence landed.
#[derive(Debug)]
enum Event {
    Call(ReqId, Call),
    /// The shell could not begin start sequence `seq`: no time driver, or the
    /// name of the application's own system is taken.
    Refused {
        seq: SeqId,
        error: AppError,
    },
    /// A start sequence ended, a lost one included.
    Started {
        seq: SeqId,
        result: Result<(), AppError>,
        free: bool,
    },
    /// A stop sequence ended; `None` for a lost one.
    Stopped {
        seq: SeqId,
        stop: Option<AppStop>,
        free: bool,
    },
    /// Everything a held application owns has released its hold.
    Released {
        app: String,
    },
    /// The top of run `run` of `app` ended.
    TopEnded {
        app: String,
        run: SeqId,
        reason: StopReason,
    },
    /// The controller is closing: stop everything and forget everything.
    Closing,
}

/// One effect the shell performs, in order.
#[derive(Debug)]
enum Effect {
    Reply(ReqId, Answer),
    /// Keep the value the load request `ReqId` carried, under its name.
    Adopt(ReqId, String),
    RunStart {
        seq: SeqId,
        app: String,
        system: AppSystem,
    },
    RunStop {
        seq: SeqId,
        app: String,
        system: AppSystem,
    },
    /// Watch the top of run `run` of `app`.
    Watch {
        app: String,
        run: SeqId,
    },
    /// Drop the value of `app` and what the shell keeps for it.
    Forget(String),
    Node(NodeShutdown),
    End,
}

#[derive(Debug)]
struct Entry {
    name: String,
    system: AppSystem,
    deps: Vec<String>,
    kind: Option<RestartType>,
    state: AppState,
    /// The start sequence of the current run.
    run: SeqId,
    /// The top ended on its own and the end hooks are still to run.
    end_owed: bool,
}

impl Entry {
    fn system_name(&self) -> &str {
        match &self.system {
            AppSystem::Own => &self.name,
            AppSystem::Shared(name) => name,
        }
    }
}

/// Who a sequence answers when it ends.
#[derive(Debug)]
enum Purpose {
    Reply(ReqId),
    Job(u64),
    Drain,
    EndHooks,
}

#[derive(Debug)]
struct Seq {
    app: String,
    system: String,
    start: bool,
    purpose: Purpose,
}

/// A `start` or an `ensure_all_started`: the names still to start, the ones
/// started, the failure to answer once what it started is stopped, and the
/// names whose rollback stop was lost.
#[derive(Debug)]
struct StartJob {
    id: u64,
    req: ReqId,
    target: String,
    kind: RestartType,
    queue: VecDeque<String>,
    started: Vec<String>,
    names: bool,
    failed: Option<AppError>,
    waiting: bool,
    lost: Vec<String>,
}

impl StartJob {
    fn answer(&mut self, mut result: Result<(), AppError>) -> Answer {
        if let Err(AppError::StartFailed { lost, .. }) = &mut result {
            lost.append(&mut self.lost);
        }
        if self.names {
            Answer::Names(result.map(|()| std::mem::take(&mut self.started)))
        } else {
            Answer::Unit(result)
        }
    }
}

/// A stop of every running application, in reverse start order, once no
/// sequence is in flight.
#[derive(Debug)]
enum Drain {
    StopAll(ReqId),
    Shutdown(Option<ReqId>),
    Node(NodeShutdown),
}

/// What a step decides about the next name of a start job.
enum Next {
    Skip,
    Go,
    Refuse(AppError),
}

#[derive(Debug, Default)]
struct Core {
    /// Every loaded application, in load order.
    apps: Vec<Entry>,
    /// The applications started and not yet stopped, in start order.
    order: Vec<String>,
    seqs: HashMap<SeqId, Seq>,
    /// The sequence in flight in each system.
    busy: HashMap<String, SeqId>,
    jobs: Vec<StartJob>,
    drains: VecDeque<(Drain, Rows)>,
    /// The node verdict is set or closed: it is given at most once per controller.
    node_resolved: bool,
    /// `Closing` was stepped.
    closing: bool,
    ended: bool,
    next: u64,
    out: Vec<Effect>,
}

impl Core {
    fn step(&mut self, event: Event) -> Vec<Effect> {
        match event {
            Event::Call(req, call) => self.call(req, call),
            Event::Refused { seq, error } => self.refused(seq, error),
            Event::Started { seq, result, free } => self.started(seq, result, free),
            Event::Stopped { seq, stop, free } => self.stopped(seq, stop, free),
            Event::Released { app } => {
                if let Some(entry) = self.entry_mut(&app) {
                    if entry.state == AppState::Held {
                        entry.state = AppState::Loaded;
                    }
                }
            }
            Event::TopEnded { app, run, reason } => self.top_ended(&app, run, reason),
            Event::Closing => {
                if !self.closing && !self.ended {
                    self.closing = true;
                    self.node_resolved = true;
                    self.drains.push_back((Drain::Shutdown(None), Vec::new()));
                }
            }
        }
        self.pump();
        std::mem::take(&mut self.out)
    }

    fn bump(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn entry(&self, name: &str) -> Option<&Entry> {
        self.apps.iter().find(|e| e.name == name)
    }

    fn entry_mut(&mut self, name: &str) -> Option<&mut Entry> {
        self.apps.iter_mut().find(|e| e.name == name)
    }

    fn running(&self, name: &str) -> bool {
        self.entry(name)
            .is_some_and(|e| e.state == AppState::Running)
    }

    /// The held applications.
    fn held(&self) -> Vec<String> {
        self.apps
            .iter()
            .filter(|e| e.state == AppState::Held)
            .map(|e| e.name.clone())
            .collect()
    }

    fn forget(&mut self, name: &str) {
        self.apps.retain(|e| e.name != name);
        self.out.push(Effect::Forget(name.to_owned()));
    }

    fn reply(&mut self, req: ReqId, answer: Answer) {
        self.out.push(Effect::Reply(req, answer));
    }

    /// The error a call about `name` gets while a sequence runs in `system`.
    fn in_flight(&self, system: &str, name: &str) -> Option<AppError> {
        let seq = self.seqs.get(self.busy.get(system)?)?;
        Some(if seq.start {
            AppError::StartInFlight(name.to_owned())
        } else {
            AppError::StopInFlight(name.to_owned())
        })
    }

    fn state_error(state: AppState, name: &str) -> AppError {
        let name = name.to_owned();
        match state {
            AppState::Loaded => AppError::NotStarted(name),
            AppState::Running => AppError::AlreadyStarted(name),
            AppState::Starting => AppError::StartInFlight(name),
            AppState::Stopping => AppError::StopInFlight(name),
            AppState::Held => AppError::Held(name),
        }
    }

    fn call(&mut self, req: ReqId, call: Call) {
        if self.ended {
            // No answer: the reply drops with the state, and the caller reads
            // the controller's end reason from its plane.
            return;
        }
        let refused = match &call {
            Call::Load { name, system, deps } => match self.entry(name) {
                Some(_) => Some(AppError::AlreadyLoaded(name.clone())),
                None => {
                    self.apps.push(Entry {
                        name: name.clone(),
                        system: system.clone(),
                        deps: deps.clone(),
                        kind: None,
                        state: AppState::Loaded,
                        run: 0,
                        end_owed: false,
                    });
                    self.out.push(Effect::Adopt(req, name.clone()));
                    return self.reply(req, Answer::Unit(Ok(())));
                }
            },
            Call::Unload(name) => match self.entry(name).map(|e| e.state) {
                None => Some(AppError::NotLoaded(name.clone())),
                Some(AppState::Loaded) => {
                    self.forget(name);
                    return self.reply(req, Answer::Unit(Ok(())));
                }
                Some(state) => Some(Self::state_error(state, name)),
            },
            Call::Start(name, kind) => match self.start_refusal(name) {
                Some(error) => Some(error),
                None => {
                    let queue = VecDeque::from([name.clone()]);
                    return self.add_job(req, name, *kind, queue, false);
                }
            },
            Call::Ensure(name, kind) => match self.ensure_order(name) {
                Err(error) => Some(error),
                Ok(_) if self.running(name) => {
                    return self.reply(req, Answer::Names(Ok(Vec::new())));
                }
                Ok(queue) => return self.add_job(req, name, *kind, queue, true),
            },
            Call::Stop(name) => match self.entry(name) {
                None => Some(AppError::NotLoaded(name.clone())),
                Some(e) if e.state != AppState::Running => Some(Self::state_error(e.state, name)),
                Some(e) => match self.in_flight(e.system_name(), name) {
                    Some(error) => Some(error),
                    None => {
                        self.issue(name, false, Purpose::Reply(req));
                        return;
                    }
                },
            },
            Call::StopAll => {
                self.drains.push_back((Drain::StopAll(req), Vec::new()));
                return;
            }
            Call::Shutdown => {
                // The controller begins to close: nothing sets the node
                // verdict from here on.
                self.node_resolved = true;
                self.drains
                    .push_back((Drain::Shutdown(Some(req)), Vec::new()));
                return;
            }
            Call::Which => {
                let rows = self
                    .apps
                    .iter()
                    .map(|e| AppInfo {
                        name: e.name.clone(),
                        system: e.system_name().to_owned(),
                        kind: e.kind,
                        state: e.state,
                    })
                    .collect();
                return self.reply(req, Answer::Which(Ok(rows)));
            }
        };
        if let Some(error) = refused {
            let answer = call.refuse(error);
            self.reply(req, answer);
        }
    }

    fn start_refusal(&self, name: &str) -> Option<AppError> {
        let Some(entry) = self.entry(name) else {
            return Some(AppError::NotLoaded(name.to_owned()));
        };
        if entry.state != AppState::Loaded {
            return Some(Self::state_error(entry.state, name));
        }
        if !self.drains.is_empty() {
            return Some(AppError::StopInFlight(name.to_owned()));
        }
        if let Some(error) = self.in_flight(entry.system_name(), name) {
            return Some(error);
        }
        entry
            .deps
            .iter()
            .find(|dep| !self.running(dep))
            .map(|dep| AppError::DependencyNotRunning {
                app: name.to_owned(),
                dependency: dep.clone(),
            })
    }

    /// `name` and its dependencies, each after the ones it needs.
    fn ensure_order(&self, name: &str) -> Result<VecDeque<String>, AppError> {
        if !self.drains.is_empty() && !self.running(name) {
            return Err(AppError::StopInFlight(name.to_owned()));
        }
        let mut queue = VecDeque::new();
        let mut seen = HashSet::new();
        let mut stack = vec![(name.to_owned(), false)];
        while let Some((next, expanded)) = stack.pop() {
            if expanded {
                queue.push_back(next);
                continue;
            }
            if !seen.insert(next.clone()) {
                continue;
            }
            let entry = self
                .entry(&next)
                .ok_or_else(|| AppError::NotLoaded(next.clone()))?;
            stack.push((next.clone(), true));
            stack.extend(entry.deps.iter().rev().map(|dep| (dep.clone(), false)));
        }
        Ok(queue)
    }

    fn add_job(
        &mut self,
        req: ReqId,
        target: &str,
        kind: RestartType,
        queue: VecDeque<String>,
        names: bool,
    ) {
        let id = self.bump();
        self.jobs.push(StartJob {
            id,
            req,
            target: target.to_owned(),
            kind,
            queue,
            started: Vec::new(),
            names,
            failed: None,
            waiting: false,
            lost: Vec::new(),
        });
    }

    fn issue(&mut self, name: &str, start: bool, purpose: Purpose) {
        let seq = self.bump();
        let Some(entry) = self.entry_mut(name) else {
            return;
        };
        entry.state = if start {
            AppState::Starting
        } else {
            AppState::Stopping
        };
        let system = entry.system_name().to_owned();
        let app_system = entry.system.clone();
        self.busy.insert(system.clone(), seq);
        self.seqs.insert(
            seq,
            Seq {
                app: name.to_owned(),
                system,
                start,
                purpose,
            },
        );
        let app = name.to_owned();
        self.out.push(if start {
            Effect::RunStart {
                seq,
                app,
                system: app_system,
            }
        } else {
            Effect::RunStop {
                seq,
                app,
                system: app_system,
            }
        });
    }

    fn end_seq(&mut self, seq: SeqId) -> Option<Seq> {
        let ended = self.seqs.remove(&seq)?;
        if self.busy.get(&ended.system) == Some(&seq) {
            self.busy.remove(&ended.system);
        }
        Some(ended)
    }

    fn job_mut(&mut self, purpose: &Purpose) -> Option<&mut StartJob> {
        match purpose {
            Purpose::Job(id) => self.jobs.iter_mut().find(|job| job.id == *id),
            _ => None,
        }
    }

    fn resolve_node(&mut self, application: String, cause: NodeCause) {
        if !self.node_resolved {
            self.node_resolved = true;
            let verdict = NodeShutdown {
                application,
                cause,
                stopped: Vec::new(),
            };
            self.drains.push_back((Drain::Node(verdict), Vec::new()));
        }
    }

    /// Moves `app` to `Loaded` when it is free, else to `Held`.
    fn settle_entry(&mut self, app: &str, free: bool) {
        if let Some(entry) = self.entry_mut(app) {
            entry.end_owed = false;
            entry.state = if free {
                AppState::Loaded
            } else {
                AppState::Held
            };
        }
    }

    /// The shell could not begin the start: no node effect.
    fn refused(&mut self, seq: SeqId, error: AppError) {
        let Some(ended) = self.end_seq(seq) else {
            return;
        };
        self.settle_entry(&ended.app, true);
        if let Some(job) = self.job_mut(&ended.purpose) {
            job.failed = Some(error);
            job.waiting = false;
        }
    }

    fn started(&mut self, seq: SeqId, result: Result<(), AppError>, free: bool) {
        let Some(ended) = self.end_seq(seq) else {
            return;
        };
        let app = ended.app;
        match result {
            Ok(()) => {
                if let Some(entry) = self.entry_mut(&app) {
                    entry.state = AppState::Running;
                    entry.run = seq;
                }
                self.order.push(app.clone());
                self.out.push(Effect::Watch {
                    app: app.clone(),
                    run: seq,
                });
                if let Some(job) = self.job_mut(&ended.purpose) {
                    job.started.push(app);
                    job.waiting = false;
                }
            }
            Err(error) => {
                let kind = self.entry(&app).and_then(|e| e.kind);
                self.settle_entry(&app, free);
                if matches!(kind, Some(RestartType::Permanent | RestartType::Transient)) {
                    let text = match &error {
                        AppError::StartFailed { error, .. } => error.to_string(),
                        other => other.to_string(),
                    };
                    self.resolve_node(app, NodeCause::StartFailed(text));
                }
                if let Some(job) = self.job_mut(&ended.purpose) {
                    job.failed = Some(error);
                    job.waiting = false;
                }
            }
        }
    }

    fn stopped(&mut self, seq: SeqId, stop: Option<AppStop>, free: bool) {
        let Some(ended) = self.end_seq(seq) else {
            return;
        };
        let app = ended.app;
        self.order.retain(|name| *name != app);
        self.settle_entry(&app, free);
        let stop = stop.map(|mut stop| {
            stop.held = !free;
            stop
        });
        match ended.purpose {
            Purpose::Reply(req) => {
                let answer = stop.ok_or_else(|| AppError::StopLost(app));
                self.reply(req, Answer::Stop(answer));
            }
            Purpose::Job(_) => {
                if let Some(job) = self.job_mut(&ended.purpose) {
                    if stop.is_none() {
                        job.lost.push(app);
                    }
                    job.waiting = false;
                }
            }
            Purpose::Drain => {
                if let Some((_, rows)) = self.drains.front_mut() {
                    rows.push((app, stop));
                }
            }
            Purpose::EndHooks => {}
        }
    }

    fn top_ended(&mut self, app: &str, run: SeqId, reason: StopReason) {
        let Some(entry) = self.entry_mut(app) else {
            return;
        };
        if entry.state != AppState::Running || entry.run != run {
            return;
        }
        entry.state = AppState::Stopping;
        entry.end_owed = true;
        let ends_node = match entry.kind {
            Some(RestartType::Permanent) => true,
            Some(RestartType::Transient) => !matches!(reason, StopReason::Graceful),
            _ => false,
        };
        if ends_node {
            self.resolve_node(app.to_owned(), NodeCause::Ended(reason));
        }
    }

    fn pump(&mut self) {
        loop {
            let owed = self
                .apps
                .iter()
                .find(|e| e.end_owed && !self.busy.contains_key(e.system_name()))
                .map(|e| e.name.clone());
            if let Some(name) = owed {
                if let Some(entry) = self.entry_mut(&name) {
                    entry.end_owed = false;
                }
                self.issue(&name, false, Purpose::EndHooks);
                continue;
            }
            if self.pump_jobs() || self.pump_drain() {
                continue;
            }
            break;
        }
    }

    /// Moves the first start job that can move; `true` when one did.
    fn pump_jobs(&mut self) -> bool {
        for i in 0..self.jobs.len() {
            if self.jobs[i].waiting {
                continue;
            }
            if self.jobs[i].failed.is_some() {
                let back = self.jobs[i]
                    .started
                    .iter()
                    .rposition(|name| self.running(name));
                if let (Some(pos), true) = (back, self.drains.is_empty()) {
                    let name = self.jobs[i].started[pos].clone();
                    let free = self
                        .entry(&name)
                        .is_some_and(|e| !self.busy.contains_key(e.system_name()));
                    if !free {
                        continue;
                    }
                    self.jobs[i].started.truncate(pos);
                    self.jobs[i].waiting = true;
                    let id = self.jobs[i].id;
                    self.issue(&name, false, Purpose::Job(id));
                    return true;
                }
                let mut job = self.jobs.remove(i);
                let error = job
                    .failed
                    .take()
                    .unwrap_or_else(|| AppError::StopInFlight(job.target.clone()));
                job.started.clear();
                let answer = job.answer(Err(error));
                self.reply(job.req, answer);
                return true;
            }
            let Some(name) = self.jobs[i].queue.front().cloned() else {
                let mut job = self.jobs.remove(i);
                let answer = job.answer(Ok(()));
                self.reply(job.req, answer);
                return true;
            };
            if !self.drains.is_empty() {
                let mut job = self.jobs.remove(i);
                let answer = job.answer(Err(AppError::StopInFlight(job.target.clone())));
                self.reply(job.req, answer);
                return true;
            }
            match self.next_start(&name) {
                Next::Skip => {
                    self.jobs[i].queue.pop_front();
                }
                Next::Refuse(error) => self.jobs[i].failed = Some(error),
                Next::Go => {
                    let job = &mut self.jobs[i];
                    job.queue.pop_front();
                    job.waiting = true;
                    let (id, kind) = (job.id, job.kind);
                    if let Some(entry) = self.entry_mut(&name) {
                        entry.kind = Some(kind);
                    }
                    self.issue(&name, true, Purpose::Job(id));
                }
            }
            return true;
        }
        false
    }

    fn next_start(&self, name: &str) -> Next {
        let Some(entry) = self.entry(name) else {
            return Next::Refuse(AppError::NotLoaded(name.to_owned()));
        };
        match entry.state {
            AppState::Running => Next::Skip,
            AppState::Loaded => match self.start_refusal(name) {
                Some(error) => Next::Refuse(error),
                None => Next::Go,
            },
            state => Next::Refuse(Self::state_error(state, name)),
        }
    }

    /// Moves the first drain once nothing else is in flight; `true` when it
    /// did.
    fn pump_drain(&mut self) -> bool {
        if self.drains.is_empty()
            || !self.seqs.is_empty()
            || !self.jobs.is_empty()
            || self.apps.iter().any(|e| e.end_owed)
        {
            return false;
        }
        let last = self
            .order
            .iter()
            .rev()
            .find(|name| self.running(name))
            .cloned();
        if let Some(name) = last {
            self.issue(&name, false, Purpose::Drain);
            return true;
        }
        let Some((drain, rows)) = self.drains.pop_front() else {
            return false;
        };
        match drain {
            Drain::StopAll(req) => self.reply(req, Answer::Stops(Ok(rows))),
            Drain::Shutdown(req) => {
                if let Some(req) = req {
                    self.reply(req, Answer::Stops(Ok(rows)));
                }
                if !self.ended {
                    for entry in std::mem::take(&mut self.apps) {
                        self.out.push(Effect::Forget(entry.name));
                    }
                    self.order.clear();
                    self.ended = true;
                    self.out.push(Effect::End);
                }
            }
            Drain::Node(mut verdict) => {
                verdict.stopped = rows;
                self.out.push(Effect::Node(verdict));
            }
        }
        true
    }
}

// - The shell: the controller actor and the sequences it runs ---------------

type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An [`Application`] behind a pointer the controller can hold.
trait DynApp: Send + 'static {
    fn start_boxed<'a>(
        &'a mut self,
        system: &'a Arc<ActorSystem>,
    ) -> Boxed<'a, Result<AppRoot, StartError>>;
    fn prep_stop_boxed(&mut self) -> Boxed<'_, ()>;
    fn stop_boxed(&mut self) -> Boxed<'_, ()>;
}

impl<A: Application> DynApp for A {
    fn start_boxed<'a>(
        &'a mut self,
        system: &'a Arc<ActorSystem>,
    ) -> Boxed<'a, Result<AppRoot, StartError>> {
        Box::pin(Application::start(self, system))
    }

    fn prep_stop_boxed(&mut self) -> Boxed<'_, ()> {
        Box::pin(Application::prep_stop(self))
    }

    fn stop_boxed(&mut self) -> Boxed<'_, ()> {
        Box::pin(Application::stop(self))
    }
}

/// The report of a top not known to have ended.
fn unanswered() -> StopReport {
    StopReport {
        outcome: StopOutcome::Unresponsive,
        reason: None,
    }
}

fn start_failed(app: &str, text: &str) -> AppError {
    AppError::StartFailed {
        app: app.to_owned(),
        error: text.into(),
        lost: Vec::new(),
    }
}

/// Whether the runtime `rt` is shutting down or has ended.
fn runtime_ending(rt: &Handle) -> bool {
    crate::time_probe::runtime_is_shut_down(rt)
}

/// Whether every actor of an application's own system has released its
/// hold: the system is `Defunct`, holds no actor and counts no live hold.
fn own_free(system: &ActorSystem) -> bool {
    system.is_defunct() && system.holds_no_actor() && system.live_holds() == 0
}

/// Whether the top whose plane is `plane` and its whole subtree have released
/// their holds, read with no wait; a plane whose task never ran has none.
fn top_free(plane: &StatusRx) -> bool {
    plane.subtree_watch().is_none_or(|watch| watch.is_gone())
}

/// The top of a run as its start accepted it.
#[derive(Clone)]
struct Top {
    id: ActorId,
    plane: StatusRx,
    system: Arc<ActorSystem>,
    /// The abort handle, kill grace and runtime the registry held for the top
    /// at accept; `None` when it had left the registry.
    task: Option<(Option<AbortHandle>, Duration, Handle)>,
}

impl Top {
    fn grace(&self) -> Duration {
        self.task.as_ref().map_or(DEFAULT_KILL_GRACE, |task| task.1)
    }

    /// Kills the top with no wait: `Kill` through the registry's declaring
    /// raise, or, for a top that has left the registry, the backstop.
    fn kill(&self) {
        if self.system.kill_root(&self.id, &self.plane)
            || self.system.root_task(&self.id, &self.plane).is_some()
        {
            return;
        }
        self.backstop();
    }

    /// Arms the reaper's abort, after the kill grace, of a top that has left
    /// the registry and whose plane is not terminal, with the handle read at
    /// accept. The reaper of the top's own runtime owns the abort.
    fn backstop(&self) {
        if read_status(&self.plane) == ActorStatus::Stopped {
            return;
        }
        if let Some((Some(abort), grace, rt)) = &self.task {
            self.system
                .arm_kill_backstop(abort.clone(), self.plane.record(), *grace, rt);
        }
    }

    /// Waits for a top that has left the registry: its kill grace plus the
    /// abort's settle time, after arming the backstop.
    async fn wait_ending(&self) -> StopReport {
        self.backstop();
        let mut plane = self.plane.clone();
        if wait_terminal(&mut plane, self.grace().saturating_add(ABORT_SETTLE)).await {
            return terminal_report(&plane, StopOutcome::Graceful);
        }
        unanswered()
    }
}

/// The one owner of a run's end until a stop, or a failed start, disarms it.
/// Dropped while armed it fires, with no await: the top is killed and, for a
/// system of the application's own, the system's shutdown is entered, unless
/// the controller's runtime is shutting down.
struct AppHold {
    top: Option<Top>,
    own: Option<(Arc<ActorSystem>, Owner)>,
    armed: bool,
}

impl AppHold {
    fn new(own: Option<(Arc<ActorSystem>, Owner)>) -> Self {
        Self {
            top: None,
            own,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AppHold {
    fn drop(&mut self) {
        if !std::mem::take(&mut self.armed) {
            return;
        }
        if let Some(top) = &self.top {
            top.kill();
        }
        if let Some((system, owner)) = self.own.take() {
            enter_shutdown(system, &owner);
        }
    }
}

/// Enters `system`'s shutdown in place, polling it once (registrations are
/// refused and the walk is spawned as a helper of the system), then hands the
/// rest of the call to a helper of the controller. Nothing is entered while
/// the controller's runtime is shutting down.
fn enter_shutdown(system: Arc<ActorSystem>, owner: &Owner) {
    if runtime_ending(&owner.rt) {
        return;
    }
    let walker = system.clone();
    let mut walk: Boxed<'static, ShutdownReport> = Box::pin(async move { walker.shutdown().await });
    let mut cx = Context::from_waker(Waker::noop());
    if walk.as_mut().poll(&mut cx).is_ready() {
        return;
    }
    app_helper(&system, owner, async move {
        drop(walk.await);
    });
}

/// How a sequence ended.
enum Landed {
    Started(Result<(), AppError>),
    Stopped(AppStop),
    /// Its guard was dropped before the sequence finished; its hold fired.
    Lost,
}

/// What a sequence hands back: the application's value, how it ended, the
/// hold of a start that succeeded, and the plane of its top, read for the
/// release.
struct Done {
    value: Box<dyn DynApp>,
    landed: Landed,
    hold: Option<AppHold>,
    watch: Option<StatusRx>,
}

/// The one owner of a sequence's value and hold. The sequence ends through
/// [`SeqGuard::finish`]; dropped before that (a panic of the crate's own
/// code, caught by the helper, or the end of its runtime), it fires its hold
/// first and then hands the value back as lost. The landing goes out before
/// the signal the controller watches is dropped, so the controller always
/// finds it.
struct SeqGuard {
    value: Option<Box<dyn DynApp>>,
    hold: Option<AppHold>,
    done_tx: Option<oneshot::Sender<Done>>,
    signal: Option<StatusTx>,
}

impl SeqGuard {
    fn value(&mut self) -> &mut dyn DynApp {
        self.value
            .as_deref_mut()
            .expect("the value is held until the sequence ends")
    }

    fn hold(&mut self) -> &mut AppHold {
        self.hold
            .as_mut()
            .expect("the hold is held until the sequence ends")
    }

    /// Disarms the hold after the sequence's last step and lets it go.
    fn release(&mut self) {
        if let Some(mut hold) = self.hold.take() {
            hold.disarm();
        }
    }

    /// Ends the sequence with `landed`; a hold still held goes with it.
    fn finish(mut self, landed: Landed) {
        if let Some(value) = self.value.take() {
            let watch = self.watch();
            let hold = self.hold.take();
            self.hand(Done {
                value,
                landed,
                hold,
                watch,
            });
        }
    }

    fn watch(&self) -> Option<StatusRx> {
        let top = self.hold.as_ref()?.top.as_ref()?;
        Some(top.plane.clone())
    }

    /// Sends `done`; when nobody receives it, it drops here, and an armed
    /// hold in it fires.
    fn hand(&mut self, done: Done) {
        if let Some(tx) = self.done_tx.take() {
            if let Err(done) = send_contained(tx, done) {
                let _ = end_value(done);
            }
        }
        drop(self.signal.take());
    }
}

impl Drop for SeqGuard {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            let watch = self.watch();
            let _ = end_value(self.hold.take());
            self.hand(Done {
                value,
                landed: Landed::Lost,
                hold: None,
                watch,
            });
        }
        drop(self.signal.take());
    }
}

/// The handle given to [`AppRoot::of`] as the top of a start in `system`:
/// spawned in that system, as a root, and, checked last, taken by no start
/// before this one, which takes it.
fn accept(root: &AppRoot, system: &Arc<ActorSystem>) -> Option<Top> {
    let record = root.plane.record();
    let mode = Arc::downgrade(system.telemetry_mode());
    if !record.spawned_in(&mode) || !record.is_root() || !record.take_as_top() {
        return None;
    }
    Some(Top {
        id: root.id.clone(),
        plane: root.plane.clone(),
        system: system.clone(),
        task: system.root_task(&root.id, &root.plane),
    })
}

/// Waits, within the abort's settle time, until every actor of an
/// application's own system has released its hold; `true` when it has.
async fn own_release(system: &ActorSystem) -> bool {
    let _ = tokio::time::timeout(ABORT_SETTLE, system.holds_released()).await;
    own_free(system)
}

/// Waits for the subtree of a top that reached a terminal report: its plane
/// closed and its own hold released within the abort's settle time, then the
/// subtree's end. `true` when the subtree ended.
async fn subtree_ended(plane: &StatusRx) -> bool {
    let mut closed = plane.clone();
    let settled = tokio::time::timeout(ABORT_SETTLE, async {
        wait_closed(&mut closed).await;
        if let Some(watch) = plane.subtree_watch() {
            watch.released().await;
        }
    })
    .await
    .is_ok();
    settled && SubtreeObserver::new(plane.clone()).wait_ended().await == SubtreeEnd::Ended
}

/// The top's stop: as the walk stops a root, or, for a top the registry no
/// longer holds, a wait of its kill grace; then its subtree. The flag is set
/// when something did not end within the bounds.
async fn stop_top(top: &Top) -> (StopReport, bool) {
    let report = match top.system.stop_one_root(&top.id, &top.plane).await {
        Some(report) => report,
        None => top.wait_ending().await,
    };
    if report.outcome == StopOutcome::Unresponsive {
        return (report, true);
    }
    let ended = subtree_ended(&top.plane).await;
    (report, !ended)
}

/// `start` and the accept; for a start that failed in a system of the
/// application's own, that system's walk and the release wait.
async fn run_start(mut g: SeqGuard, app: String, system: Arc<ActorSystem>, own: bool) {
    let value = g.value();
    let host = &system;
    let failure = match catch_hook_t((), move || value.start_boxed(host)).await {
        Ok(Ok(root)) => match accept(&root, &system) {
            Some(top) => {
                g.hold().top = Some(top);
                return g.finish(Landed::Started(Ok(())));
            }
            None => start_failed(
                &app,
                "the handle given to AppRoot::of is no untaken root of the application's system",
            ),
        },
        Ok(Err(error)) => AppError::StartFailed {
            app: app.clone(),
            error,
            lost: Vec::new(),
        },
        Err(panic) => {
            let text = format!("start panicked: {}", payload_into_string(panic));
            start_failed(&app, &text)
        }
    };
    if own {
        drop(system.shutdown().await);
        let _ = own_release(&system).await;
    }
    g.release();
    g.finish(Landed::Started(Err(failure)));
}

/// `prep_stop`, the top's stop and its subtree, `stop`, and, for a system of
/// the application's own, that system's walk and the release wait.
async fn run_stop(mut g: SeqGuard, own: Option<Arc<ActorSystem>>) {
    let value = g.value();
    if let Err(panic) = catch_hook_t((), move || value.prep_stop_boxed()).await {
        discard(panic);
    }
    let top = g.hold().top.clone();
    #[cfg(test)]
    points::hit(points::Point::AtTop);
    let (report, mut held) = match &top {
        Some(top) => stop_top(top).await,
        None => (unanswered(), false),
    };
    let value = g.value();
    if let Err(panic) = catch_hook_t((), move || value.stop_boxed()).await {
        discard(panic);
    }
    let system = match &own {
        Some(system) => {
            let report = system.shutdown().await;
            held |= !own_release(system).await;
            Some(report)
        }
        None => None,
    };
    g.release();
    g.finish(Landed::Stopped(AppStop {
        top: report,
        system,
        held,
    }));
}

/// What a sequence runs.
enum Course {
    Start { system: Arc<ActorSystem>, own: bool },
    Stop { own: Option<Arc<ActorSystem>> },
}

/// A sequence ready to run.
struct Ready {
    app: String,
    value: Box<dyn DynApp>,
    hold: AppHold,
    course: Course,
}

/// A sequence in flight on a helper.
struct Flight {
    app: String,
    start: bool,
    rx: oneshot::Receiver<Done>,
}

/// The controller the helpers work for, and the runtime they run on.
#[derive(Clone)]
struct Owner {
    rt: Handle,
    id: ActorId,
    record: IncarnationRef,
}

impl Owner {
    fn of(ctx: &ActorContext<Controller>) -> Self {
        Self {
            rt: ctx.runtime_handle().clone(),
            id: ctx.actor_id().clone(),
            record: ctx.stop_lane().abort_record().clone(),
        }
    }
}

/// Spawns `work` on a helper of `system` that works for the controller: its
/// end is recorded by the system's helper slot and counted among the
/// controller's events, through the controller's record.
fn app_helper(
    system: &ActorSystem,
    owner: &Owner,
    work: impl Future<Output = ()> + Send + 'static,
) {
    let helper_owner = HelperOwner::new(owner.id.clone(), Some(owner.record.clone()));
    drop(system.spawn_helper(
        &owner.rt,
        HelperKind::Application,
        Some(helper_owner),
        move |end| end.observe(work),
    ));
}

enum Msg {
    Call(Call, Option<Box<dyn DynApp>>, oneshot::Sender<Answer>),
    /// The helper of sequence `SeqId` ended.
    Landed(SeqId),
    TopDown {
        app: String,
        run: SeqId,
        reason: StopReason,
    },
}

/// The controller's state: the core and what the shell keeps for it. The
/// controller actor owns it while it runs; its wind-down owns it after.
struct Shell {
    core: Core,
    values: HashMap<String, Box<dyn DynApp>>,
    loads: HashMap<ReqId, Box<dyn DynApp>>,
    replies: HashMap<ReqId, oneshot::Sender<Answer>>,
    /// Each system of an application's own, held across its stops.
    owned: HashMap<String, Arc<ActorSystem>>,
    /// The hold of each running application.
    holds: HashMap<String, AppHold>,
    /// The plane of each application's last top, read for the release.
    kept: HashMap<String, StatusRx>,
    flights: HashMap<SeqId, Flight>,
    verdict: watch::Sender<Verdict>,
    next_req: ReqId,
}

impl Shell {
    fn new(verdict: watch::Sender<Verdict>) -> Self {
        Self {
            core: Core::default(),
            values: HashMap::new(),
            loads: HashMap::new(),
            replies: HashMap::new(),
            owned: HashMap::new(),
            holds: HashMap::new(),
            kept: HashMap::new(),
            flights: HashMap::new(),
            verdict,
            next_req: 0,
        }
    }

    /// Whether everything `app` owns has released its hold: every
    /// actor of its own system, or its last top's whole subtree.
    fn free(&self, app: &str) -> bool {
        match self.owned.get(app) {
            Some(system) => own_free(system),
            None => self.kept.get(app).is_none_or(top_free),
        }
    }

    /// S0: the system a start of `app` runs in and its hold. A system of its
    /// own is created at the first start and reactivated at the next.
    fn begin(
        &mut self,
        seq: SeqId,
        app: &str,
        system: &AppSystem,
        owner: &Owner,
    ) -> Result<Ready, Event> {
        let host = match system {
            AppSystem::Own => match self.owned.get(app) {
                Some(held) => {
                    held.reactivate();
                    held.clone()
                }
                None => match ActorSystem::create_owned(app) {
                    Ok(created) => {
                        self.owned.insert(app.to_owned(), created.clone());
                        created
                    }
                    Err(_) => {
                        let error = AppError::SystemTaken(app.to_owned());
                        return Err(Event::Refused { seq, error });
                    }
                },
            },
            AppSystem::Shared(name) if self.owned.contains_key(name.as_str()) => {
                // The system of an application of its own, named as that
                // application, is never shared: its stop shuts it down.
                let error = AppError::SystemTaken(app.to_owned());
                return Err(Event::Refused { seq, error });
            }
            AppSystem::Shared(name) => match ActorSystem::get_named(name)
                .map_or_else(|| ActorSystem::create(name), Ok)
                .or_else(|error| ActorSystem::get_named(name).ok_or(error))
            {
                Ok(host) => host,
                Err(error) => {
                    let error = start_failed(app, &error.to_string());
                    let free = self.free(app);
                    let result = Err(error);
                    return Err(Event::Started { seq, result, free });
                }
            },
        };
        if !host.is_active() {
            let result = Err(start_failed(
                app,
                "the application's system is shutting down",
            ));
            let free = self.free(app);
            return Err(Event::Started { seq, result, free });
        }
        let Some(value) = self.values.remove(app) else {
            let error = start_failed(app, "the application's value is gone");
            return Err(Event::Refused { seq, error });
        };
        let own = *system == AppSystem::Own;
        Ok(Ready {
            app: app.to_owned(),
            value,
            hold: AppHold::new(own.then(|| (host.clone(), owner.clone()))),
            course: Course::Start { system: host, own },
        })
    }

    /// Takes what a sequence needs out of the shell, or the event of one
    /// that cannot begin. A start on a runtime without the time driver is
    /// refused, since the steps of its start and of its stop wait under
    /// timers.
    fn prepare(&mut self, effect: Effect, owner: &Owner) -> Result<Ready, Event> {
        match effect {
            Effect::RunStart { seq, app, system } => {
                if !Handle::try_current().is_ok_and(|runtime| runtime_has_timers(&runtime)) {
                    let error = AppError::NoTimeDriver;
                    return Err(Event::Refused { seq, error });
                }
                self.begin(seq, &app, &system, owner)
            }
            Effect::RunStop { seq, app, system } => {
                let hold = self
                    .holds
                    .remove(&app)
                    .unwrap_or_else(|| AppHold::new(None));
                let Some(value) = self.values.remove(&app) else {
                    drop(hold);
                    let free = self.free(&app);
                    return Err(Event::Stopped {
                        seq,
                        stop: None,
                        free,
                    });
                };
                let own = match system {
                    AppSystem::Own => self.owned.get(&app).cloned(),
                    AppSystem::Shared(_) => None,
                };
                Ok(Ready {
                    app,
                    value,
                    hold,
                    course: Course::Stop { own },
                })
            }
            other => unreachable!("only sequences are prepared: {other:?}"),
        }
    }

    /// The event of a sequence that ended, `done` being what it handed back.
    fn landed(&mut self, seq: SeqId, flight: Flight, done: Option<Done>) -> Event {
        let Flight { app, start, .. } = flight;
        let landed = match done {
            Some(Done {
                value,
                landed,
                hold,
                watch,
            }) => {
                self.values.insert(app.clone(), value);
                if let Some(watch) = watch {
                    self.kept.insert(app.clone(), watch);
                }
                if let Some(hold) = hold {
                    self.holds.insert(app.clone(), hold);
                }
                landed
            }
            None => Landed::Lost,
        };
        let free = self.free(&app);
        match landed {
            Landed::Started(result) => Event::Started { seq, result, free },
            Landed::Stopped(stop) => Event::Stopped {
                seq,
                stop: Some(stop),
                free,
            },
            Landed::Lost if start => Event::Started {
                seq,
                result: Err(start_failed(&app, "the start sequence was lost")),
                free,
            },
            Landed::Lost => Event::Stopped {
                seq,
                stop: None,
                free,
            },
        }
    }

    /// Performs an effect that needs no task of its own.
    fn settle(&mut self, effect: Effect) {
        match effect {
            Effect::Reply(req, answer) => {
                if let Some(value) = self.loads.remove(&req) {
                    let _ = end_value(value);
                }
                if let Some(reply) = self.replies.remove(&req) {
                    if let Err(answer) = send_contained(reply, answer) {
                        let _ = end_value(answer);
                    }
                }
            }
            Effect::Adopt(req, name) => {
                if let Some(value) = self.loads.remove(&req) {
                    if let Some(old) = self.values.insert(name, value) {
                        let _ = end_value(old);
                    }
                }
            }
            Effect::Forget(name) => {
                if let Some(value) = self.values.remove(&name) {
                    let _ = end_value(value);
                }
                let _ = end_value(self.holds.remove(&name));
                self.owned.remove(&name);
                self.kept.remove(&name);
            }
            Effect::Node(verdict) => {
                self.verdict.send_replace(Verdict::Shutdown(verdict));
            }
            Effect::RunStart { .. }
            | Effect::RunStop { .. }
            | Effect::Watch { .. }
            | Effect::End => {}
        }
    }

    /// Steps `event`, after stepping `Released` for each held application
    /// that is free.
    fn step(&mut self, event: Event) -> Vec<Effect> {
        let mut out = Vec::new();
        for app in self.core.held() {
            if self.free(&app) {
                out.extend(self.core.step(Event::Released { app }));
            }
        }
        out.extend(self.core.step(event));
        out
    }

    /// Runs each step of the core from `event` on and performs its effects,
    /// each sequence on a helper that works for the controller. With `ctx`
    /// the controller watches each helper's end and each top; without it (the
    /// wind-down) the landings are read from their channels.
    fn apply(
        &mut self,
        event: Event,
        mut ctx: Option<&mut ActorContext<Controller>>,
        owner: &Owner,
    ) {
        let mut events = VecDeque::from([event]);
        while let Some(event) = events.pop_front() {
            for effect in self.step(event) {
                match effect {
                    Effect::RunStart { seq, .. } | Effect::RunStop { seq, .. } => {
                        match self.prepare(effect, owner) {
                            Ok(ready) => {
                                #[cfg(test)]
                                points::hit(points::Point::BeforeLaunch);
                                self.launch(seq, ready, ctx.as_deref_mut(), owner);
                            }
                            Err(event) => events.push_back(event),
                        }
                    }
                    Effect::Watch { app, run } => {
                        if let Some(ctx) = ctx.as_deref_mut() {
                            self.watch(app, run, ctx);
                        }
                    }
                    Effect::End => {
                        if let Some(ctx) = ctx.as_deref_mut() {
                            let _ = ctx.stop(StopReason::Graceful);
                        }
                    }
                    other => self.settle(other),
                }
            }
        }
    }

    /// Spawns a sequence on a helper of its system. The sequence's guard
    /// hands its result over a one-shot channel and then drops a signal plane
    /// the controller monitors, so every end of the helper, a panic or the
    /// end of its runtime included, reaches the controller as one message.
    fn launch(
        &mut self,
        seq: SeqId,
        ready: Ready,
        ctx: Option<&mut ActorContext<Controller>>,
        owner: &Owner,
    ) {
        let Ready {
            app,
            value,
            hold,
            course,
        } = ready;
        let host = match &course {
            Course::Start { system, .. } => system.clone(),
            Course::Stop { own } => own
                .clone()
                .or_else(|| hold.top.as_ref().map(|top| top.system.clone()))
                .unwrap_or_else(ActorSystem::default),
        };
        let start = matches!(course, Course::Start { .. });
        let (done_tx, rx) = oneshot::channel();
        let signal = ctx.map(|ctx| {
            let (signal, ended) = signal_plane(host.telemetry_mode().clone());
            let target = PlaneTarget {
                actor_id: ctx.actor_id().clone(),
                incarnation: None,
                plane: ended,
            };
            let _ = ctx.monitor(&target, move |_| Msg::Landed(seq));
            signal
        });
        let guard = SeqGuard {
            value: Some(value),
            hold: Some(hold),
            done_tx: Some(done_tx),
            signal,
        };
        let name = app.clone();
        app_helper(&host, owner, async move {
            match course {
                Course::Start { system, own } => run_start(guard, name, system, own).await,
                Course::Stop { own } => run_stop(guard, own).await,
            }
        });
        self.flights.insert(seq, Flight { app, start, rx });
    }

    /// Monitors the top of run `run` of `app`. A top that has ended already
    /// gives its own reason.
    fn watch(&mut self, app: String, run: SeqId, ctx: &mut ActorContext<Controller>) {
        let Some(top) = self.holds.get(&app).and_then(|hold| hold.top.as_ref()) else {
            return;
        };
        let target = PlaneTarget {
            actor_id: top.id.clone(),
            incarnation: Some(top.plane.record().incarnation()),
            plane: top.plane.clone(),
        };
        let _ = ctx.monitor_spawned(&target, move |down| Msg::TopDown {
            app: app.clone(),
            run,
            reason: match down.reason {
                DownReason::Stopped(reason) => reason,
                DownReason::NoProc | DownReason::Unknown => StopReason::Kill,
            },
        });
    }
}

/// Stops every running application and forgets every loaded one once the
/// controller has stopped: steps `Closing`, runs each sequence on its own
/// helper, as the controller does, and reads each landing from its channel.
/// When the core has ended it sends `done` and drops the state, the verdict
/// sender with it, so every reader takes the controller's end reason from
/// its plane. A step that panics leaves a torn core, which is not stepped
/// again: the state drops, and every hold in it fires.
async fn wind_down(mut shell: Shell, owner: Owner, done: oneshot::Sender<()>) {
    let mut event = Some(Event::Closing);
    while let Some(next) = event.take() {
        if catch_sync(|| shell.apply(next, None, &owner)).is_err() {
            let _ = end_value(shell);
            return;
        }
        if shell.core.ended {
            break;
        }
        let Some(seq) = shell.flights.keys().min().copied() else {
            break;
        };
        let Some(mut flight) = shell.flights.remove(&seq) else {
            break;
        };
        let landed = (&mut flight.rx).await.ok();
        event = Some(shell.landed(seq, flight, landed));
    }
    let _ = send_contained(done, ());
    let _ = end_value(shell);
}

/// The application controller: its state, which leaves for the wind-down
/// when the controller stops, and the mark of a turn in progress.
struct Controller {
    shell: Option<Shell>,
    /// Set before each step of the core and cleared after it, with no await
    /// between: found set, a turn panicked and the core is torn.
    torn: bool,
}

impl Controller {
    fn new(verdict: watch::Sender<Verdict>) -> Self {
        Self {
            shell: Some(Shell::new(verdict)),
            torn: false,
        }
    }
}

impl Actor for Controller {
    type Message = Msg;
    type Response = ();

    /// A torn core is never stepped again: a notice delivered at the stop
    /// after a turn panicked changes nothing, and the mark stays set.
    async fn handle(&mut self, msg: Msg, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.torn {
            return Ok(());
        }
        let Some(shell) = self.shell.as_mut() else {
            return Ok(());
        };
        let event = match msg {
            Msg::Call(call, value, reply) => {
                shell.next_req += 1;
                let req = shell.next_req;
                shell.replies.insert(req, reply);
                if let Some(value) = value {
                    shell.loads.insert(req, value);
                }
                Event::Call(req, call)
            }
            Msg::Landed(seq) => {
                let Some(mut flight) = shell.flights.remove(&seq) else {
                    return Ok(());
                };
                let done = flight.rx.try_recv().ok();
                shell.landed(seq, flight, done)
            }
            Msg::TopDown { app, run, reason } => Event::TopEnded { app, run, reason },
        };
        let owner = Owner::of(ctx);
        self.torn = true;
        shell.apply(event, Some(ctx), &owner);
        self.torn = false;
        Ok(())
    }

    /// Ended by any reason but `Kill`, with its core whole, moves its state
    /// to one wind-down helper before its first await and waits for its done
    /// signal. The wait is bounded by the controller's own stop ladder: an
    /// abort of the controller's task cuts only this wait. With its core torn
    /// it drops the state, and every hold in it fires.
    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let Some(shell) = self.shell.take() else {
            return Ok(());
        };
        if matches!(reason, StopReason::Kill) || self.torn || shell.core.ended {
            let _ = end_value(shell);
            return Ok(());
        }
        let owner = Owner::of(ctx);
        let (done_tx, done) = oneshot::channel();
        app_helper(
            &ctx.resolved_system(),
            &owner,
            wind_down(shell, owner.clone(), done_tx),
        );
        let _ = done.await;
        Ok(())
    }
}

/// Panic points of the tests: each fires once after it is armed.
#[cfg(test)]
mod points {
    use std::sync::atomic::{AtomicU8, Ordering};

    #[derive(Debug, Clone, Copy)]
    pub(super) enum Point {
        /// Between the core's issue of a sequence and its launch.
        BeforeLaunch = 1,
        /// At the start of a stop's top step.
        AtTop = 2,
    }

    static ARMED: AtomicU8 = AtomicU8::new(0);

    pub(super) fn arm(point: Point) {
        ARMED.store(point as u8, Ordering::SeqCst);
    }

    pub(super) fn hit(point: Point) {
        let armed = ARMED.compare_exchange(point as u8, 0, Ordering::SeqCst, Ordering::SeqCst);
        assert!(armed.is_err(), "a planted panic at {point:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use RestartType::{Permanent as P, Temporary as T, Transient as Tr};

    /// Whether what an application owns has released its hold at a landing.
    const F: bool = true;
    const H: bool = false;

    /// One input of a row: a call, or the end of the sequence or top an
    /// application has in flight.
    enum Ev {
        Load(&'static str, Option<&'static str>, &'static [&'static str]),
        Start(&'static str, RestartType),
        Ensure(&'static str, RestartType),
        Stop(&'static str),
        StopAll,
        Shutdown,
        Which,
        Up(&'static str),
        Fail(&'static str, bool),
        Refused(&'static str),
        LostStart(&'static str, bool),
        Down(&'static str, bool),
        LostStop(&'static str, bool),
        End(&'static str, StopReason),
        /// The top of the `n`th run of an application ended.
        EndRun(&'static str, usize, StopReason),
        Release(&'static str),
        Closing,
    }

    fn render(effect: &Effect, out: &mut Vec<String>, seqs: &mut HashMap<String, SeqId>) {
        let mark = |system: &AppSystem| if *system == AppSystem::Own { "!" } else { "" };
        let error = |e: &AppError| {
            let name = format!("{e:?}");
            let name = name.split(['(', ' ', '{']).next().unwrap_or("").to_owned();
            match e {
                AppError::StartFailed { lost, .. } if !lost.is_empty() => {
                    format!("{name}(lost={})", lost.join(","))
                }
                _ => name,
            }
        };
        let rows = |v: &Rows| {
            v.iter()
                .map(|(n, s)| {
                    if s.is_some() {
                        n.clone()
                    } else {
                        format!("{n}=lost")
                    }
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        out.push(match effect {
            Effect::RunStart { seq, app, system } => {
                seqs.insert(app.clone(), *seq);
                format!("+{app}{}", mark(system))
            }
            Effect::RunStop { seq, app, system } => {
                seqs.insert(app.clone(), *seq);
                format!("-{app}{}", mark(system))
            }
            Effect::Reply(req, answer) => format!(
                "#{req}:{}",
                match answer {
                    Answer::Unit(Ok(())) | Answer::Stop(Ok(AppStop { held: false, .. })) => {
                        "ok".to_owned()
                    }
                    Answer::Stop(Ok(_)) => "ok held".to_owned(),
                    Answer::Names(Ok(v)) => v.join(","),
                    Answer::Stops(Ok(v)) => rows(v),
                    Answer::Which(Ok(v)) => v
                        .iter()
                        .map(|i| format!("{}={:?}", i.name, i.state))
                        .collect::<Vec<_>>()
                        .join(","),
                    Answer::Unit(Err(e)) | Answer::Names(Err(e)) | Answer::Stop(Err(e)) => error(e),
                    Answer::Stops(Err(e)) | Answer::Which(Err(e)) => error(e),
                }
            ),
            Effect::Forget(name) => format!("forget:{name}"),
            Effect::Node(v) => {
                let cause = match v.cause {
                    NodeCause::Ended(_) => "Ended",
                    NodeCause::StartFailed(_) => "StartFailed",
                };
                let stopped = if v.stopped.is_empty() {
                    String::new()
                } else {
                    format!(" stopped:{}", rows(&v.stopped))
                };
                format!("node:{}:{cause}{stopped}", v.application)
            }
            Effect::End => "end".to_owned(),
            Effect::Adopt(..) | Effect::Watch { .. } => return,
        });
    }

    /// The rows of the table: a name, the inputs, and the rendered effects.
    fn application_rows() -> Vec<(&'static str, Vec<Ev>, &'static str)> {
        use Ev::*;
        let graceful = || StopReason::Graceful;
        let failure = || StopReason::Failure(crate::ActorError::User("no".into()));
        vec![
            ("start needs every dependency running", vec![Load("a", None, &[]), Load("b", None, &["a"]), Start("b", P)],
                "#1:ok #2:ok #3:DependencyNotRunning"),
            ("ensure starts in order and leaves out what runs",
                vec![Load("a", None, &[]), Load("b", None, &["a"]), Load("c", None, &["b"]), Start("a", T), Up("a"),
                    Ensure("c", T), Up("b"), Up("c")],
                "#1:ok #2:ok #3:ok +a! #4:ok +b! +c! #5:b,c"),
            ("ensure stops what it started on a failure",
                vec![Load("a", None, &[]), Load("b", None, &["a"]), Ensure("b", T), Up("a"), Fail("b", F), Down("a", F)],
                "#1:ok #2:ok +a! +b! -a! #3:StartFailed"),
            ("a transient end with Graceful runs the hooks and stops nothing else",
                vec![Load("t", None, &[]), Start("t", Tr), Up("t"), End("t", graceful()), Down("t", F), Stop("t")],
                "#1:ok +t! #2:ok -t! #3:NotStarted"),
            ("a transient end with Shutdown stops the others in reverse start order",
                vec![Load("a", None, &[]), Load("b", None, &[]), Load("t", None, &[]), Start("a", T), Up("a"),
                    Start("b", T), Up("b"), Start("t", Tr), Up("t"), End("t", StopReason::Shutdown("down".into())),
                    Down("t", F), Down("b", F), Down("a", F)],
                "#1:ok #2:ok #3:ok +a! #4:ok +b! #5:ok +t! #6:ok -t! -b! -a! node:t:Ended stopped:b,a"),
            ("a permanent failed start ends the node once",
                vec![Load("a", None, &[]), Load("p", None, &[]), Start("a", T), Up("a"), Start("p", P), Fail("p", F),
                    Down("a", F), Start("a", P), Up("a"), End("a", graceful()), Down("a", F)],
                "#1:ok #2:ok +a! #3:ok +p! #4:StartFailed -a! node:p:StartFailed stopped:a +a! #5:ok -a!"),
            ("stop_all stops in reverse start order and never walks a shared system",
                vec![Load("a", Some("s"), &[]), Load("b", Some("s"), &[]), Start("a", T), Up("a"), Start("b", T),
                    Up("b"), StopAll, Down("b", F), Down("a", F)],
                "#1:ok #2:ok +a #3:ok +b #4:ok -b -a #5:b,a"),
            ("one sequence runs per system",
                vec![Load("a", Some("s"), &[]), Load("b", Some("s"), &[]), Load("c", None, &[]), Start("a", T),
                    Start("b", T), Start("c", T), Stop("a"), Which],
                "#1:ok #2:ok #3:ok +a #5:StartInFlight +c! #7:StartInFlight #8:a=Starting,b=Loaded,c=Starting"),
            ("a system of its own is walked at the stop and started again",
                vec![Load("a", None, &[]), Start("a", T), Up("a"), Stop("a"), Down("a", F), Start("a", T)],
                "#1:ok +a! #2:ok -a! #3:ok +a!"),
            ("stop_all waits for a start in flight, then stops it",
                vec![Load("a", None, &[]), Start("a", T), StopAll, Up("a"), Down("a", F)],
                "#1:ok +a! #2:ok -a! #3:a"),
            ("shutdown stops all, forgets all and ends the controller; a later call gets no answer",
                vec![Load("a", None, &[]), Load("b", Some("s"), &[]), Start("a", T), Up("a"), Shutdown, Down("a", F),
                    Stop("a")],
                "#1:ok #2:ok +a! #3:ok -a! #4:a forget:a forget:b end"),
            ("a held top holds the application until it is released",
                vec![Load("a", None, &[]), Start("a", T), Up("a"), Stop("a"), Down("a", H), Start("a", T), Stop("a"),
                    Release("a"), Start("a", T)],
                "#1:ok +a! #2:ok -a! #3:ok held #4:Held #5:Held +a!"),
            ("a drain lists a held application and goes on",
                vec![Load("a", None, &[]), Load("b", None, &[]), Start("a", T), Up("a"), Start("b", T), Up("b"), StopAll,
                    Down("b", H), Down("a", F), Which],
                "#1:ok #2:ok +a! #3:ok +b! #4:ok -b! -a! #5:b,a #6:a=Loaded,b=Held"),
            ("an end after a held stop has no node effect",
                vec![Load("p", None, &[]), Start("p", P), Up("p"), Stop("p"), Down("p", H), End("p", StopReason::Kill)],
                "#1:ok +p! #2:ok -p! #3:ok held"),
            ("a stop that lands held",
                vec![Load("a", None, &[]), Start("a", T), Up("a"), Stop("a"), Down("a", H), Start("a", T), Release("a"),
                    Start("a", T)],
                "#1:ok +a! #2:ok -a! #3:ok held #4:Held +a!"),
            ("a lost stop",
                vec![Load("p", None, &[]), Start("p", P), Up("p"), Stop("p"), LostStop("p", H), Which],
                "#1:ok +p! #2:ok -p! #3:StopLost #4:p=Held"),
            ("a lost start",
                vec![Load("p", None, &[]), Start("p", P), LostStart("p", H), Which],
                "#1:ok +p! #2:StartFailed node:p:StartFailed #3:p=Held"),
            ("a refusal before start",
                vec![Load("p", None, &[]), Start("p", P), Refused("p"), Which],
                "#1:ok +p! #2:SystemTaken #3:p=Loaded"),
            ("a failed start that leaves something running",
                vec![Load("a", None, &[]), Start("a", T), Fail("a", H), Start("a", T), Release("a"), Start("a", T)],
                "#1:ok +a! #2:StartFailed #3:Held +a!"),
            ("an end seen after the stop was accepted",
                vec![Load("p", None, &[]), Start("p", P), Up("p"), Stop("p"), End("p", failure()), Down("p", F)],
                "#1:ok +p! #2:ok -p! #3:ok"),
            ("an end of an earlier run, in R of a later one",
                vec![Load("p", None, &[]), Start("p", P), Up("p"), Stop("p"), Down("p", F), Start("p", P), Up("p"),
                    EndRun("p", 1, failure())],
                "#1:ok +p! #2:ok -p! #3:ok +p! #4:ok"),
            ("Closing waits for a stop in flight and settles the verdict",
                vec![Load("a", None, &[]), Load("p", None, &[]), Start("a", T), Up("a"), Start("p", P), Stop("a"),
                    Closing, Fail("p", F), Down("a", F)],
                "#1:ok #2:ok +a! #3:ok +p! -a! #4:StartFailed #5:ok forget:a forget:p end"),
            ("a held dependency refuses an ensure",
                vec![Load("a", None, &[]), Load("b", None, &["a"]), Start("a", T), Up("a"), Stop("a"), Down("a", H),
                    Ensure("b", T)],
                "#1:ok #2:ok +a! #3:ok -a! #4:ok held #5:Held"),
            ("a drain lists a lost stop",
                vec![Load("a", None, &[]), Load("b", None, &[]), Start("a", T), Up("a"), Start("b", T), Up("b"), StopAll,
                    LostStop("b", F), Down("a", F), Which],
                "#1:ok #2:ok +a! #3:ok +b! #4:ok -b! -a! #5:b=lost,a #6:a=Loaded,b=Loaded"),
            ("a job's rollback stop is lost and the drain still runs",
                vec![Load("a", None, &[]), Load("b", None, &["a"]), Ensure("b", T), Up("a"), Fail("b", F),
                    LostStop("a", H), StopAll, Which],
                "#1:ok #2:ok +a! +b! -a! #3:StartFailed(lost=a) #4: #5:a=Held,b=Loaded"),
            ("the node drain carries its rows",
                vec![Load("a", None, &[]), Load("p", None, &[]), Start("a", T), Up("a"), Start("p", P), Up("p"),
                    End("p", failure()), Down("p", F), Down("a", F)],
                "#1:ok #2:ok +a! #3:ok +p! #4:ok -p! -a! node:p:Ended stopped:a"),
        ]
    }

    /// Feeds one row's inputs to a fresh core and renders every effect.
    fn run_application_row(events: Vec<Ev>) -> String {
        use Ev::*;
        let mut core = Core::default();
        let (mut req, mut out) = (0, Vec::new());
        let (mut seqs, mut runs) = (HashMap::new(), HashMap::<String, Vec<SeqId>>::new());
        for ev in events {
            let mut call = |c| {
                req += 1;
                Event::Call(req, c)
            };
            let seq = |app: &str| seqs.get(app).copied().unwrap_or(0);
            let run = |app: &str, n: usize| {
                runs.get(app)
                    .and_then(|r| r.get(n - 1))
                    .copied()
                    .unwrap_or(0)
            };
            let event = match ev {
                Load(n, s, d) => call(Call::Load {
                    name: n.into(),
                    system: s.map_or(AppSystem::Own, |s| AppSystem::Shared(s.into())),
                    deps: d.iter().map(|d| (*d).into()).collect(),
                }),
                Start(n, k) => call(Call::Start(n.into(), k)),
                Ensure(n, k) => call(Call::Ensure(n.into(), k)),
                Stop(n) => call(Call::Stop(n.into())),
                StopAll => call(Call::StopAll),
                Shutdown => call(Call::Shutdown),
                Which => call(Call::Which),
                Up(a) => Event::Started {
                    seq: seq(a),
                    result: Ok(()),
                    free: F,
                },
                Fail(a, free) => Event::Started {
                    seq: seq(a),
                    result: Err(start_failed(a, "no")),
                    free,
                },
                LostStart(a, free) => Event::Started {
                    seq: seq(a),
                    result: Err(start_failed(a, "lost")),
                    free,
                },
                Refused(a) => Event::Refused {
                    seq: seq(a),
                    error: AppError::SystemTaken(a.into()),
                },
                Down(a, free) => {
                    let top = StopReport {
                        outcome: StopOutcome::Graceful,
                        reason: Some(StopReason::Graceful),
                    };
                    let stop = AppStop {
                        top,
                        system: None,
                        held: false,
                    };
                    Event::Stopped {
                        seq: seq(a),
                        stop: Some(stop),
                        free,
                    }
                }
                LostStop(a, free) => Event::Stopped {
                    seq: seq(a),
                    stop: None,
                    free,
                },
                Release(a) => Event::Released { app: a.into() },
                Closing => Event::Closing,
                End(a, reason) => Event::TopEnded {
                    app: a.into(),
                    run: run(a, runs.get(a).map_or(1, Vec::len)),
                    reason,
                },
                EndRun(a, n, reason) => Event::TopEnded {
                    app: a.into(),
                    run: run(a, n),
                    reason,
                },
            };
            for effect in core.step(event) {
                if let Effect::Watch { app, run } = &effect {
                    runs.entry(app.clone()).or_default().push(*run);
                }
                render(&effect, &mut out, &mut seqs);
            }
        }
        out.join(" ")
    }

    #[test]
    fn application_table() {
        for (name, events, expected) in application_rows() {
            assert_eq!(run_application_row(events), expected, "row: {name}");
        }
        // The default bound of `which_applications` is the 5 seconds the docs name.
        assert_eq!(WHICH_BOUND, Duration::from_secs(5));
    }

    /// A top that parks in a handler until its gate opens.
    struct Parked;

    impl Actor for Parked {
        type Message = (oneshot::Sender<()>, oneshot::Receiver<()>);
        type Response = ();

        async fn handle(
            &mut self,
            msg: Self::Message,
            _: &mut ActorContext<Self>,
        ) -> ActorResult<()> {
            let (entered, open) = msg;
            let _ = entered.send(());
            let _ = open.await;
            Ok(())
        }
    }

    struct Gated(
        &'static str,
        tokio::sync::mpsc::UnboundedSender<ActorHandle<Parked>>,
        tokio::sync::mpsc::UnboundedSender<()>,
    );

    impl Application for Gated {
        fn name(&self) -> &str {
            self.0
        }

        fn system(&self) -> AppSystem {
            if self.0 == "lib-own" {
                AppSystem::Own
            } else {
                AppSystem::Shared(self.0.into())
            }
        }

        async fn start(&mut self, system: &Arc<ActorSystem>) -> Result<AppRoot, StartError> {
            let top = Parked
                .spawn()
                .on_system(system)
                .kill_grace(Duration::from_secs(60))
                .await?;
            let _ = self.1.send(top.clone());
            Ok(AppRoot::of(&top))
        }

        async fn stop(&mut self) {
            let _ = self.2.send(());
        }
    }

    async fn within<F: Future>(fut: F) -> F::Output {
        // A hang detector, never a wait for an event.
        tokio::time::timeout(Duration::from_secs(30), fut)
            .await
            .expect("hung")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn application_panic_points() {
        use tokio::sync::mpsc::unbounded_channel;
        // A stop lost at its top step: `StopLost`, held until the top took
        // the `Kill` its hold raised, and no hook runs.
        let apps = Applications::spawn().await.unwrap();
        let ((tops_tx, mut tops), (stops_tx, mut stops)) =
            (unbounded_channel(), unbounded_channel());
        apps.load(Gated("lib-own", tops_tx.clone(), stops_tx.clone()))
            .await
            .unwrap();
        apps.start("lib-own", RestartType::Temporary).await.unwrap();
        let top = tops.recv().await.unwrap();
        let ((entered_tx, entered), (open, open_rx)) = (oneshot::channel(), oneshot::channel());
        top.notify((entered_tx, open_rx)).await.unwrap();
        entered.await.unwrap();
        points::arm(points::Point::AtTop);
        assert!(matches!(
            apps.stop("lib-own").await,
            Err(AppError::StopLost(_))
        ));
        assert!(matches!(
            apps.start("lib-own", RestartType::Temporary).await,
            Err(AppError::Held(_))
        ));
        open.send(()).unwrap();
        assert_eq!(within(top.wait_stopped()).await, StopReason::Kill);
        assert!(stops.try_recv().is_err(), "no hook runs for a lost stop");
        within(async {
            while apps.which_applications().await.unwrap()[0].state != AppState::Loaded {}
        })
        .await;
        apps.start("lib-own", RestartType::Temporary).await.unwrap();
        within(apps.shutdown()).await.unwrap();
        // A controller turn that panics between the issue and the launch: the
        // core is torn, the state drops and the running top is killed.
        let apps = Applications::spawn().await.unwrap();
        let (tops_tx, mut tops) = unbounded_channel();
        apps.load(Gated("lib-launch", tops_tx, stops_tx))
            .await
            .unwrap();
        apps.start("lib-launch", RestartType::Temporary)
            .await
            .unwrap();
        let top = tops.recv().await.unwrap();
        points::arm(points::Point::BeforeLaunch);
        assert!(matches!(
            apps.stop("lib-launch").await,
            Err(AppError::Ended(_))
        ));
        let ended = within(apps.node_shutdown()).await;
        assert!(
            matches!(ended, Err(AppError::Ended(StopReason::Failure(_)))),
            "{ended:?}"
        );
        assert_eq!(within(top.wait_stopped()).await, StopReason::Kill);
        // Two tops end and the turn of the first end panics: the other end,
        // delivered at the stop, steps nothing on the torn core, so no hook
        // runs and the state drops.
        let apps = Applications::spawn().await.unwrap();
        let ((tops_tx, mut tops), (stops_tx, mut stops)) =
            (unbounded_channel(), unbounded_channel());
        for name in ["lib-t1", "lib-t2"] {
            apps.load(Gated(name, tops_tx.clone(), stops_tx.clone()))
                .await
                .unwrap();
            apps.start(name, RestartType::Temporary).await.unwrap();
        }
        let ends = [tops.recv().await.unwrap(), tops.recv().await.unwrap()];
        points::arm(points::Point::BeforeLaunch);
        for top in &ends {
            // The second top may already be ending: the torn controller's
            // dropped state stops it through its hold.
            let _ = top.stop(StopReason::Kill).await;
        }
        for top in &ends {
            assert_eq!(within(top.wait_stopped()).await, StopReason::Kill);
        }
        let ended = within(apps.node_shutdown()).await;
        assert!(
            matches!(ended, Err(AppError::Ended(StopReason::Failure(_)))),
            "{ended:?}"
        );
        assert!(stops.try_recv().is_err(), "no hook runs on a torn core");
    }
}

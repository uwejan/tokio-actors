//! The messages an actor's channels carry: the user envelope, the system
//! channel's messages and the requests that travel on it.

use std::fmt::Formatter;
use std::sync::Arc;

use tokio::sync::oneshot;

use crate::error::ActorError;
use crate::types::{ActorId, ChildCounts, ChildInfo, StatusRequest, StopReason};

/// Tags a message made from a [`Down`](crate::types::Down), a
/// [`LinkExit`](crate::types::LinkExit), a
/// [`GroupChange`](crate::types::GroupChange) or a
/// [`RequestReply`](crate::types::RequestReply), so the actor that made the
/// monitor, link or request can revoke a notice that is already queued. It is
/// opaque; it only appears in [`Envelope::Signal`].
#[doc(hidden)]
#[derive(Clone)]
pub struct SignalTag {
    pub(crate) shared: Arc<crate::actor::monitor::MonitorShared>,
}

impl std::fmt::Debug for SignalTag {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalTag").finish_non_exhaustive()
    }
}

/// Internal system-level messages sent via the system channel.
///
/// These have priority over user messages via `biased; select!`, and sit
/// below the stop lane, which the run loop observes first of all (see the
/// `runtime` module docs). Stop/kill requests do not travel through this
/// channel: they are delivered through the stop lane instead, so they are
/// never delayed by anything queued here. A child's death and a restart
/// attempt's completion likewise never travel here: they are delivered
/// through the supervisor's death plane and restart plane (per-incarnation
/// fate cells and a parent-owned `JoinSet`, respectively) instead.
///
/// The full branch order, highest priority first: stop lane, system channel,
/// restart plane, death plane, verdict signal, forwarder plane, priority lane,
/// mailbox.
#[derive(Debug)]
pub(crate) enum SystemMessage {
    /// Request a status snapshot.
    GetStatus(StatusRequest),
    /// Link, unlink or exit items are waiting in the actor's signal inbox.
    Signals,
    /// A request made through a
    /// [`SupervisorRef`](crate::actor::supervisor_ref::SupervisorRef),
    /// answered at a boundary of the run loop. Boxed so the message stays
    /// the size of a status request.
    Supervise(Box<SuperviseRequest>),
    /// A call of the `sys` bundle, boxed so the channel's slots stay the
    /// size of a status request.
    #[cfg(feature = "sys")]
    #[allow(dead_code)]
    Sys(Box<SysCommand>),
}

/// One request of a `SupervisorRef`. Each carries the sender its answer goes
/// to, except [`Settle`](Self::Settle) and [`Start`](Self::Start), whose
/// answers travel inside what they carry.
#[derive(Debug)]
pub(crate) enum SuperviseRequest {
    /// `restart_child`.
    Restart {
        /// The child to restart.
        child: ActorId,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<(), crate::error::SupervisionError>>,
    },
    /// `delete_child`.
    Delete {
        /// The child whose spec is removed.
        child: ActorId,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<(), crate::error::SupervisionError>>,
    },
    /// `which_children`.
    Which {
        /// Where the answer goes.
        reply: oneshot::Sender<Result<Vec<ChildInfo>, crate::error::SupervisionError>>,
    },
    /// `get_childspec`.
    Spec {
        /// The child whose spec is read.
        child: ActorId,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<ChildInfo, crate::error::SupervisionError>>,
    },
    /// `count_children`.
    Count {
        /// Where the answer goes.
        reply: oneshot::Sender<Result<ChildCounts, crate::error::SupervisionError>>,
    },
    /// `terminate_child`: the stop is committed at the boundary, and the
    /// answer goes out when the child and its subtree ended.
    Terminate {
        /// The child to stop.
        child: ActorId,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<(), crate::error::SupervisorRefError>>,
    },
    /// The end of a child stopped by a `Terminate`, seen by the reply helper:
    /// the supervisor finishes the classification of that stop, if its death
    /// arm has not, before the answer goes out.
    Settle {
        /// The child whose stop ended.
        child: ActorId,
        /// The incarnation the stop concerns.
        incarnation: u64,
        /// The kind the stop is classified with.
        kind: crate::actor::supervision::ManualStop,
        /// How the incarnation ended.
        reason: StopReason,
    },
    /// `start_child`: the typed job the ref built, opened at one site by
    /// `supervisor_ref::serve` and never seen by user code.
    Start(Box<dyn std::any::Any + Send>),
}

/// Where the answer to a `sys` command goes.
#[cfg(feature = "sys")]
pub(crate) type Reply<T> = oneshot::Sender<T>;

/// A call of the `sys` bundle, as the system channel carries it. The run
/// loop serves it between two turns.
#[cfg(feature = "sys")]
#[allow(dead_code)]
pub(crate) enum SysCommand {
    /// Stop taking messages.
    Suspend(Reply<()>),
    /// Take messages again.
    Resume(Reply<()>),
    /// Turn the statistics on or off.
    SetStatistics(bool, Reply<()>),
    /// Read the statistics.
    Statistics(Reply<Option<crate::actor::sys::Statistics>>),
    /// Turn the log on with this capacity, or off.
    SetLog(Option<usize>, Reply<()>),
    /// Read the log.
    Log(Reply<Option<Vec<crate::actor::sys::DebugEntry>>>),
    /// Turn the trace on or off.
    SetTrace(bool, Reply<()>),
    /// Install a function under an id.
    Install(
        String,
        Box<dyn FnMut(&crate::actor::sys::DebugEntry) -> crate::actor::sys::Install + Send>,
        Reply<()>,
    ),
    /// Remove the function installed under an id.
    Remove(String, Reply<bool>),
    /// Run a read or a change of the actor's value on its task.
    State(Box<dyn crate::actor::sys_state::StateOp>),
    /// Count a run, for the tests of the request path.
    #[cfg(test)]
    Probe(std::sync::Arc<std::sync::atomic::AtomicUsize>, Reply<()>),
}

#[cfg(feature = "sys")]
impl std::fmt::Debug for SysCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Suspend(_) => "Suspend",
            Self::Resume(_) => "Resume",
            Self::SetStatistics(..) => "SetStatistics",
            Self::Statistics(_) => "Statistics",
            Self::SetLog(..) => "SetLog",
            Self::Log(_) => "Log",
            Self::SetTrace(..) => "SetTrace",
            Self::Install(..) => "Install",
            Self::Remove(..) => "Remove",
            Self::State(_) => "State",
            #[cfg(test)]
            Self::Probe(..) => "Probe",
        };
        f.debug_tuple("SysCommand").field(&name).finish()
    }
}

/// Internal representation of actual payloads sent to actors.
#[doc(hidden)]
#[derive(Debug)]
pub enum Envelope<M, R> {
    /// A standard message with an optional responder.
    Message {
        /// The message payload.
        payload: M,
        /// The channel to send the response to (if any).
        responder: Option<oneshot::Sender<Result<R, ActorError>>>,
    },
    /// A message built from a [`Down`](crate::types::Down), a
    /// [`LinkExit`](crate::types::LinkExit), a
    /// [`GroupChange`](crate::types::GroupChange) or a
    /// [`RequestReply`](crate::types::RequestReply), carrying the tag that
    /// lets the actor that made the monitor, link or request drop it while it
    /// is still queued. It reaches
    /// `handle` exactly as a [`Message`](Envelope::Message) with no responder
    /// does.
    Signal {
        /// Identifies the monitor, link or request the notice belongs to.
        tag: SignalTag,
        /// The message the map function made from the notice.
        payload: M,
    },
}

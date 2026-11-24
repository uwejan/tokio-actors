//! Identity and mailbox readings of a handle: ids, lane sizes, liveness and status.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use tokio::sync::mpsc;

use super::{ActorHandle, HandleBlock, LaneFlags};
use crate::actor::incarnation::{IncarnationRef, StatusRx, StopLane};
use crate::actor::mailbox::MailTx;
#[cfg(feature = "sys")]
use crate::actor::sys::Sys;
use crate::actor::{Actor, ActorEnvelope};
use crate::error::AskError;
use crate::system::{read_status, ActorSystem};
use crate::types::{ActorId, ActorStatus, Incarnation};

impl<A: Actor> ActorHandle<A> {
    /// As [`new`](Self::new), with the lane's shared flags made by the
    /// caller.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn with_lane_flags(
        id: ActorId,
        tx: MailTx<ActorEnvelope<A>>,
        stop_lane: StopLane,
        mailbox_capacity: usize,
        status_rx: StatusRx,
        priority_tx: Option<mpsc::Sender<ActorEnvelope<A>>>,
        lane_flags: Option<Arc<LaneFlags>>,
    ) -> Self {
        Self {
            inner: Arc::new(HandleBlock {
                id,
                tx,
                priority_tx,
                stop_lane,
                mailbox_capacity,
                status_rx,
                lane_flags,
            }),
        }
    }

    /// The priority lane a plain handle sends on: the lane's sender, unless
    /// the actor has none or leaves it to aliases.
    #[inline]
    pub(super) fn open_lane(&self) -> Option<&mpsc::Sender<ActorEnvelope<A>>> {
        let lane = self.inner.priority_tx.as_ref()?;
        match &self.inner.lane_flags {
            Some(flags) if flags.by_alias => None,
            _ => Some(lane),
        }
    }

    /// The "lane full" warning flag, when the actor has a lane.
    pub(super) fn lane_warned(&self) -> Option<&AtomicBool> {
        self.inner.lane_flags.as_deref().map(|flags| &flags.warned)
    }

    /// The `sys` calls of this actor: statistics, log, trace, installed
    /// functions, suspension and, when the actor allows it, `inspect` and
    /// `replace_state`.
    ///
    /// Needs the `sys` feature, which is on by default.
    #[cfg(feature = "sys")]
    pub fn sys(&self) -> Sys<'_, A> {
        Sys::new(self)
    }

    /// The incarnation's abort record, which carries the bits of the `sys`
    /// bundle and the system channel the calls ride.
    #[allow(dead_code)]
    pub(crate) fn sys_record(&self) -> &IncarnationRef {
        self.inner.stop_lane.abort_record()
    }

    /// The identity of the system this actor was spawned in: the address of
    /// the system's telemetry state, which the record keeps from being reused,
    /// so it is unique to one system for the life of the process; `0` for a
    /// handle no spawn made (only a unit test builds one).
    /// Two handles are equal when they have the same id and the same
    /// system.
    pub(crate) fn system_generation(&self) -> u64 {
        self.inner.stop_lane.abort_record().system_identity()
    }

    /// The system this actor was spawned in, if it is still alive: `None`
    /// once every holder of it has let go, and for a handle no spawn made.
    /// Resolved by identity, never by name, so a later system of the same
    /// name is never taken for it.
    pub(crate) fn system(&self) -> Option<Arc<ActorSystem>> {
        self.inner.stop_lane.abort_record().system()
    }

    /// Returns a clone of this actor's stop lane sender (for wiring the
    /// supervision escalation ladder and the system registry's shutdown
    /// sweep without a mailbox round-trip).
    pub(crate) fn stop_lane(&self) -> StopLane {
        self.inner.stop_lane.clone()
    }

    /// This actor's stop lane, borrowed: for the caller that only reads or
    /// narrows it and needs no clone of the lane, let alone of the handle.
    pub(crate) fn stop_lane_ref(&self) -> &StopLane {
        &self.inner.stop_lane
    }

    /// `Err(AskError::CallingSelf)` when the calling task is the task that
    /// runs this incarnation: a request it makes could never be answered. The
    /// check reads one thread-local, the key of the incarnation the thread is
    /// polling, and only a caller on that thread goes on to compare the task
    /// ids. A `spawn_blocking` thread, a task spawned from a callback, a task
    /// polled inside the actor's poll under an id of its own, another
    /// incarnation and an ended incarnation never match.
    #[inline]
    pub(super) fn refuse_self_call(&self) -> Result<(), AskError> {
        if self.inner.stop_lane.abort_record().is_self_call() {
            return Err(AskError::CallingSelf);
        }
        Ok(())
    }

    /// The identity fields of this actor's log statements.
    #[cfg(feature = "tracing")]
    pub(crate) fn log_fields(&self) -> crate::telemetry::LogFields<'static> {
        crate::telemetry::log_fields(self.inner.stop_lane.abort_record(), None)
    }

    /// Returns a clone of the runtime status receiver (used by the system
    /// registry's shutdown cascade - see the module docs on the two
    /// observability planes).
    pub(crate) fn status_rx(&self) -> StatusRx {
        self.inner.status_rx.clone()
    }

    /// Returns the unique identifier of the actor.
    pub fn id(&self) -> &ActorId {
        &self.inner.id
    }

    /// Returns the [`Incarnation`] this handle reaches: the single run of the
    /// actor whose channels it holds.
    ///
    /// The [`ActorId`] stays the same across a supervised restart and the
    /// incarnation does not, so two handles to one actor's successive runs
    /// compare equal and report different incarnations.
    ///
    /// # Examples
    /// ```rust
    /// use tokio_actors::{actor::{context::ActorContext, Actor, ActorExt}, ActorResult};
    ///
    /// struct Idle;
    ///
    /// impl Actor for Idle {
    ///     type Message = ();
    ///     type Response = ();
    ///     async fn handle(&mut self, _: (), _: &mut ActorContext<Self>) -> ActorResult<()> {
    ///         Ok(())
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let handle = Idle.spawn().await?;
    /// assert_eq!(handle.get_status().await?.incarnation, handle.incarnation());
    /// # Ok(())
    /// # }
    /// ```
    pub fn incarnation(&self) -> Incarnation {
        self.inner.status_rx.record().incarnation()
    }

    /// This incarnation as the owner of the drops its log lines name: its
    /// id, with the sensitivity its record carries.
    pub(crate) fn owner(&self) -> crate::actor::runtime::Owner {
        crate::actor::runtime::Owner::of(&self.inner.id, self.inner.stop_lane.abort_record())
    }

    /// The wait lists the actor's mailbox was made with.
    #[cfg(test)]
    pub(crate) fn mailbox_lanes(&self) -> usize {
        self.inner.tx.lane_count()
    }

    /// Returns the total capacity of the actor's mailbox.
    pub fn mailbox_capacity(&self) -> usize {
        self.inner.mailbox_capacity
    }

    /// Returns the number of mailbox slots in use: the messages queued plus
    /// the slots senders have reserved and not yet filled, so a slot handed to
    /// a waiting sender counts as in use from that moment.
    pub fn mailbox_len(&self) -> usize {
        self.inner.mailbox_capacity - self.inner.tx.capacity()
    }

    /// The senders parked on a full mailbox now.
    #[cfg(test)]
    pub(crate) fn mailbox_waiting(&self) -> usize {
        self.inner.tx.waiting()
    }

    /// Returns the number of available slots in the mailbox.
    pub fn mailbox_available(&self) -> usize {
        self.inner.tx.capacity()
    }

    /// Returns the priority lane's total capacity, or `0` if this actor has
    /// no priority lane (see
    /// [`MailboxConfig::with_priority_lane`](crate::actor::runtime::MailboxConfig::with_priority_lane)).
    ///
    /// This and [`priority_len`](Self::priority_len) are the two
    /// observability points [`MailboxConfig::with_priority_lane`](crate::actor::runtime::MailboxConfig::with_priority_lane)'s
    /// documented divergence from Erlang refers to: a plain Erlang process
    /// exposes only its total queue length, never a per-lane breakdown.
    pub fn priority_capacity(&self) -> usize {
        self.inner
            .priority_tx
            .as_ref()
            .map_or(0, |tx| tx.max_capacity())
    }

    /// Returns the number of priority-lane slots in use: the messages queued
    /// in the lane plus the slots senders have reserved and not yet filled, or
    /// `0` if this actor has no priority lane. Unaffected by traffic on the
    /// normal mailbox, and vice versa: see
    /// [`mailbox_len`](Self::mailbox_len) for that counter.
    pub fn priority_len(&self) -> usize {
        self.inner
            .priority_tx
            .as_ref()
            .map_or(0, |tx| tx.max_capacity() - tx.capacity())
    }

    /// Returns `true` while the actor's mailbox is open.
    ///
    /// `false` with the actor's name still registered means the actor is
    /// stopping: it accepts nothing from here on (a send returns `Closed`, or
    /// `ResponseDropped` for a send that raced the close; the message is not
    /// processed either way), and the name is free only once its teardown has
    /// ended. A caller that must reach the actor by name waits with
    /// [`wait_stopped`](Self::wait_stopped) on the handle it holds, then looks
    /// the name up again; an actor a supervisor restarts is registered again
    /// as the next incarnation. A lookup that finds nothing is final for an
    /// actor that nothing restarts; for a supervised actor it means either
    /// that the next incarnation is not registered yet or that its supervisor
    /// will not restart it, so bound the retries.
    ///
    /// `true` does not promise that the actor will handle a message: while its
    /// `on_stopped` or a supervisor's final deliveries run it still accepts
    /// messages and drops them unhandled (a `send` then reads
    /// `ResponseDropped`).
    ///
    /// Once the actor has left its message loop for good (its `on_stopped` hook
    /// has returned, or a `Kill` ended the loop, which skips that hook), this
    /// reports `false` immediately, throughout the teardown of its children,
    /// not only once its task has fully exited. Right after
    /// `stop(StopReason::Kill)` returns it can still read `true`, until the
    /// actor reaches its next turn boundary; await `kill_by_id`,
    /// `stop_within(StopReason::Kill, d)` or `wait_stopped()` when the end
    /// matters.
    ///
    /// This differs from Erlang/OTP, where `is_process_alive/1` stays true
    /// until `terminate/2` returns and reads false right after
    /// `exit(Pid, kill)`: here `false` means that the actor has left its
    /// message loop for good and sends are refused, so it turns false before
    /// the end of the children's teardown, and `status()` keeps its own
    /// reading until the actor reaches `Stopped`.
    pub fn is_alive(&self) -> bool {
        !self.inner.tx.is_closed()
    }

    /// Returns the actor's current lifecycle status.
    ///
    /// This is the runtime observability plane (see the module docs):
    /// it reads a per-actor cell maintained directly by the runtime, so it
    /// answers without waiting for the actor, even while the actor is hung
    /// inside a callback and not servicing its mailbox or system channel.
    /// Contrast with
    /// [`get_status`](Self::get_status), the queue plane, which travels
    /// through the system channel and can hang on a stuck actor.
    ///
    /// The cell keeps only the latest value: a short-lived intermediate status
    /// (for example [`Stopping`](ActorStatus::Stopping)) can be missed if it
    /// changes again before this is read. [`Stopped`](ActorStatus::Stopped)
    /// is terminal, so it is never missed this way; see
    /// [`wait_stopped`](Self::wait_stopped).
    ///
    /// An actor whose task was aborted never writes a final status itself;
    /// its status sender closing is the terminal signal instead, and this
    /// reports [`Stopped`](ActorStatus::Stopped) for that case too.
    pub fn status(&self) -> ActorStatus {
        read_status(&self.inner.status_rx)
    }
}

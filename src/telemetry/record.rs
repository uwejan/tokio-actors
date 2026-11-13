//! The read-only view of one actor that a system snapshot returns.

use std::time::Duration;

use super::actor_ref::ActorRef;
use crate::types::ActorStatus;

/// What a system snapshot reports about one actor.
#[derive(Clone, Debug)]
pub struct ActorRecord {
    /// The incarnation, with its ancestors.
    pub actor: ActorRef,
    /// Its registered name.
    pub name: Option<String>,
    /// Its status when the snapshot read it.
    pub status: ActorStatus,
    /// The type name of the actor value.
    pub type_name: &'static str,
    /// The label the actor set for itself.
    pub label: Option<String>,
    /// The messages queued in its mailbox and the slots reserved by senders.
    pub mailbox_len: usize,
    /// The capacity of its mailbox.
    pub mailbox_capacity: usize,
    /// The messages queued in its priority lane.
    pub priority_len: usize,
    /// The capacity of its priority lane; 0 when it has none.
    pub priority_capacity: usize,
    /// The number of child specs it holds: a child that is down or waiting
    /// for a restart counts until its spec is deleted.
    pub child_count: usize,
    /// How long its current turn has run, when turns are timed (turn tracking,
    /// a turn warning threshold or a monitor's `long_schedule`).
    pub in_turn: Option<Duration>,
    /// The actor a send of this actor waits on, when waits are tracked.
    pub waiting_on: Option<ActorRef>,
    /// Whether the actor is suspended and takes no message.
    pub suspended: bool,
    pub(crate) coupling: CouplingInfo,
}

impl ActorRecord {
    /// Number of monitors this actor holds that have not yet been delivered
    /// or revoked.
    pub fn monitors(&self) -> usize {
        self.coupling.monitors
    }

    /// Number of monitors other actors hold on this actor's run that have
    /// not fired or been revoked.
    pub fn monitored_by(&self) -> usize {
        self.coupling.monitored_by
    }

    /// Number of links this actor holds that have not been delivered or
    /// cancelled.
    pub fn links(&self) -> usize {
        self.coupling.links
    }

    /// Number of process-group memberships this actor holds, a membership
    /// joined twice counting twice.
    pub fn pg_groups(&self) -> usize {
        self.coupling.pg_groups
    }

    /// Number of group and scope monitors this actor holds that have not
    /// been revoked.
    pub fn group_monitors(&self) -> usize {
        self.coupling.group_monitors
    }
}

/// Counts of the couplings of an actor, read from its incarnation's shared
/// block by the snapshot, without asking the actor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CouplingInfo {
    pub(crate) monitors: usize,
    pub(crate) monitored_by: usize,
    pub(crate) links: usize,
    pub(crate) pg_groups: usize,
    pub(crate) group_monitors: usize,
}

impl CouplingInfo {
    /// The counts the incarnation behind `record` shows now: the lengths of
    /// its own signal tables as it last recorded them, the monitors other
    /// actors hold on it and its group memberships. Leaf locks only, and no
    /// signal cell is made for an incarnation that has none.
    pub(crate) fn of(record: &crate::actor::subtree::AbortRecord) -> Self {
        let [monitors, links, group_monitors] = record.coupling();
        Self {
            monitors: monitors as usize,
            monitored_by: record.monitored_by(),
            links: links as usize,
            pg_groups: record.pg_joined(),
            group_monitors: group_monitors as usize,
        }
    }
}

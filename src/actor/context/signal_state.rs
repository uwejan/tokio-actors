//! Counts and tables of the actor's monitors, links, requests and group monitors, as the run loop reads them.

use std::sync::Arc;

use super::{ActorContext, ForwarderKind, PanicSlot, Released};
use crate::actor::envelope::SignalTag;
use crate::actor::link::{LinkTable, TrapCell, TrapMap};
use crate::actor::monitor::MonitorState;
use crate::actor::pg_monitor::GroupTable;
use crate::actor::Actor;
use crate::types::{LinkExit, MonitorId};

impl<A: Actor> ActorContext<A> {
    /// Whether the end of this run has anything to look at that only a monitor,
    /// a link, a request or a signal sender leaves: a monitor or a link entry
    /// held now (a late link entry included), the word's push-or-pair bit (set
    /// by every sender of a signal and every pair, before it reads the seal),
    /// or the signal cell, which a map function's panic is kept in
    /// (`keep_map_panic` makes it) and which the first monitor or link makes.
    /// A map function runs only in a task or a delivery that state started,
    /// and the end reads this after the last moment one can run. The receive
    /// timeout, the idle hook, a continuation and an alias run no map that
    /// the end collects, so an actor that uses only them, or only `stop`,
    /// skips the checks.
    #[inline]
    pub(crate) fn has_end_state(&self) -> bool {
        let record = self.stop_lane().abort_record();
        self.monitors
            .as_deref()
            .is_some_and(|state| state.len() != 0 || !state.links.is_empty())
            || record.has_signal_state()
            || record.has_signal_cell()
    }

    /// The monitors this actor holds that have not been delivered or revoked.
    pub(crate) fn monitor_count(&self) -> usize {
        self.monitors.as_deref().map_or(0, MonitorState::len)
    }

    /// The monitor state, when this actor ever made a monitor.
    pub(crate) fn monitors_mut(&mut self) -> Option<&mut MonitorState<A>> {
        self.monitors.as_deref_mut()
    }

    /// Removes the monitor whose notice was just dispatched.
    pub(crate) fn finish_monitor(&mut self, id: MonitorId) {
        if let Some(state) = self.monitors.as_deref_mut() {
            state.finish(id);
        }
    }

    /// A signal was just dispatched: the notice of a death monitor or a link
    /// ends its entry, a group change is counted as handled, the reply of a
    /// request leaves the count. Ids are unique across the four kinds, so at
    /// most one of the calls finds the entry.
    pub(crate) fn finish_signal(&mut self, tag: &SignalTag) {
        self.finish_monitor(tag.shared.id());
        if let Some(state) = self.monitors.as_deref_mut() {
            state.groups.finish(tag);
            state.requests.finish(tag);
        }
    }

    /// The requests this actor made that have not been answered or
    /// cancelled.
    pub(crate) fn request_count(&self) -> usize {
        self.monitors
            .as_deref()
            .map_or(0, |state| state.requests.len())
    }

    /// The group and scope monitors this actor holds that have not been
    /// revoked.
    pub(crate) fn group_monitor_count(&self) -> usize {
        self.monitors
            .as_deref()
            .map_or(0, |state| state.groups.len())
    }

    /// The table of group and scope monitors.
    pub(crate) fn groups_mut(&mut self) -> &mut GroupTable<A> {
        let owner = self.owner();
        &mut self
            .monitors
            .get_or_insert_with(|| Box::new(MonitorState::new(owner)))
            .groups
    }

    /// Starts the entry task of a group or scope monitor on the forwarder
    /// plane.
    pub(crate) fn spawn_group_forwarder<F>(&mut self, id: MonitorId, fut: F)
    where
        F: std::future::Future<Output = Released> + Send + 'static,
    {
        self.spawn_forwarder(ForwarderKind::Group(id), fut, PanicSlot::default());
    }
}

impl<A: Actor> ActorContext<A> {
    /// The links this actor holds and the exits it is delivering; created
    /// with the first one.
    pub(crate) fn link_table(&mut self) -> &mut LinkTable {
        let owner = self.owner();
        &mut self
            .monitors
            .get_or_insert_with(|| Box::new(MonitorState::new(owner)))
            .links
    }

    /// The links this actor holds that have not been delivered or cancelled.
    pub(crate) fn link_count(&self) -> usize {
        self.monitors
            .as_deref()
            .map_or(0, |state| state.links.len())
    }

    /// Whether the stage has link work: an entry waits in the table, or items
    /// wait in the signal inbox.
    pub(crate) fn link_work(&self) -> bool {
        self.monitors
            .as_deref()
            .is_some_and(|state| !state.links.is_empty())
            || self.stop_lane().abort_record().signals_pending()
    }

    /// Whether this actor can deliver a trapped exit now: the flag is set and
    /// a map stands.
    pub(crate) fn can_deliver_exits(&self) -> bool {
        self.stop_lane().abort_record().trap()
            && self
                .monitors
                .as_deref()
                .is_some_and(|state| state.trap.get().is_some())
    }

    /// The stage closes every link nobody has taken; see
    /// [`LinkTable::stage_pass`].
    pub(crate) fn close_links(&mut self) -> Vec<(MonitorId, LinkExit)> {
        let deliver = self.can_deliver_exits();
        self.monitors
            .as_deref_mut()
            .map(|state| state.links.stage_pass(deliver))
            .unwrap_or_default()
    }

    /// The cell the tasks of the links read the trap map from.
    pub(crate) fn trap_share(&mut self) -> Arc<TrapMap<A::Message>> {
        let owner = self.owner();
        self.monitors
            .get_or_insert_with(|| Box::new(MonitorState::new(owner.clone())))
            .trap
            .share(&owner)
    }

    /// The trap cell of this actor.
    pub(crate) fn trap_cell(&mut self) -> &mut TrapCell<A::Message> {
        let owner = self.owner();
        &mut self
            .monitors
            .get_or_insert_with(|| Box::new(MonitorState::new(owner)))
            .trap
    }

    /// Starts the entry task of a link or an exit signal on the forwarder
    /// plane.
    pub(crate) fn spawn_link_forwarder<F>(&mut self, id: MonitorId, fut: F)
    where
        F: std::future::Future<Output = Released> + Send + 'static,
    {
        self.spawn_forwarder(ForwarderKind::Link(id), fut, PanicSlot::default());
    }
}

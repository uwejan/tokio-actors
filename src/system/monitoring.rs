//! Telemetry controls and reads of a system: thresholds, the system monitor, handlers, snapshots and quiescence.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;

use super::{nanos_to_threshold, threshold_to_nanos, ActorSystem, MailView};
use crate::actor::handle::ActorHandle;
use crate::actor::Actor;
use crate::helper::HelperKind;
#[cfg(doc)]
use crate::system::SystemConfig;
use crate::telemetry::{Delivery, MonitorConfig, MonitorError, MonitorSettings};
use crate::types::{ActorId, ActorStatus};

/// What a snapshot copied out of one registry entry.
struct Seen {
    id: ActorId,
    /// Whether the entry was registered under a name, which is its id.
    named: bool,
    seq: u64,
    /// The address of the incarnation's abort record.
    pub(super) key: usize,
    /// The address of the incarnation's subtree token, and of its
    /// supervisor's.
    token: Option<usize>,
    parent: Option<usize>,
    /// The identity of the incarnation's turn clock, which is the caller token
    /// of the wait graph.
    clock: u64,
    /// Present while the incarnation's end is not final; an entry without it
    /// is kept only so its live children can name it as their supervisor.
    live: Option<LiveRead>,
}

/// What a snapshot reads of an actor that has not ended.
struct LiveRead {
    status: ActorStatus,
    mail: MailView,
    children: usize,
    in_turn: Option<Duration>,
    suspended: bool,
    coupling: crate::telemetry::CouplingInfo,
}

/// The reference of the entry `at`, with its ancestry, built from the entries
/// the snapshot copied out: each supervisor is found by its subtree token, so
/// the incarnation named is the one that spawned the child, never one looked
/// up by id. A supervisor the registry does not hold ends the chain there.
fn ref_of(
    seen: &[Seen],
    by_token: &HashMap<usize, usize>,
    refs: &mut [Option<crate::telemetry::ActorRef>],
    at: usize,
) -> crate::telemetry::ActorRef {
    use crate::telemetry::ActorRef;

    let mut chain = vec![at];
    let mut top = at;
    while refs[top].is_none() {
        let next = seen[top]
            .parent
            .and_then(|parent| by_token.get(&parent).copied())
            .filter(|next| !chain.contains(next));
        match next {
            Some(next) => {
                chain.push(next);
                top = next;
            }
            None => break,
        }
    }
    let mut above = if refs[top].is_some() {
        chain.pop();
        refs[top].clone()
    } else {
        None
    };
    while let Some(index) = chain.pop() {
        let row = &seen[index];
        let built = match &above {
            Some(parent) => ActorRef::new_child(row.id.clone(), row.seq, parent),
            None => ActorRef::new_root(row.id.clone(), row.seq),
        };
        refs[index] = Some(built.clone());
        above = Some(built);
    }
    refs[at]
        .clone()
        .expect("the chain ends at the entry asked for")
}

/// One read of the state quiescence compares.
#[derive(PartialEq, Eq)]
struct QuietPass {
    /// Registration number, turns begun, inside a turn, mailbox length and
    /// priority length of each actor whose end is not final.
    rows: Vec<(u64, u64, bool, usize, usize)>,
    /// The restart attempts of all supervisors that have begun and not
    /// settled, and the child deaths their runners landed and the
    /// supervisors have not finished handling.
    in_flight: u64,
    /// Every actor read idle with empty lanes and none in flight.
    idle: bool,
}

impl ActorSystem {
    // - Stuck-turn watchdog --------------------------------------------------

    /// Sets this system's stuck-turn watchdog threshold: how long a single
    /// callback of an actor may run before it is reported by
    /// [`stuck_actors`](Self::stuck_actors) (and, with the `tracing` feature
    /// enabled, logged with a warning). `None` turns the watchdog off (the
    /// default for every system); `Some(Duration::ZERO)` is treated the same
    /// as `None`, since a threshold of zero would fire on every actor at
    /// every instant.
    ///
    /// Every callback is timed: `pre_start`, `on_started`, `handle`,
    /// `pre_stop`, `on_child_stopped`, `on_stopped`, `on_idle` and an inline
    /// signal map (the map of an exit that rides the priority lane). An actor
    /// stuck in its initialization or inside its stop is reported too, while
    /// its [`actor_status`](Self::actor_status) reads `Initializing`, `Running`
    /// (inside `pre_stop`) or `Stopping`.
    ///
    /// The watchdog times turns only while a threshold is set: while it is `None`,
    /// [`turn_elapsed`](Self::turn_elapsed) returns `None` for every actor
    /// and [`stuck_actors`](Self::stuck_actors) is empty. Setting a
    /// threshold adds one monotonic clock read per callback of every
    /// actor in this system, for as long as the threshold stays set. A turn
    /// already in flight at the moment a threshold is first set is not
    /// timed; the next turn that actor starts is.
    ///
    /// This is observation only: it never interrupts a turn, never raises a
    /// stop, and never changes how a stop resolves.
    /// [`actor_status`](Self::actor_status) still reports the status the
    /// actor holds ([`ActorStatus::Initializing`], [`ActorStatus::Running`] or
    /// [`ActorStatus::Stopping`]) for a wedged actor - `turn_elapsed` and
    /// `stuck_actors`, not `actor_status`, are how to tell. The closest
    /// Erlang/OTP mechanism is `erlang:system_monitor/2` with
    /// `{long_schedule, Time}` (erlang.org/docs/28), which reports a process
    /// that runs at least `Time` wall-clock milliseconds without being
    /// scheduled out, as a message to its monitor process; this watchdog times
    /// each callback, awaits included, and is read on demand through
    /// `turn_elapsed` and `stuck_actors`. Set the threshold well above normal
    /// turn lengths to avoid false positives.
    ///
    /// With the `tracing` feature, setting a threshold also (re)starts the
    /// background task that logs actors over it: it runs on whichever
    /// runtime was current when it was started, provided that runtime has the
    /// Tokio time driver (a call made on one without it starts nothing; with
    /// `panic = "abort"` the task's first timer ends the process instead),
    /// exits when that runtime is dropped out from under it, and is started
    /// again, when a threshold is in effect, by the next call to this method
    /// or the next actor registration made with a runtime current.
    ///
    /// Needs the `telemetry` feature, which is on by default.
    pub fn set_turn_warning_threshold(&self, threshold: Option<Duration>) {
        let nanos = threshold_to_nanos(threshold);
        self.turn_warning_threshold_nanos
            .store(nanos, Ordering::Relaxed);
        // The watchdog bit stays on while the system monitor's `long_schedule`
        // wants turns timed, whatever this threshold says.
        crate::telemetry::monitor::refresh_watchdog(
            &self.telemetry.0,
            &self.turn_warning_threshold_nanos,
        );
        // With no runtime current nothing is spawned yet; the next
        // `register_actor` call (which always has one) tries again.
        self.ensure_sweep();
    }

    /// Returns this system's current stuck-turn watchdog threshold, as set
    /// by [`set_turn_warning_threshold`](Self::set_turn_warning_threshold)
    /// or [`SystemConfig::turn_warning_threshold`]. `None` means the
    /// watchdog is off. Observation only - see
    /// [`set_turn_warning_threshold`](Self::set_turn_warning_threshold).
    ///
    /// Needs the `telemetry` feature, which is on by default.
    pub fn turn_warning_threshold(&self) -> Option<Duration> {
        nanos_to_threshold(self.turn_warning_threshold_nanos.load(Ordering::Relaxed))
    }

    /// Makes `monitor` the actor that is told about long turns and long
    /// mailboxes, as Erlang/OTP's `erlang:system_monitor/2` does for its
    /// monitor process, and returns the settings it replaces. A system has
    /// one monitor. An empty `config` clears it.
    ///
    /// The monitor is told through `map`, which turns each report into a
    /// message of the monitor's own type; the message is offered to the
    /// monitor's mailbox without waiting. A message the full mailbox refuses
    /// is counted in the next
    /// [`SubscriptionNotice::Overflow`](crate::telemetry::SubscriptionNotice::Overflow)
    /// notice the monitor is given, and its report is offered again at the
    /// next look while the turn still runs or the mailbox is still long,
    /// until the monitor takes it; a report the monitor took is not offered
    /// again, and a mailbox whose long report the monitor never took is not
    /// reported short to it. A mailbox already long at the monitor's first
    /// look is reported to it neither long nor short; its next crossing is.
    /// `config` names what the monitor is told about:
    /// [`TurnStuck`](crate::telemetry::EventKind::TurnStuck) for a turn that
    /// has run longer than `long_schedule`, or longer than the system's turn
    /// warning threshold when that is smaller (the event carries the threshold
    /// it passed), and
    /// [`MailboxBacklog`](crate::telemetry::EventKind::MailboxBacklog) for a
    /// mailbox that reached `enable` messages and again when it fell to
    /// `disable`. Both are sampled by a task of the system, so a report comes
    /// some time after the crossing and a mailbox that grew and shrank between
    /// two looks is not reported; a `map` that panics ends the monitor after
    /// one more call with a [`SubscriptionNotice::MapPanicked`](crate::telemetry::SubscriptionNotice::MapPanicked)
    /// notice. The monitor's own turns and mailbox are never reported to it.
    /// Setting a `long_schedule` makes the system time turns, as the turn
    /// warning threshold does, and leaves that threshold and
    /// [`stuck_actors`](Self::stuck_actors) as they were.
    ///
    /// The monitor is cleared when its actor ends or when it is replaced.
    ///
    /// # Errors
    /// - [`MonitorError::ZeroEnable`] and [`MonitorError::DisableNotBelowEnable`]
    ///   for a queue length that cannot work;
    /// - [`MonitorError::MonitorEnded`] when `monitor` has ended or is not an
    ///   actor of this system;
    /// - [`MonitorError::NoTimeDriver`] when no runtime with the time driver
    ///   is current, since the sampling sleeps on it.
    ///
    /// A call that fails leaves the monitor the system had.
    ///
    /// Needs the `telemetry` feature, which is on by default.
    pub fn system_monitor<A: Actor>(
        &self,
        monitor: &ActorHandle<A>,
        map: impl FnMut(Delivery) -> A::Message + Send + 'static,
        config: MonitorConfig,
    ) -> Result<Option<MonitorSettings>, MonitorError> {
        use crate::telemetry::monitor as mon;

        mon::validate(&config)?;
        let config = mon::normalized(config);
        if mon::is_empty(&config) {
            return Ok(self.clear_system_monitor());
        }
        let ended = {
            let status = monitor.status_rx();
            let reason = status.borrow().reason.is_some();
            reason || status.status() == crate::types::ActorStatus::Stopped
        };
        let identity = if ended {
            None
        } else {
            self.resolve_identity(monitor.stop_lane_ref().abort_record(), monitor.id())
        };
        let Some(identity) = identity else {
            return Err(MonitorError::MonitorEnded);
        };
        let rt = match Handle::try_current() {
            Ok(rt) if crate::time_probe::runtime_has_timers(&rt) => rt,
            _ => return Err(MonitorError::NoTimeDriver),
        };

        let mode = self.telemetry_mode();
        let sub = crate::telemetry::subscription::next_id(mode);
        let (inbox, rx) = mon::MonitorInbox::new(identity.actor().clone());
        let cancel = tokio_util::sync::CancellationToken::new();
        // Built before the task is spawned and moved into it, so the monitor
        // is cleared whether the task runs to its end, is aborted, or is
        // dropped unpolled with its runtime.
        let guard =
            mon::MonitorGuard::new(mode.clone(), sub, self.turn_warning_threshold_nanos.clone());
        let previous = mon::install(
            mode,
            MonitorSettings {
                monitor: identity.actor().clone(),
                config,
            },
            sub,
            inbox.clone(),
            cancel.clone(),
            &self.turn_warning_threshold_nanos,
        );
        // The sweep offers the monitor's reports; it starts here when the
        // monitor is the first thing that wants it.
        self.ensure_sweep();
        let handle = monitor.clone();
        // The monitor's record goes with its id: the handle this task holds
        // keeps that record alive as long, and the helper's end is then
        // counted among the monitor's own events.
        drop(self.spawn_helper(
            &rt,
            HelperKind::Monitor,
            Some(crate::helper::HelperOwner::new(
                monitor.id().clone(),
                Some(monitor.stop_lane_ref().abort_record().clone()),
            )),
            |end| end.observe(mon::forward(guard, rx, inbox, map, handle, cancel)),
        ));
        Ok(previous)
    }

    /// The settings of the system's monitor, if it has one.
    ///
    /// Needs the `telemetry` feature, which is on by default.
    pub fn system_monitor_settings(&self) -> Option<MonitorSettings> {
        crate::telemetry::monitor::settings(self.telemetry_mode())
    }

    /// Clears the system's monitor and returns its settings.
    ///
    /// Needs the `telemetry` feature, which is on by default.
    pub fn clear_system_monitor(&self) -> Option<MonitorSettings> {
        crate::telemetry::monitor::clear(self.telemetry_mode(), &self.turn_warning_threshold_nanos)
    }

    /// Turns turn tracking on or off. While it is on, the system counts the
    /// turns of its actors and flags the turn each one is inside, which is
    /// what [`is_quiescent`](Self::is_quiescent) and the `in_turn` field of a
    /// [`snapshot`](Self::snapshot) read. Off by default. Switch it on before
    /// the turns, restart attempts and child deaths it should see: a turn
    /// already in flight when it is switched on is not flagged, and an
    /// attempt or a death that began earlier is not counted.
    ///
    /// Needs the `telemetry` feature, which is on by default.
    pub fn set_turn_tracking(&self, on: bool) {
        self.telemetry
            .0
            .set_ctl(crate::telemetry::CTL_TRACK_TURNS, on);
        self.telemetry.0.engage_pass();
    }

    /// Whether turn tracking is on (see
    /// [`set_turn_tracking`](Self::set_turn_tracking)).
    ///
    /// Needs the `telemetry` feature, which is on by default.
    pub fn turn_tracking(&self) -> bool {
        self.telemetry.0.word() & crate::telemetry::CTL_TRACK_TURNS != 0
    }

    /// Turns wait-cycle detection on or off. While it is on, a send made from
    /// inside an actor's hook that has to wait on another actor is recorded
    /// for as long as it waits, and a cycle of such waits raises one
    /// [`WaitCycle`](crate::telemetry::EventKind::WaitCycle) event. Detection
    /// reports and changes no send. Off by default.
    ///
    /// What is recorded: `send`, `send_priority`, `notify` and
    /// `notify_priority` made from a hook, while they wait for room in the
    /// target's mailbox or priority lane, and while `send` and `send_priority`
    /// wait for the reply. What is not recorded: sends with a deadline
    /// (`send_timeout` and `send_priority_timeout`). A cycle that passes
    /// through such a wait is not reported.
    ///
    /// What detection sees is a send future polled inside a hook: the switch
    /// is read when the send begins, its wait is recorded when the send first
    /// returns pending and ends when the future completes or drops, so poll a
    /// send where you await it (a send future
    /// moved to another task keeps describing the hook it first waited in
    /// until it is dropped). A hook that was already running when the switch
    /// was turned on records nothing for the rest of that turn. Cycles are
    /// searched within one system: waits between actors of different systems
    /// are not joined into a cycle.
    ///
    /// Needs the `telemetry` feature, which is on by default.
    pub fn set_wait_cycle_detection(&self, on: bool) {
        self.telemetry.0.set_ctl(crate::telemetry::CTL_DEADLOCK, on);
        self.telemetry.0.engage_pass();
    }

    /// Whether wait-cycle detection is on (see
    /// [`set_wait_cycle_detection`](Self::set_wait_cycle_detection)).
    ///
    /// Needs the `telemetry` feature, which is on by default.
    pub fn wait_cycle_detection(&self) -> bool {
        self.telemetry.0.word() & crate::telemetry::CTL_DEADLOCK != 0
    }

    /// A read-only record of every actor registered in the system, in the
    /// order they were registered. It asks no actor anything, so a stuck actor
    /// does not delay it; it takes no lock a running actor holds across a
    /// turn and runs no user code. An actor whose end is final is not listed.
    ///
    /// The records are read one after another, each at its own instant, so
    /// the snapshot is not atomic across actors: a queue length and a status
    /// of two actors may belong to different moments. `in_turn` is filled
    /// while [`turn tracking`](Self::set_turn_tracking) is on, a turn warning
    /// threshold is set or the system monitor watches turns, and `waiting_on`
    /// while
    /// [`wait-cycle detection`](Self::set_wait_cycle_detection) is on.
    ///
    /// The coupling counts of a row ([`monitors`](crate::telemetry::ActorRecord::monitors),
    /// [`monitored_by`](crate::telemetry::ActorRecord::monitored_by),
    /// [`links`](crate::telemetry::ActorRecord::links),
    /// [`pg_groups`](crate::telemetry::ActorRecord::pg_groups) and
    /// [`group_monitors`](crate::telemetry::ActorRecord::group_monitors)) are
    /// read from the incarnation's shared block, where the actor records the
    /// lengths of its own monitor, link and group-monitor tables when they
    /// change and where its watchers and its groups keep the other two
    /// counts: they are the values of the last change, as [`get_status`](crate::actor::handle::ActorHandle::get_status)
    /// reports them, without asking the actor.
    ///
    /// Needs the `telemetry` feature, which is on by default.
    pub fn snapshot(&self) -> Vec<crate::telemetry::ActorRecord> {
        use crate::telemetry::{
            ActorRecord, ActorRef, CTL_DEADLOCK, CTL_TRACK_TURNS, CTL_WATCHDOG,
        };

        let mode = self.telemetry_mode();
        let word = mode.word();
        let timed = word & (CTL_WATCHDOG | CTL_TRACK_TURNS) != 0;

        // One walk of the registry. A shard guard is held only while its
        // entries are read, and what is read under it is atomics, the
        // status plane's leaf cell and the mailbox's counters: no user code,
        // no await, no other registry lock.
        let mut seen: Vec<Seen> = Vec::new();
        for entry in &self.by_id {
            let record = entry.stop_lane.abort_record();
            let ended = {
                let value = entry.status_rx.borrow();
                value.reason.is_some()
            };
            seen.push(Seen {
                id: entry.key().0.clone(),
                named: entry.named,
                seq: entry.registration_seq,
                key: record.key(),
                token: record.token_addr(),
                parent: record.parent_token_addr(),
                clock: entry.turn_clock.id(),
                live: (!ended).then(|| LiveRead {
                    status: entry.status_rx.status(),
                    mail: entry.handle.mail_view(),
                    children: record.children(),
                    in_turn: if timed {
                        entry.turn_clock.snapshot().map(|(_, elapsed)| elapsed)
                    } else {
                        None
                    },
                    suspended: record.sys_bits() & crate::actor::incarnation::SYS_SUSPENDED != 0,
                    coupling: crate::telemetry::CouplingInfo::of(record),
                }),
            });
        }
        #[cfg(test)]
        {
            let hook = self.after_snapshot_walk.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        // Everything below works on what was copied out: no guard is held.
        seen.sort_by_key(|row| row.seq);
        let by_token: HashMap<usize, usize> = seen
            .iter()
            .enumerate()
            .filter_map(|(at, row)| row.token.map(|token| (token, at)))
            .collect();
        let by_clock: HashMap<u64, usize> = seen
            .iter()
            .enumerate()
            .map(|(at, row)| (row.clock, at))
            .collect();
        let mut refs: Vec<Option<ActorRef>> = vec![None; seen.len()];
        let mut records = Vec::new();
        for at in 0..seen.len() {
            let Some(live) = &seen[at].live else {
                continue;
            };
            // The table is read by a record address that may have been reused
            // since the walk: an identity is kept only when it is the one of
            // this entry's registration, and the row is built from the
            // walk's own copy otherwise.
            let identity = mode
                .find(seen[at].key)
                .filter(|identity| identity.incarnation() == seen[at].seq);
            let actor = match &identity {
                Some(identity) => identity.actor().clone(),
                None => ref_of(&seen, &by_token, &mut refs, at),
            };
            let waiting_on = if word & CTL_DEADLOCK != 0 {
                mode.wait
                    .target_of(seen[at].clock)
                    .and_then(|target| by_clock.get(&target).copied())
                    .map(|target| ref_of(&seen, &by_token, &mut refs, target))
            } else {
                None
            };
            records.push(ActorRecord {
                actor,
                name: seen[at].named.then(|| seen[at].id.as_str().to_string()),
                status: live.status,
                type_name: live.mail.type_name,
                label: identity
                    .as_ref()
                    .and_then(|identity| identity.label())
                    .map(|label| label.to_string()),
                mailbox_len: live.mail.mailbox_len,
                mailbox_capacity: live.mail.mailbox_capacity,
                priority_len: live.mail.priority_len,
                priority_capacity: live.mail.priority_capacity,
                child_count: live.children,
                in_turn: live.in_turn,
                waiting_on,
                suspended: live.suspended,
                coupling: live.coupling,
            });
        }
        records
    }

    /// Whether the system is quiescent now: no actor is inside a turn, none
    /// has a queued message or is still initializing, and no restart attempt
    /// is in flight. `None` unless [turn tracking](Self::set_turn_tracking) is
    /// on.
    ///
    /// The registry is read twice. The answer is `true` only when both reads
    /// found the same actors with the same turn counts and every actor idle
    /// with empty lanes, so a turn that began and ended between the reads
    /// makes it `false`. It says the system is quiet at the instant of the
    /// call, not that it will stay quiet: an item a stream forwarder holds, a
    /// send that has not begun and a timer that is not yet due are not seen,
    /// and neither is a message taken from a mailbox and not yet inside its
    /// turn (a window of a few instructions), or a child death before the
    /// child's end is recorded.
    ///
    /// A child's death counts from the child's end until the supervisor has
    /// decided on it, and a restart attempt from its start until the
    /// supervisor takes its outcome, so the gap between them is not quiet. A
    /// group restart counts from the supervisor's decision on the group until
    /// the group's last member has its fate, so the span between its stops
    /// and its starts is not quiet either.
    /// Turn tracking must be on before the turns, attempts and deaths the
    /// answer should cover. The answer is `None` when the tracking switch
    /// changed during the call, an off and on again included.
    ///
    /// Needs the `telemetry` feature, which is on by default.
    pub fn is_quiescent(&self) -> Option<bool> {
        use crate::telemetry::CTL_TRACK_TURNS;

        let mode = self.telemetry_mode();
        if mode.word() & CTL_TRACK_TURNS == 0 {
            return None;
        }
        let switches = mode.ctl_changes();
        let first = self.quiet_pass();
        #[cfg(test)]
        {
            let hook = self.between_quiet_passes.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let second = self.quiet_pass();
        // A switch turned off during the call leaves no counters to compare,
        // and one that moved at all (off and on again included) means turns
        // may have run uncounted between the reads.
        if mode.word() & CTL_TRACK_TURNS == 0 || mode.ctl_changes() != switches {
            return None;
        }
        Some(first.idle && first == second)
    }

    /// Runs `hook` once, in the sweep task, after it decided to end and
    /// before it clears its latch.
    #[cfg(test)]
    pub(crate) fn run_before_sweep_latch_clear(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.before_sweep_latch_clear.lock().unwrap() = Some(Arc::new(hook));
    }

    /// Runs `hook` once, inside the next snapshot, after the registry walk and
    /// before the rows are built.
    #[cfg(test)]
    pub(crate) fn run_after_snapshot_walk(&self, hook: impl FnOnce() + Send + 'static) {
        *self.after_snapshot_walk.lock().unwrap() = Some(Box::new(hook));
    }

    /// Runs `hook` once, inside the next [`is_quiescent`](Self::is_quiescent),
    /// between its two reads.
    #[cfg(test)]
    pub(crate) fn run_between_quiet_passes(&self, hook: impl FnOnce() + Send + 'static) {
        *self.between_quiet_passes.lock().unwrap() = Some(Box::new(hook));
    }

    /// One read of what quiescence compares: for every actor whose end is not
    /// final, its registration number, turn count, in-turn flag and lane
    /// lengths, and the restart attempts in flight across them.
    fn quiet_pass(&self) -> QuietPass {
        let mut rows = Vec::new();
        let mut keys = Vec::new();
        let mut idle = true;
        for entry in &self.by_id {
            if entry.status_rx.borrow().reason.is_some() {
                continue;
            }
            let mail = entry.handle.mail_view();
            let in_turn = entry.turn_clock.token() != 0;
            let starting = entry.status_rx.status() == ActorStatus::Initializing;
            idle &= !in_turn && !starting && mail.mailbox_len == 0 && mail.priority_len == 0;
            rows.push((
                entry.registration_seq,
                entry.turn_clock.turns(),
                in_turn,
                mail.mailbox_len,
                mail.priority_len,
            ));
            keys.push((entry.stop_lane.abort_record().key(), entry.registration_seq));
        }
        rows.sort_unstable();
        // Read after the walk, outside every registry guard. A record address
        // may have been reused since the walk, so an identity counts only
        // when it is the one of the registration the walk read.
        let mode = self.telemetry_mode();
        let attempts: u64 = keys
            .into_iter()
            .filter_map(|(key, seq)| {
                mode.find(key)
                    .filter(|identity| identity.incarnation() == seq)
            })
            .map(|identity| {
                u64::from(identity.restarts_in_flight()) + u64::from(identity.deaths_owed())
            })
            .sum();
        let in_flight = attempts;
        QuietPass {
            rows,
            in_flight,
            idle: idle && in_flight == 0,
        }
    }

    /// Attaches a synchronous handler for the telemetry events of `kinds`
    /// under `id`.
    ///
    /// Returns `false`, and attaches nothing, when a handler is already
    /// attached under `id`. The handler takes effect from the next event: an
    /// event being delivered when the call is made goes to the handlers it
    /// started with.
    ///
    /// An actor carries nothing for telemetry until a handler or a
    /// subscription takes events. An actor spawned while one does has its
    /// events raised with its incarnation number and its ancestry. An actor
    /// spawned before that is identified from the system's registry, with
    /// every other actor the registry holds, by the call that attached the
    /// first handler or subscription: the events about it carry its incarnation
    /// number and its ancestry too, and a supervisor is named with its id and
    /// its incarnation number. Such an actor has no recorded birth time, so
    /// its end event reports no lifetime.
    ///
    /// The [`TurnFinished`](crate::telemetry::EventKind::TurnFinished) event of
    /// a `handle` turn that went on, the
    /// [`RunFinished`](crate::telemetry::EventKind::RunFinished) event and the
    /// [`Trace`](crate::telemetry::EventKind::Trace) events of the actor's
    /// turns run the handler on the actor's own task, between turns, while the
    /// helper holds no event about that actor, so a handler that blocks holds that actor's
    /// next take and whatever waits on its task: a stop request is read only
    /// at the turn boundary, and the stop clock counts the handler because the
    /// turn has already been timed. While the helper holds an event about the
    /// actor (its start events after a spawn, the event of a stop request, a
    /// sender wait, a timer tick or a child's death, or a backlog the helper
    /// has not yet delivered), such an event is queued behind it and runs on
    /// the helper, so it never reaches a handler ahead of an event about the
    /// same actor that was raised before it. The turn events of every other
    /// hook, those of a turn after the actor's loop ended or of a turn that
    /// stopped its own actor, and every other kind, the timer tick and the
    /// sender wait included, run the handler on a helper task of the system,
    /// never on the task or thread that raised the event. The one exception is
    /// [`HandlerDetached`](crate::telemetry::EventKind::HandlerDetached), which
    /// reaches the remaining handlers on the thread that ran the handler that
    /// panicked.
    /// [`KindSet::all`](crate::telemetry::KindSet::all) includes the hot
    /// kinds. The handlers attached for one event run one after another, each
    /// call ending before the next begins, and the crate holds no lock while
    /// one runs; a turn or run event delivered on its actor's own task can run
    /// while the helper delivers an event about another actor, so handlers
    /// guard the state they share. On the helper, events reach a handler in
    /// the order they were raised. The helper runs on
    /// the runtime that started it, so a handler that blocks holds a worker
    /// thread of that runtime, delays the events behind it, and on a
    /// `current_thread` runtime stalls every task on it, the actors and the
    /// stop steps included: handlers must not block. The helper's queue holds
    /// 4096 events; what finds it full is dropped and counted, what finds the
    /// system gone is counted too, and so is what is raised inside a handler of
    /// this system. The count is reported to the handlers that
    /// take [`Dropped`](crate::telemetry::EventKind::Dropped) events, and to
    /// each subscription that would have taken the dropped event as
    /// `missed_before` of its next delivery, only while a handler or
    /// subscription still exists to receive it. Events still queued when the
    /// system is dropped are counted as dropped too. `shutdown` does not wait
    /// for delivery; events raised by the last actors reach handlers only while
    /// the helper's runtime is driven. Hand slow work to a
    /// bounded channel with `try_send`, sending the owned copy
    /// [`to_static`](crate::telemetry::TelemetryEvent::to_static) makes, and
    /// count what is dropped:
    ///
    /// ```
    /// use std::sync::atomic::{AtomicU64, Ordering};
    /// use std::sync::Arc;
    /// use tokio_actors::telemetry::{KindSet, TelemetryEvent};
    /// use tokio_actors::ActorSystem;
    ///
    /// let system = ActorSystem::create("exporter-example").unwrap();
    /// let (tx, mut rx) = tokio::sync::mpsc::channel::<TelemetryEvent<'static>>(1024);
    /// let lost = Arc::new(AtomicU64::new(0));
    /// let counted = lost.clone();
    /// system.attach_telemetry("export", KindSet::all(), move |event| {
    ///     if tx.try_send(event.to_static()).is_err() {
    ///         counted.fetch_add(1, Ordering::Relaxed);
    ///     }
    /// });
    /// // A task of the user's reads `rx` and does the slow work there.
    /// # drop(rx.try_recv());
    /// ```
    ///
    /// The helper stays on the runtime that started it. A handler that holds an
    /// `Arc<ActorSystem>` keeps the system, and with it the helper, alive:
    /// detach it first.
    ///
    /// A handler that panics is detached, the panic is contained, the
    /// remaining handlers still run, and one
    /// [`HandlerDetached`](crate::telemetry::EventKind::HandlerDetached) event
    /// goes to the handlers that remain; a handler that attaches another from
    /// that event and panics in turn starts the chain again, one handler per
    /// step. Under `panic = "abort"` there is no containment: a panic of a
    /// handler ends the process, as a panic of any hook does.
    ///
    /// A handler may call `attach_telemetry` and
    /// [`detach_telemetry`](Self::detach_telemetry). An event of this system
    /// raised from inside one of its handlers is skipped and counted as
    /// dropped like any other: it reaches no handler and no subscription, and
    /// the subscriptions that would have taken it read it as `missed_before`.
    /// A handler of this system that raises events of another system does not
    /// make that system skip them.
    ///
    /// Needs the `telemetry` feature, which is on by default.
    pub fn attach_telemetry(
        &self,
        id: &str,
        kinds: crate::telemetry::KindSet,
        handler: impl Fn(&crate::telemetry::TelemetryEvent<'_>) + Send + Sync + 'static,
    ) -> bool {
        let attached =
            crate::telemetry::handlers::attach(&self.telemetry.0, id, kinds, Arc::new(handler));
        // The registry mutex of the sinks is released: an attach that made
        // the system resolve identities records every registered actor's
        // here, on the caller's thread, not on the first event's.
        self.telemetry.0.engage_pass();
        crate::telemetry::subscription::start_sweep_for(&self.telemetry.0, kinds);
        attached
    }

    /// Detaches the handler attached under `id`; `false` when there is none.
    ///
    /// No event delivered after the call reaches the handler, and the state it
    /// captured is dropped once the last call in flight ends; a panic of that
    /// drop is contained. The call does not wait for a handler that is
    /// running.
    ///
    /// An actor that delivered an event on its own task (a turn event, the run
    /// event or a trace entry) keeps a copy of the whole handler list, read
    /// again when it next delivers such an event and dropped at its end. Until then the state a detached
    /// handler captured, of any kind, stays alive, so do not rely on a detach
    /// to close a channel or release a resource the handler captured while
    /// actors are running.
    ///
    /// Needs the `telemetry` feature, which is on by default.
    pub fn detach_telemetry(&self, id: &str) -> bool {
        crate::telemetry::handlers::detach(&self.telemetry.0, id)
    }

    /// How long the actor registered under `id` has been inside its current
    /// callback (`pre_start`, `on_started`, `handle`, `pre_stop`,
    /// `on_child_stopped`, `on_stopped`, `on_idle` or an inline signal map).
    /// `None` if it is between
    /// callbacks, not registered, or this system's threshold is currently
    /// `None` - the watchdog
    /// times turns only while a threshold is set, so this always returns `None` for
    /// every actor while the watchdog is off, regardless of whether a turn
    /// happens to be in flight.
    ///
    /// Observation only - see
    /// [`set_turn_warning_threshold`](Self::set_turn_warning_threshold).
    ///
    /// Needs the `telemetry` feature, which is on by default.
    pub fn turn_elapsed(&self, id: &ActorId) -> Option<Duration> {
        // Checked here, not just inside `TurnClock::begin`: a turn that was
        // already in flight when the threshold was switched off mid-turn
        // keeps a nonzero token (and would keep answering `Some` from
        // `TurnClock::elapsed` alone) until it ends - this system's current
        // threshold, not the actor's own recorded token, is what decides
        // whether that turn is still reported as timed.
        if self.turn_warning_threshold_nanos.load(Ordering::Relaxed) == 0 {
            return None;
        }
        self.by_id.get(id).and_then(|e| e.turn_clock.elapsed())
    }

    /// Actors whose current callback (`pre_start`, `on_started`, `handle`,
    /// `pre_stop`, `on_child_stopped`, `on_stopped`, `on_idle` or an inline
    /// signal map) has run longer than the
    /// configured threshold, longest first. Empty when no threshold is set.
    ///
    /// Observation only - see
    /// [`set_turn_warning_threshold`](Self::set_turn_warning_threshold).
    ///
    /// Needs the `telemetry` feature, which is on by default.
    pub fn stuck_actors(&self) -> Vec<(ActorId, Duration)> {
        let threshold_nanos = self.turn_warning_threshold_nanos.load(Ordering::Relaxed);
        if threshold_nanos == 0 {
            return Vec::new();
        }
        let threshold = Duration::from_nanos(threshold_nanos);

        // Plain iteration, no `.await` anywhere inside: never hold a
        // `DashMap` guard across an await point. `snapshot()` reads the
        // token and the elapsed time from a single atomic load, so a turn
        // ending and a new one starting mid-iteration can never be reported
        // as a hybrid of the two.
        let mut stuck: Vec<(ActorId, Duration)> = self
            .by_id
            .iter()
            .filter_map(|e| {
                let (_, elapsed) = e.turn_clock.snapshot()?;
                (elapsed > threshold).then(|| (e.key().0.clone(), elapsed))
            })
            .collect();
        stuck.sort_by_key(|(_, elapsed)| std::cmp::Reverse(*elapsed));
        stuck
    }
}

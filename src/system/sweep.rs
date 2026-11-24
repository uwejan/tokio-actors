//! The sweep: the helper task that times turns, reports stuck turns and samples mailboxes for the system monitor.

#[cfg(feature = "tracing")]
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::runtime::Handle;

use super::{ActorSystem, TIMERLESS_MEMO_LIMIT};
use crate::actor::incarnation::IncarnationRef;
use crate::helper::HelperKind;
use crate::telemetry::TelemetryEvent;
use crate::types::ActorId;

impl ActorSystem {
    /// Starts the sweep on the runtime current now, when something wants it
    /// and none is alive. A call with no runtime current starts nothing; the
    /// next call with one retries.
    #[inline]
    pub(crate) fn ensure_sweep(&self) {
        if self.sweep_wanted() {
            self.ensure_sweep_wanted();
        }
    }

    /// The part of [`ensure_sweep`](Self::ensure_sweep) that runs only when
    /// something wants the sweep, kept out of line so a spawn that nothing
    /// watches tests the want and goes on.
    #[inline(never)]
    fn ensure_sweep_wanted(&self) {
        // The want was published before this call and the latch is read
        // below; a sweep that is ending clears its latch and then reads the
        // want. The fence orders both pairs, so at least one side sees the
        // other and a want is never left with no sweep.
        std::sync::atomic::fence(Ordering::SeqCst);
        if let Ok(rt) = Handle::try_current() {
            self.ensure_sweep_spawned_with(&rt);
        }
    }

    /// Whether anything wants the sweep to run: a sink that takes
    /// [`TurnStuck`](crate::telemetry::EventKind::TurnStuck) or
    /// [`MailboxBacklog`](crate::telemetry::EventKind::MailboxBacklog), an
    /// installed system monitor (it arms no kind: its thresholds are in the
    /// mode), or, with the `tracing` feature, a turn warning threshold. Three
    /// relaxed loads when nothing asks and the feature is off.
    #[inline]
    pub(crate) fn sweep_wanted(&self) -> bool {
        use crate::telemetry::EventKind;
        let mode = self.telemetry_mode();
        let sampled = EventKind::TurnStuck.bit() | EventKind::MailboxBacklog.bit();
        mode.word() & sampled != 0
            || mode.monitor_long_schedule_nanos() != 0
            || mode.monitor_queue() != 0
            || (cfg!(feature = "tracing")
                && self.turn_warning_threshold_nanos.load(Ordering::Relaxed) != 0)
    }

    /// Spawns the turn-watchdog sweep task for this system, unless one is
    /// already alive (`sweep_spawned` is already `true`). A no-op whenever a
    /// sweep is currently running; once that sweep's task ends - see
    /// [`turn_watchdog_sweep`]'s drop guard - the next call here spawns a
    /// fresh one.
    ///
    /// The sweep sleeps on `rt`, so a runtime without the time driver is
    /// never used for it: the call does nothing there, and the next call on a
    /// runtime with the driver starts it. A runtime found without the driver
    /// is remembered by this system, so a spawn there does not probe it again.
    pub(super) fn ensure_sweep_spawned_with(&self, rt: &Handle) {
        // A sweep that is alive needs no probe. A runtime known to have the
        // driver goes on; one this system already found without it returns;
        // any other is probed, outside every lock, and a negative answer is
        // remembered.
        if self.sweep_spawned.load(Ordering::Acquire) {
            return;
        }
        if !crate::time_probe::runtime_known_to_have_timers(rt) {
            let id = rt.id();
            if self
                .timerless_runtimes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&id)
            {
                return;
            }
            if !crate::time_probe::probe_runtime_for_timers(rt) {
                let mut memo = self
                    .timerless_runtimes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if memo.len() >= TIMERLESS_MEMO_LIMIT {
                    memo.clear();
                }
                memo.insert(id);
                return;
            }
        }
        if self
            .sweep_spawned
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        // Built here and moved into the spawned future as a plain captured
        // value, not constructed as the first line of the future's own
        // body: a future's captures drop along with it even if it is never
        // polled, so this way the latch clears whether the task runs to
        // completion, is aborted mid-flight, or is dropped unpolled because
        // its runtime never gets around to it before shutting down.
        let guard = SweepLatchGuard(self.sweep_spawned.clone());
        let system = self.me.clone();
        drop(self.helpers.spawn(rt, HelperKind::Sweep, None, |end| {
            end.observe(async move {
                turn_watchdog_sweep(system.clone()).await;
                // The sweep decided to end. A want that arrived since its last
                // look found the latch set and started nothing, so the latch
                // clears first and the want is read again after it: either
                // that caller or this read starts the next sweep.
                #[cfg(test)]
                if let Some(live) = system.upgrade() {
                    let hook = live.before_sweep_latch_clear.lock().unwrap().take();
                    if let Some(hook) = hook {
                        hook();
                    }
                }
                drop(guard);
                std::sync::atomic::fence(Ordering::SeqCst);
                if let Some(live) = system.upgrade() {
                    live.ensure_sweep();
                }
            })
        }));
    }
}

// ---------------------------------------------------------------------------
// Stuck-turn watchdog: threshold encoding and the sweep task
// ---------------------------------------------------------------------------

/// Decodes a `turn_warning_threshold_nanos` value back into the `Option`
/// surface: `0` is `None`.
pub(super) fn nanos_to_threshold(nanos: u64) -> Option<Duration> {
    (nanos != 0).then(|| Duration::from_nanos(nanos))
}

/// Clears a sweep task's shared spawn latch back to `false` when dropped:
/// whether the task it travels with returns normally, is aborted mid-flight,
/// or is dropped without ever being polled at all, because the runtime it
/// was spawned on shuts down first. Without this, a sweep task that never
/// clears the latch would leave `sweep_spawned` stuck at `true` forever, and
/// no later runtime could ever spawn a replacement for it.
struct SweepLatchGuard(Arc<AtomicBool>);

impl Drop for SweepLatchGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// How long the turn-watchdog sweep sleeps between checks for a given turn
/// warning threshold: a quarter of the threshold, never less than 50 ms and
/// never more than 1 s. A zero threshold gives 50 ms and `Duration::MAX`
/// gives 1 s; the division cannot overflow.
pub(super) fn sweep_tick(threshold: Duration) -> Duration {
    (threshold / 4).clamp(Duration::from_millis(50), Duration::from_secs(1))
}

/// An actor whose turn ran past a threshold, as the sweep saw it. It holds
/// the actor's record itself, not the record's address: the record names the
/// actor in the telemetry table for as long as it lives, so an actor that
/// ends while the sweep writes the lines before its own is still named by its
/// own identity and never by a new actor placed at the same address.
pub(super) struct StuckTurn {
    pub(super) id: ActorId,
    seq: u64,
    token: u64,
    elapsed: Duration,
    record: crate::actor::incarnation::IncarnationRef,
}

impl StuckTurn {
    /// The address that names the record in the telemetry table.
    #[cfg(test)]
    pub(super) fn key(&self) -> usize {
        self.record.key()
    }

    /// The identity the telemetry table holds for this actor's record.
    #[cfg(feature = "tracing")]
    pub(super) fn identity(
        &self,
        mode: &crate::telemetry::Mode,
    ) -> Option<Arc<crate::telemetry::Identity>> {
        mode.find(self.record.key())
    }
}

/// The actors whose current turn has run longer than `threshold`.
pub(super) fn stuck_turns(system: &ActorSystem, threshold: Duration) -> Vec<StuckTurn> {
    system
        .by_id
        .iter()
        .filter_map(|e| {
            let (token, elapsed) = e.turn_clock.snapshot()?;
            (elapsed > threshold).then(|| StuckTurn {
                id: e.key().0.clone(),
                seq: e.registration_seq,
                token,
                elapsed,
                record: e.stop_lane.abort_record().clone(),
            })
        })
        .collect()
}

/// Background task, spawned by [`ActorSystem::ensure_sweep_spawned_with`]
/// whenever none is currently alive for this system and something asks for
/// it, that looks at the actors now and then: it reports a turn that has run
/// past the smaller of the monitor's `long_schedule` and the turn warning
/// threshold, whichever of them is set, reports a mailbox that crossed the
/// monitor's lengths, and with the `tracing` feature logs actors over the
/// turn warning threshold. Holds its system only weakly and upgrades it for
/// the length of one look, and exits the moment that upgrade comes back
/// empty - so this task never keeps an otherwise-unreferenced system alive,
/// never sweeps a later system that claimed the name, and never needs a
/// separate shutdown signal. It also exits after two looks in a row that
/// found nothing asking for it. The caller spawns this alongside a
/// [`SweepLatchGuard`] built from the owning system's `sweep_spawned`, so
/// that latch clears on the way out by any exit path, including this task
/// being dropped before it is ever polled, or being dropped outright by its
/// runtime shutting down.
async fn turn_watchdog_sweep(system: Weak<ActorSystem>) {
    let mut state = crate::telemetry::monitor::SweepState::default();
    let mut idle_looks = 0u8;
    loop {
        let Some(live) = system.upgrade() else {
            return;
        };
        if live.sweep_wanted() {
            idle_looks = 0;
        } else {
            idle_looks += 1;
            if idle_looks >= 2 {
                return;
            }
        }
        // The look has no `.await` in it: never hold a `DashMap` guard across
        // one.
        let tick = live.sweep_look(&mut state);
        drop(live);
        tokio::time::sleep(tick).await;
    }
}

impl ActorSystem {
    /// One look of the sweep; returns how long to wait before the next.
    ///
    /// The smallest nonzero threshold among the turn warning threshold and
    /// the monitor's `long_schedule` is the one a turn is reported at (once
    /// per turn), and a quarter of it sets the wait; a monitor of mailboxes
    /// alone counts as a second. Events go to the pump; reports for the
    /// system monitor go to its inbox; no handler and no map runs here.
    fn sweep_look(&self, state: &mut crate::telemetry::monitor::SweepState) -> Duration {
        use crate::telemetry::EventKind;

        let mode = self.telemetry_mode();
        let watchdog = self.turn_warning_threshold_nanos.load(Ordering::Relaxed);
        let long_schedule = mode.monitor_long_schedule_nanos();
        let smallest = [watchdog, long_schedule]
            .into_iter()
            .filter(|nanos| *nanos != 0)
            .min()
            .map(Duration::from_nanos);
        let tick = sweep_tick(smallest.unwrap_or(Duration::from_secs(1)));
        state.pass += 1;
        let inbox = self.monitor_inbox_for(state);

        let turns_inbox = inbox.as_ref().filter(|_| long_schedule != 0);
        let report =
            smallest.is_some() && (mode.armed(EventKind::TurnStuck) || turns_inbox.is_some());
        let warn = cfg!(feature = "tracing") && watchdog != 0;
        match smallest {
            Some(threshold) if report || warn => {
                let turns = stuck_turns(self, threshold);
                if report {
                    self.report_stuck_turns(&turns, threshold, state, turns_inbox.map(|i| &**i));
                } else {
                    state.stuck.clear();
                }
                #[cfg(feature = "tracing")]
                if warn {
                    self.warn_stuck_turns(&turns, Duration::from_nanos(watchdog), state);
                }
            }
            _ => state.stuck.clear(),
        }

        match crate::telemetry::monitor::unpack_queue(mode.monitor_queue()) {
            Some((enable, disable)) => {
                self.report_backlogs(enable, disable, state, inbox.as_deref());
            }
            _ => {
                state.alarms.clear();
                state.leaving.clear();
            }
        }
        tick
    }

    /// The system monitor's inbox for this look, after the helper's answers
    /// since the last look are read into `state`. A monitor that is not the
    /// one the tracked states were told to is owed none of them: they read as
    /// owed nothing, the offers and short reports still out are forgotten,
    /// and the look is that monitor's first.
    fn monitor_inbox_for(
        &self,
        state: &mut crate::telemetry::monitor::SweepState,
    ) -> Option<Arc<crate::telemetry::monitor::MonitorInbox>> {
        use crate::telemetry::monitor::{Offer, Told};

        let current = crate::telemetry::monitor::inbox(self.telemetry_mode());
        let id = current.as_ref().map(|(id, _)| *id);
        state.first_look = state.monitor != id;
        if state.first_look {
            state.monitor = id;
            state.offered.clear();
            state.leaving.clear();
            for stuck in state.stuck.values_mut() {
                stuck.told = Told::Unowed;
            }
            for alarm in state.alarms.values_mut() {
                alarm.told = Told::Unowed;
            }
        }
        let (_, inbox) = current?;
        for (tag, landed) in inbox.take_outcomes() {
            let Some(offer) = state.offered.remove(&tag) else {
                continue;
            };
            let answer = if landed { Told::Landed } else { Told::Refused };
            // An answer counts only for the offer the state still waits on:
            // a state that ended or moved on since has another offer or none.
            match offer {
                Offer::Turn(seq) => {
                    if let Some(stuck) = state.stuck.get_mut(&seq) {
                        if stuck.told == Told::Pending(tag) {
                            stuck.told = answer;
                        }
                    }
                }
                Offer::Entered(seq) => {
                    if let Some(alarm) = state.alarms.get_mut(&seq) {
                        if alarm.told == Told::Pending(tag) {
                            alarm.told = answer;
                        }
                    }
                    // A short report held behind this long one goes out once
                    // the long one landed, and never when it was refused.
                    if state
                        .leaving
                        .get(&seq)
                        .is_some_and(|l| l.after == Some(tag))
                    {
                        if landed {
                            if let Some(leaving) = state.leaving.get_mut(&seq) {
                                leaving.after = None;
                            }
                        } else {
                            state.leaving.remove(&seq);
                        }
                    }
                }
                Offer::Left(seq) => {
                    if state
                        .leaving
                        .get(&seq)
                        .is_some_and(|l| l.told == Told::Pending(tag))
                    {
                        if landed {
                            state.leaving.remove(&seq);
                        } else if let Some(leaving) = state.leaving.get_mut(&seq) {
                            leaving.told = Told::Refused;
                        }
                    }
                }
            }
        }
        inbox.prompt_missed();
        Some(inbox)
    }

    /// Offers `event` to the monitor as a new offer for `what`; the state's
    /// `Told` after the offer.
    fn offer_to_monitor(
        state: &mut crate::telemetry::monitor::SweepState,
        inbox: &crate::telemetry::monitor::MonitorInbox,
        what: crate::telemetry::monitor::Offer,
        event: TelemetryEvent<'static>,
    ) -> crate::telemetry::monitor::Told {
        use crate::telemetry::monitor::Told;
        state.next_tag += 1;
        let tag = state.next_tag;
        if inbox.offer(event, tag) {
            state.offered.insert(tag, what);
            Told::Pending(tag)
        } else {
            Told::Refused
        }
    }

    /// Reports each turn in `turns` that was not reported yet, once, to the
    /// sinks that take the kind; offers it to the monitor's `inbox` until the
    /// monitor's mailbox takes it while the turn lasts; and forgets the turns
    /// that are over.
    fn report_stuck_turns(
        &self,
        turns: &[StuckTurn],
        threshold: Duration,
        state: &mut crate::telemetry::monitor::SweepState,
        inbox: Option<&crate::telemetry::monitor::MonitorInbox>,
    ) {
        use crate::telemetry::monitor::{Offer, Stuck, Told};
        use crate::telemetry::EventKind;

        let mode = self.telemetry_mode();
        let pass = state.pass;
        for turn in turns {
            let known = state
                .stuck
                .get(&turn.seq)
                .filter(|stuck| stuck.token == turn.token);
            let new = known.is_none();
            let told = known.map_or(Told::Unoffered, |stuck| stuck.told);
            let to_sinks = new && mode.armed(EventKind::TurnStuck);
            let to_monitor = inbox.is_some() && matches!(told, Told::Unoffered | Told::Refused);
            let mut told = if inbox.is_some() { told } else { Told::Unowed };
            if to_sinks || to_monitor {
                match crate::telemetry::identity_for(mode, &turn.record, Some(self), Some(&turn.id))
                {
                    Some(identity) => {
                        let event = || TelemetryEvent::TurnStuck {
                            actor: identity.actor().clone(),
                            elapsed: turn.elapsed,
                            threshold,
                        };
                        if to_sinks {
                            mode.push_owned(&identity, EventKind::TurnStuck, event);
                        }
                        if let Some(inbox) = inbox.filter(|_| to_monitor) {
                            told = if inbox.is_monitor(identity.actor()) {
                                Told::Unowed
                            } else {
                                Self::offer_to_monitor(state, inbox, Offer::Turn(turn.seq), event())
                            };
                        }
                    }
                    None => {
                        // The registry has let go of the actor and nothing
                        // recorded it, so no report can name it: counted with
                        // the other drops.
                        mode.pump.count_dropped();
                        told = Told::Unowed;
                    }
                }
            }
            state.stuck.insert(
                turn.seq,
                Stuck {
                    token: turn.token,
                    seen: pass,
                    told,
                },
            );
        }
        state.stuck.retain(|_, stuck| stuck.seen == pass);
    }

    /// Logs each turn in `turns` over `threshold` that was not logged yet.
    #[cfg(feature = "tracing")]
    fn warn_stuck_turns(
        &self,
        turns: &[StuckTurn],
        threshold: Duration,
        state: &mut crate::telemetry::monitor::SweepState,
    ) {
        let mode = self.telemetry_mode();
        let system_name = self.name();
        let mut still_stuck: HashSet<ActorId> = HashSet::new();
        for turn in turns.iter().filter(|turn| turn.elapsed > threshold) {
            let (id, token, elapsed) = (&turn.id, turn.token, turn.elapsed);
            still_stuck.insert(id.clone());
            if state.warned.get(id) != Some(&token) {
                let elapsed_ms = elapsed.as_millis() as u64;
                let threshold_ms = threshold.as_millis() as u64;
                let fields = crate::telemetry::LogFields::of(turn.identity(mode), system_name);
                caught_log!(warn, target: "tokio_actors::system",
                    actor_id = %id,
                    actor_name = fields.name(),
                    incarnation = fields.incarnation(),
                    system = fields.system(),
                    elapsed_ms,
                    threshold_ms,
                    "actor turn exceeds the turn warning threshold"
                );
                state.warned.insert(id.clone(), token);
            }
        }
        state.warned.retain(|id, _| still_stuck.contains(id));
    }

    /// Reports each mailbox that reached `enable` and each reported mailbox
    /// that fell to `disable` or whose actor is gone, once per edge, to the
    /// sinks that take the kind. The monitor's `inbox` is offered the same
    /// edges until its mailbox takes each while it lasts; a short report is
    /// offered only for a long report the monitor took.
    fn report_backlogs(
        &self,
        enable: usize,
        disable: usize,
        state: &mut crate::telemetry::monitor::SweepState,
        inbox: Option<&crate::telemetry::monitor::MonitorInbox>,
    ) {
        use crate::telemetry::monitor::{backlog_edge, Alarm, Leaving, Offer, Told};
        use crate::telemetry::EventKind;

        let mode = self.telemetry_mode();
        let to_sinks = mode.armed(EventKind::MailboxBacklog);
        let pass = state.pass;
        // Collected under the registry guards; the identities are resolved
        // and the events pushed after the last guard is gone.
        let mut entered: Vec<(u64, ActorId, IncarnationRef, usize, usize)> = Vec::new();
        let mut left: Vec<(u64, Alarm, usize)> = Vec::new();
        let mut long: Vec<(u64, usize)> = Vec::new();
        for entry in &self.by_id {
            let seq = entry.registration_seq;
            let alarmed = state.alarms.contains_key(&seq);
            let view = entry.handle.mail_view();
            let Some(edge) = backlog_edge(alarmed, view.mailbox_len, enable, disable) else {
                if let Some(alarm) = state.alarms.get_mut(&seq) {
                    alarm.seen = pass;
                    long.push((seq, view.mailbox_len));
                }
                continue;
            };
            if entry.status_rx.borrow().reason.is_some() {
                // An ended actor is gone as far as its mailbox is concerned.
                continue;
            }
            if edge {
                entered.push((
                    seq,
                    entry.key().0.clone(),
                    entry.stop_lane.abort_record().clone(),
                    view.mailbox_len,
                    view.mailbox_capacity,
                ));
            } else if let Some(alarm) = state.alarms.remove(&seq) {
                left.push((seq, alarm, view.mailbox_len));
            }
        }
        // A reported mailbox the walk did not see alive is reported short once.
        let gone: Vec<u64> = state
            .alarms
            .iter()
            .filter(|(_, alarm)| alarm.seen != pass)
            .map(|(seq, _)| *seq)
            .collect();
        for seq in gone {
            if let Some(alarm) = state.alarms.remove(&seq) {
                left.push((seq, alarm, 0));
            }
        }
        for (seq, alarm, len) in left {
            let event = || TelemetryEvent::MailboxBacklog {
                actor: alarm.identity.actor().clone(),
                entered: false,
                len,
                capacity: alarm.capacity,
            };
            if to_sinks {
                mode.push_owned(&alarm.identity, EventKind::MailboxBacklog, event);
            }
            // The monitor is owed the short report only for a long one it
            // took, or one whose answer is still out.
            let after = match alarm.told {
                Told::Landed => None,
                Told::Pending(tag) => Some(tag),
                Told::Unoffered | Told::Refused | Told::Unowed => continue,
            };
            if inbox.is_none() {
                continue;
            }
            state.leaving.insert(
                seq,
                Leaving {
                    identity: alarm.identity.clone(),
                    len,
                    capacity: alarm.capacity,
                    after,
                    told: Told::Unoffered,
                },
            );
        }
        for (seq, id, record, len, capacity) in entered {
            let Some(identity) =
                crate::telemetry::identity_for(mode, &record, Some(self), Some(&id))
            else {
                // The registry has let go of the actor and nothing recorded
                // it, so no report can name it: counted with the other drops.
                mode.pump.count_dropped();
                continue;
            };
            if to_sinks {
                mode.push_owned(&identity, EventKind::MailboxBacklog, || {
                    TelemetryEvent::MailboxBacklog {
                        actor: identity.actor().clone(),
                        entered: true,
                        len,
                        capacity,
                    }
                });
            }
            // A mailbox long again ends the short state the monitor was
            // still owed.
            state.leaving.remove(&seq);
            let told = match inbox {
                // A mailbox long at the monitor's first look stood before it,
                // and the monitor's own mailbox is never reported to it.
                Some(inbox) if !state.first_look && !inbox.is_monitor(identity.actor()) => {
                    Told::Unoffered
                }
                _ => Told::Unowed,
            };
            state.alarms.insert(
                seq,
                Alarm {
                    identity,
                    capacity,
                    seen: pass,
                    told,
                },
            );
            long.push((seq, len));
        }
        let Some(inbox) = inbox else {
            return;
        };
        // Offer each long mailbox the monitor has not taken yet, with its
        // length now, and each short report owed and not held back.
        for (seq, len) in long {
            let Some(alarm) = state.alarms.get(&seq) else {
                continue;
            };
            if !matches!(alarm.told, Told::Unoffered | Told::Refused) {
                continue;
            }
            let event = TelemetryEvent::MailboxBacklog {
                actor: alarm.identity.actor().clone(),
                entered: true,
                len,
                capacity: alarm.capacity,
            };
            let told = Self::offer_to_monitor(state, inbox, Offer::Entered(seq), event);
            if let Some(alarm) = state.alarms.get_mut(&seq) {
                alarm.told = told;
            }
        }
        let owed: Vec<u64> = state
            .leaving
            .iter()
            .filter(|(_, l)| l.after.is_none() && matches!(l.told, Told::Unoffered | Told::Refused))
            .map(|(seq, _)| *seq)
            .collect();
        for seq in owed {
            let Some(leaving) = state.leaving.get(&seq) else {
                continue;
            };
            let event = TelemetryEvent::MailboxBacklog {
                actor: leaving.identity.actor().clone(),
                entered: false,
                len: leaving.len,
                capacity: leaving.capacity,
            };
            let told = Self::offer_to_monitor(state, inbox, Offer::Left(seq), event);
            if let Some(leaving) = state.leaving.get_mut(&seq) {
                leaving.told = told;
            }
        }
    }
}

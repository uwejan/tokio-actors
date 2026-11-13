//! The system monitor: the thresholds a monitor actor is told about when a
//! turn or a mailbox crosses them.
//!
//! The monitor is sampled. A task of the system looks at every actor now and
//! then, so a turn is reported some time after it passed its threshold (about
//! a quarter of the smallest threshold later, never sooner than 50 ms and
//! never later than a second) and a mailbox that grew and shrank between two
//! looks is not reported. Erlang/OTP's `erlang:system_monitor/2` checks a
//! queue length when a message is enqueued; this monitor does not. There is
//! no report for garbage collection, heap size or ports, which this runtime
//! does not have.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::actor_ref::ActorRef;
use super::event::TelemetryEvent;
use super::identity::Identity;
use super::mode::{Mode, CTL_WATCHDOG};
use super::sinks::SinkTables;
use super::subscription::{Delivery, SubscriptionId, SubscriptionNotice};
use crate::actor::handle::ActorHandle;
use crate::actor::panic::{catch_sync, payload_into_string};
use crate::actor::Actor;

/// What the monitor watches for. An empty configuration watches nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MonitorConfig {
    /// Report a turn that has run longer than this. A zero duration watches
    /// nothing.
    pub long_schedule: Option<Duration>,
    /// Report a mailbox that reached a length, and again when it fell back.
    pub long_message_queue: Option<LongQueue>,
}

/// The two lengths of a mailbox the monitor reports on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LongQueue {
    /// The length at or below which a mailbox that was reported long is
    /// reported short again.
    pub disable: usize,
    /// The length at or above which a mailbox is reported long.
    pub enable: usize,
}

/// The monitor a system holds and what it watches for.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorSettings {
    /// The actor that is told.
    pub monitor: ActorRef,
    /// What it is told about.
    pub config: MonitorConfig,
}

/// Why a system could not take a monitor.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum MonitorError {
    /// The length that reports a mailbox long is zero.
    #[error("the length that enables the report is zero")]
    ZeroEnable,
    /// The length that ends the report is not below the length that starts it.
    #[error("the length that disables the report is not below the one that enables it")]
    DisableNotBelowEnable,
    /// The monitor actor has ended, or is not an actor of this system.
    #[error("the monitor actor has ended")]
    MonitorEnded,
    /// The runtime has no time driver, which the monitor's sampling needs.
    #[error("the runtime has no time driver")]
    NoTimeDriver,
}

/// The monitor a system holds, as the sink tables keep it.
#[derive(Debug)]
pub(crate) struct MonitorSlot {
    pub(crate) settings: MonitorSettings,
    /// The number that names this monitor and no other.
    pub(crate) sub: SubscriptionId,
    /// Where the sweep offers the monitor's reports.
    pub(crate) inbox: Arc<MonitorInbox>,
    /// Ends the helper that delivers to the monitor.
    pub(crate) cancel: CancellationToken,
}

/// The length of the queue between the sweep and the helper that delivers to
/// the monitor.
pub(crate) const MONITOR_QUEUE: usize = 64;

/// One report the sweep offers to the monitor, with the sweep's number for
/// this offer; with no event, a prompt to tell the monitor the count of the
/// reports it missed.
#[derive(Debug)]
pub(crate) struct Report {
    pub(crate) event: Option<Arc<TelemetryEvent<'static>>>,
    pub(crate) tag: u64,
}

/// What the sweep and the helper that delivers to the monitor share: the
/// queue of reports, the count of reports the monitor was not given, and the
/// helper's answer for each offer. The sweep offers a report only to this
/// queue, never through the pump, so a report it offers again reaches the
/// monitor alone. Nothing here waits, and no lock is held across user code.
#[derive(Debug)]
pub(crate) struct MonitorInbox {
    tx: mpsc::Sender<Report>,
    /// Reports the monitor's mailbox refused or the queue could not take,
    /// not yet told to the monitor.
    missed: AtomicU64,
    /// The helper's answers, by offer: whether the message went into the
    /// monitor's mailbox. A leaf lock over plain numbers.
    outcomes: Mutex<Vec<(u64, bool)>>,
    /// The monitor itself, whose own turns and mailbox are never offered.
    monitor: ActorRef,
}

impl MonitorInbox {
    /// An inbox for `monitor` and the receiver its helper drains.
    pub(crate) fn new(monitor: ActorRef) -> (Arc<Self>, mpsc::Receiver<Report>) {
        let (tx, rx) = mpsc::channel(MONITOR_QUEUE);
        let inbox = Self {
            tx,
            missed: AtomicU64::new(0),
            outcomes: Mutex::new(Vec::new()),
            monitor,
        };
        (Arc::new(inbox), rx)
    }

    /// Offers `event` as offer `tag`, without waiting. `false` when the queue
    /// is full or its helper is gone; the report is then counted as missed.
    pub(crate) fn offer(&self, event: TelemetryEvent<'static>, tag: u64) -> bool {
        let report = Report {
            event: Some(Arc::new(event)),
            tag,
        };
        if self.tx.try_send(report).is_ok() {
            return true;
        }
        self.missed.fetch_add(1, Ordering::AcqRel);
        false
    }

    /// Prompts the helper to tell the monitor the count of the reports it
    /// missed, when a count waits and the queue is empty: a count whose
    /// notice the full mailbox refused is offered again at each look until it
    /// lands, as a refused report is.
    pub(crate) fn prompt_missed(&self) {
        if self.missed.load(Ordering::Acquire) != 0 && self.tx.capacity() == self.tx.max_capacity()
        {
            let _ = self.tx.try_send(Report {
                event: None,
                tag: 0,
            });
        }
    }

    /// Whether `actor` is the monitor itself.
    pub(crate) fn is_monitor(&self, actor: &ActorRef) -> bool {
        *actor == self.monitor
    }

    /// The answers given since the last call, in the order they were given.
    pub(crate) fn take_outcomes(&self) -> Vec<(u64, bool)> {
        std::mem::take(&mut *self.outcomes.lock().unwrap_or_else(|p| p.into_inner()))
    }

    fn answer(&self, tag: u64, landed: bool) {
        self.outcomes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((tag, landed));
    }
}

/// The inbox of the system's monitor and the number naming that monitor, if
/// a monitor is installed.
pub(crate) fn inbox(mode: &Mode) -> Option<(SubscriptionId, Arc<MonitorInbox>)> {
    mode.sinks
        .tables
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .monitor
        .as_ref()
        .map(|slot| (slot.sub, slot.inbox.clone()))
}

/// Checks the lengths of a configuration.
pub(crate) fn validate(config: &MonitorConfig) -> Result<(), MonitorError> {
    if let Some(queue) = config.long_message_queue {
        if queue.enable == 0 {
            return Err(MonitorError::ZeroEnable);
        }
        if queue.disable >= queue.enable {
            return Err(MonitorError::DisableNotBelowEnable);
        }
    }
    Ok(())
}

/// The configuration without a zero `long_schedule`, which watches nothing.
pub(crate) fn normalized(config: MonitorConfig) -> MonitorConfig {
    MonitorConfig {
        long_schedule: config.long_schedule.filter(|d| !d.is_zero()),
        long_message_queue: config.long_message_queue,
    }
}

/// Whether the configuration watches nothing.
pub(crate) fn is_empty(config: &MonitorConfig) -> bool {
    config.long_schedule.is_none() && config.long_message_queue.is_none()
}

/// The `long_schedule` cell: nanoseconds, 0 when off.
fn schedule_cell(config: &MonitorConfig) -> u64 {
    config.long_schedule.map_or(0, |d| {
        u64::try_from(d.as_nanos()).unwrap_or(u64::MAX).max(1)
    })
}

/// The queue cell: `enable` in the high half and `disable` in the low half,
/// each clamped to 32 bits with `disable` kept below `enable`; 0 when off.
fn queue_cell(config: &MonitorConfig) -> u64 {
    config.long_message_queue.map_or(0, |queue| {
        let enable = u32::try_from(queue.enable).unwrap_or(u32::MAX).max(1);
        let disable = u32::try_from(queue.disable)
            .unwrap_or(u32::MAX)
            .min(enable - 1);
        (u64::from(enable) << 32) | u64::from(disable)
    })
}

/// The lengths of a queue cell as `(enable, disable)`, or `None` when off.
pub(crate) fn unpack_queue(cell: u64) -> Option<(usize, usize)> {
    (cell != 0).then_some(((cell >> 32) as usize, (cell & 0xffff_ffff) as usize))
}

/// Sets the watchdog control bit from the two thresholds that want it: the
/// user's own and the monitor's `long_schedule`. The registry mutex is held
/// by the caller (`_tables`), so two changes cannot interleave their reads
/// and their store.
fn refresh_locked(mode: &Mode, _tables: &SinkTables, watchdog: &AtomicU64) {
    let on = watchdog.load(Ordering::Relaxed) != 0
        || mode.monitor_long_schedule_nanos.load(Ordering::Relaxed) != 0;
    mode.set_ctl(CTL_WATCHDOG, on);
}

/// Sets the watchdog control bit from the user's threshold `watchdog` and the
/// monitor's `long_schedule`; call it after storing a new `watchdog` value.
pub(crate) fn refresh_watchdog(mode: &Mode, watchdog: &AtomicU64) {
    let tables = mode
        .sinks
        .tables
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    refresh_locked(mode, &tables, watchdog);
}

/// Makes the monitor in `settings` the system's monitor, named by `sub` and
/// reached through `inbox`, and returns the monitor it replaces, whose helper
/// is told to end. The monitor holds no subscription and arms no event kind:
/// the sweep reads its thresholds from the mode and offers its reports to
/// `inbox`.
pub(crate) fn install(
    mode: &Mode,
    settings: MonitorSettings,
    sub: SubscriptionId,
    inbox: Arc<MonitorInbox>,
    cancel: CancellationToken,
    watchdog: &AtomicU64,
) -> Option<MonitorSettings> {
    let previous = {
        let mut tables = mode
            .sinks
            .tables
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = tables.monitor.take();
        mode.monitor_long_schedule_nanos
            .store(schedule_cell(&settings.config), Ordering::Relaxed);
        mode.monitor_queue
            .store(queue_cell(&settings.config), Ordering::Relaxed);
        tables.monitor = Some(MonitorSlot {
            settings,
            sub,
            inbox,
            cancel,
        });
        refresh_locked(mode, &tables, watchdog);
        previous
    };
    previous.map(|old| {
        old.cancel.cancel();
        old.settings
    })
}

/// Removes the monitor and its thresholds, when the slot satisfies `mine`;
/// returns the settings removed.
fn clear_where(
    mode: &Mode,
    watchdog: &AtomicU64,
    mine: impl FnOnce(&MonitorSlot) -> bool,
) -> Option<MonitorSettings> {
    let removed = {
        let mut tables = mode
            .sinks
            .tables
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !tables.monitor.as_ref().is_some_and(mine) {
            return None;
        }
        let old = tables.monitor.take()?;
        mode.monitor_long_schedule_nanos.store(0, Ordering::Relaxed);
        mode.monitor_queue.store(0, Ordering::Relaxed);
        refresh_locked(mode, &tables, watchdog);
        old
    };
    removed.cancel.cancel();
    Some(removed.settings)
}

/// Removes the system's monitor and returns its settings.
pub(crate) fn clear(mode: &Mode, watchdog: &AtomicU64) -> Option<MonitorSettings> {
    clear_where(mode, watchdog, |_| true)
}

/// The settings of the system's monitor.
pub(crate) fn settings(mode: &Mode) -> Option<MonitorSettings> {
    mode.sinks
        .tables
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .monitor
        .as_ref()
        .map(|slot| slot.settings.clone())
}

/// Clears the monitor reached through `sub` when its helper ends, on any
/// path: the monitor actor ended, the map panicked, the helper was told to
/// end, or its task was dropped without ever running. A monitor that has
/// replaced it is left alone.
pub(crate) struct MonitorGuard {
    mode: Arc<Mode>,
    sub: SubscriptionId,
    watchdog: Arc<AtomicU64>,
}

impl MonitorGuard {
    pub(crate) fn new(mode: Arc<Mode>, sub: SubscriptionId, watchdog: Arc<AtomicU64>) -> Self {
        Self {
            mode,
            sub,
            watchdog,
        }
    }
}

impl Drop for MonitorGuard {
    fn drop(&mut self) {
        let sub = self.sub;
        let _ = clear_where(&self.mode, &self.watchdog, |slot| slot.sub == sub);
    }
}

/// The helper that delivers the monitor's reports: takes each report the
/// sweep offered out of the inbox, maps it into a message of the monitor and
/// offers it to the monitor's mailbox without waiting, and answers the sweep
/// whether it went in. A message the full mailbox refuses is counted, and the
/// count reaches the monitor as an [`Overflow`](SubscriptionNotice::Overflow)
/// notice once the inbox runs empty; the sweep offers the refused report's
/// state again at its next look while the state lasts, so a refusal is never
/// silent and a state the monitor was told is never told twice.
/// The helper ends when the monitor's status plane reads a final reason, when
/// it is told to end, or when the map panics (after one more call with a
/// notice that it did).
pub(crate) async fn forward<A, F>(
    _guard: MonitorGuard,
    mut rx: mpsc::Receiver<Report>,
    inbox: Arc<MonitorInbox>,
    mut map: F,
    handle: ActorHandle<A>,
    cancel: CancellationToken,
) where
    A: Actor,
    F: FnMut(Delivery) -> A::Message + Send + 'static,
{
    let mut status = handle.status_rx();
    loop {
        let report = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            _ = crate::system::wait_final_reason(&mut status) => return,
            report = rx.recv() => report,
        };
        let Some(Report { event, tag }) = report else {
            return;
        };
        if let Some(event) = event {
            let delivery = Delivery {
                event,
                missed_before: 0,
            };
            let Some(landed) = deliver(&mut map, delivery, &handle) else {
                return;
            };
            if !landed {
                inbox.missed.fetch_add(1, Ordering::AcqRel);
            }
            inbox.answer(tag, landed);
        }
        if rx.is_empty() {
            let missed = inbox.missed.swap(0, Ordering::AcqRel);
            if missed > 0 {
                let notice = Delivery {
                    event: Arc::new(TelemetryEvent::Notice(SubscriptionNotice::Overflow {
                        missed,
                    })),
                    missed_before: 0,
                };
                match deliver(&mut map, notice, &handle) {
                    None => return,
                    // The count waits for the next notice, which the sweep
                    // prompts at its next look.
                    Some(false) => {
                        inbox.missed.fetch_add(missed, Ordering::AcqRel);
                    }
                    Some(true) => {}
                }
            }
        }
    }
}

/// Maps `delivery` and offers the message to the monitor: `Some(true)` when
/// it went in, `Some(false)` when the mailbox refused it. A map that panics
/// is called once more with the notice that it did, and ends the helper
/// (`None`). A message the mailbox refuses is dropped inside a catch.
fn deliver<A, F>(map: &mut F, delivery: Delivery, handle: &ActorHandle<A>) -> Option<bool>
where
    A: Actor,
    F: FnMut(Delivery) -> A::Message + Send + 'static,
{
    match catch_sync(|| map(delivery)) {
        Ok(msg) => Some(offer(handle, msg)),
        Err(payload) => {
            let _ = payload_into_string(payload);
            #[cfg(feature = "tracing")]
            caught_log!(warn,
                actor_id = %handle.id(),
                "the map of the system monitor panicked; the monitor ends"
            );
            let notice = Delivery {
                event: Arc::new(TelemetryEvent::Notice(SubscriptionNotice::MapPanicked)),
                missed_before: 0,
            };
            // A second panic ends the monitor without a notice.
            if let Ok(msg) = catch_sync(|| map(notice)) {
                offer(handle, msg);
            }
            None
        }
    }
}

/// Offers `msg` to the monitor's mailbox; `true` when it went in. The
/// message and the refusal are dropped inside a catch, since the message is
/// the user's.
fn offer<A: Actor>(handle: &ActorHandle<A>, msg: A::Message) -> bool {
    match catch_sync(|| handle.try_notify(msg).is_ok()) {
        Ok(landed) => landed,
        Err(payload) => {
            let _ = payload_into_string(payload);
            false
        }
    }
}

/// What the monitor knows of one state the sweep tracks: nothing offered yet,
/// an offer awaiting the helper's answer, told, refused and owed again, or
/// owed nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Told {
    /// Not offered to the monitor.
    Unoffered,
    /// Offered as the offer with this number; no answer yet.
    Pending(u64),
    /// The monitor's mailbox took it.
    Landed,
    /// The monitor's mailbox refused it: offered again at the next look.
    Refused,
    /// Nothing is owed to this monitor for the state: it stood before the
    /// monitor's first look, it is the monitor's own turn or mailbox, no
    /// monitor is installed, or no report can name it. It is never offered,
    /// and its end owes the monitor no short report.
    Unowed,
}

/// Which state an offer told: a stuck turn, a long mailbox, or a mailbox
/// that left, each by registration number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Offer {
    Turn(u64),
    Entered(u64),
    Left(u64),
}

/// A turn the sweep found stuck: its token, the look that last saw it, and
/// what the monitor knows of it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stuck {
    pub(crate) token: u64,
    pub(crate) seen: u64,
    pub(crate) told: Told,
}

/// A mailbox that fell back, owed to the monitor because it was told the
/// mailbox was long: the report's fields and what the monitor knows of it.
/// `after` holds the offer of the long report while that one awaits its
/// answer: the short report goes out only once the long one landed.
pub(crate) struct Leaving {
    pub(crate) identity: Arc<Identity>,
    pub(crate) len: usize,
    pub(crate) capacity: usize,
    pub(crate) after: Option<u64>,
    pub(crate) told: Told,
}

/// A mailbox the sweep reported long, until it is reported short again.
pub(crate) struct Alarm {
    /// The actor, kept so the report that it left can name it after the
    /// registry has let go of it.
    pub(crate) identity: Arc<Identity>,
    pub(crate) capacity: usize,
    /// The pass that last saw the actor alive.
    pub(crate) seen: u64,
    /// What the monitor knows of the long mailbox.
    pub(crate) told: Told,
}

/// What the sweep remembers between its looks.
#[derive(Default)]
pub(crate) struct SweepState {
    /// The turn already reported for a registration.
    pub(crate) stuck: HashMap<u64, Stuck>,
    /// The mailboxes the monitor is owed a short report for, by registration
    /// number.
    pub(crate) leaving: HashMap<u64, Leaving>,
    /// The offers awaiting the helper's answer, by offer number.
    pub(crate) offered: HashMap<u64, Offer>,
    /// The number of the next offer.
    pub(crate) next_tag: u64,
    /// The monitor the states above were told to; a new monitor is owed none
    /// of the reports made before it.
    pub(crate) monitor: Option<SubscriptionId>,
    /// Whether this look is the first for its monitor: a mailbox it finds
    /// long stood before the monitor, which is owed nothing for it.
    pub(crate) first_look: bool,
    /// The mailboxes reported long, by registration number.
    pub(crate) alarms: HashMap<u64, Alarm>,
    /// The turns already warned about in the log, by actor.
    #[cfg(feature = "tracing")]
    pub(crate) warned: HashMap<crate::types::ActorId, u64>,
    /// The number of looks so far.
    pub(crate) pass: u64,
}

/// What a mailbox length means for a mailbox that is `alarmed` or not:
/// `Some(true)` when it reached `enable`, `Some(false)` when it fell to
/// `disable`, `None` between the two or when nothing changed.
pub(crate) fn backlog_edge(
    alarmed: bool,
    len: usize,
    enable: usize,
    disable: usize,
) -> Option<bool> {
    if alarmed {
        (len <= disable).then_some(false)
    } else {
        (len >= enable).then_some(true)
    }
}

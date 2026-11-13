//! What the system's telemetry state knows about one incarnation, kept only
//! while some sink takes events.
//!
//! An actor carries no telemetry state of its own. A spawn made while a sink
//! takes events (or a tracing subscriber wants the actor's span) records an
//! [`Identity`] in the system's table, keyed by the incarnation's abort
//! record. A spawn made with no sink records nothing: the first event about
//! that incarnation (or about a child that names it as supervisor) reads its
//! id, number and name from the registry entry and its supervisor from the
//! entry above it, and records the result. The entry is removed when the last
//! holder of the incarnation's record lets go.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::time::Instant;

use super::actor_ref::{ActorRef, UNKNOWN_INCARNATION};
use super::hot::HotCache;
use crate::types::ActorId;

/// Where a supervised restart's spawn publishes the incarnation it creates,
/// and the stop bounds it publishes with it.
#[derive(Clone, Copy)]
pub(crate) struct PublishTo<'a> {
    pub(crate) io: &'a crate::actor::supervision::slot::SlotIo,
    pub(crate) own: &'a crate::actor::supervision::slot::OwnStop,
}

impl std::fmt::Debug for PublishTo<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PublishTo")
    }
}

/// What a spawn is told about the incarnation it creates.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Birth<'a> {
    /// The incarnation number, or `None` to draw the next one from the
    /// system when one is wanted.
    pub(crate) incarnation: Option<u64>,
    /// The id and the incarnation number of the supervisor spawning the
    /// incarnation, or `None` for a root. The registry entry keeps them, and
    /// the supervisor's identity is looked up by both, so a child never names
    /// another incarnation of its supervisor's id.
    pub(crate) parent: Option<(&'a ActorId, u64)>,
    /// The slot of a restart attempt, which the spawn publishes the
    /// incarnation's stop lane, status plane and subtree to before the
    /// incarnation's task can run, or `None` for every other spawn (a first
    /// start publishes to its own slot right after the spawn, on the
    /// supervisor's task, where nothing else reads the slot).
    pub(crate) publish: Option<PublishTo<'a>>,
}

impl Birth<'_> {
    /// The birth of a root actor.
    pub(crate) fn root() -> Self {
        Self {
            incarnation: None,
            parent: None,
            publish: None,
        }
    }
}

/// The identity of one incarnation: its id, its incarnation number, the
/// identity of its supervisor, its registered name and, when the end event
/// was armed at the spawn, its birth time.
#[derive(Debug)]
pub(crate) struct Identity {
    id: ActorId,
    incarnation: u64,
    /// The supervisor's identity, from which the ancestry is built on first
    /// use.
    parent: Option<Arc<Identity>>,
    /// The incarnation's reference, built by the first event that needs it.
    actor: OnceLock<ActorRef>,
    pub(crate) name: Option<Arc<str>>,
    /// Read at spawn only when `ActorEnded` is armed then.
    born: Option<Instant>,
    /// The type name of the actor value; empty for an identity the registry
    /// gave after the spawn.
    type_name: &'static str,
    /// The label the actor set for itself; unset for an unlabelled actor.
    label: OnceLock<Mutex<Arc<str>>>,
    /// The restart attempts of a supervisor that have begun and not settled.
    restarts_in_flight: AtomicU32,
    /// The events about this incarnation that the pump holds: raised when one
    /// is accepted into the queue, lowered when its delivery ends. A hot event
    /// of the incarnation runs its handlers on its own task only while this
    /// reads zero, so it never overtakes an event it follows.
    pumped: AtomicU32,
    /// The children of a supervisor whose death it owes a decision, by
    /// incarnation: `claimed` while a child runs under a runner, `owed` from
    /// the child's end until the decision is made.
    deaths: Mutex<Deaths>,
    /// The size of `deaths.owed`, read without the lock.
    deaths_owed: AtomicU32,
    /// The size of both sets, so a decision with nothing tracked takes no
    /// lock.
    deaths_open: AtomicU32,
    /// Whether a group restart of this supervisor is in flight: raised at the
    /// group decision, lowered when the group's last member has its fate.
    group_pending: AtomicBool,
    /// Whether the actor is sensitive: no log line prints text derived from
    /// its failures.
    sensitive: bool,
}

/// The children whose death a supervisor owes a decision.
#[derive(Debug, Default)]
struct Deaths {
    /// Running under a runner.
    claimed: std::collections::HashSet<u64>,
    /// Ended, decision not yet made.
    owed: std::collections::HashSet<u64>,
}

impl Identity {
    /// The identity of an incarnation being spawned: a root, or a child of
    /// the supervisor whose identity is `parent`.
    pub(crate) fn new(
        id: ActorId,
        incarnation: u64,
        parent: Option<Arc<Identity>>,
        name: Option<Arc<str>>,
        born: Option<Instant>,
    ) -> Self {
        Self {
            id,
            incarnation,
            parent,
            actor: OnceLock::new(),
            name,
            born,
            type_name: "",
            label: OnceLock::new(),
            restarts_in_flight: AtomicU32::new(0),
            pumped: AtomicU32::new(0),
            deaths: Mutex::new(Deaths::default()),
            deaths_owed: AtomicU32::new(0),
            deaths_open: AtomicU32::new(0),
            group_pending: AtomicBool::new(false),
            sensitive: false,
        }
    }

    /// The same identity, marked sensitive or not.
    pub(crate) fn with_sensitive(mut self, sensitive: bool) -> Self {
        self.sensitive = sensitive;
        self
    }

    /// Whether the actor is sensitive.
    pub(crate) fn sensitive(&self) -> bool {
        self.sensitive
    }

    /// Counts one more event about the incarnation in the pump.
    pub(super) fn raise_pumped(&self) {
        self.pumped.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts one event about the incarnation delivered or given up on.
    pub(super) fn lower_pumped(&self) {
        self.pumped.fetch_sub(1, Ordering::Release);
    }

    /// Whether an event about the incarnation is queued or being delivered:
    /// one acquire load.
    ///
    /// The pump lowers the count with a release after the handlers of the
    /// event have returned. Every change of the count, a raise from any task
    /// or a lower, is a read-modify-write, and a read-modify-write continues
    /// the release sequence it follows, so the value a load reads lies in the
    /// release sequence of every lower before it in the count's order. A load
    /// that reads zero with acquire therefore synchronizes with all of those
    /// lowers, and what the handlers of every earlier event wrote is visible
    /// before the caller runs a handler inline: a handler that keeps its
    /// state without a lock reads the state the earlier events left. A zero
    /// read from a count never raised has no earlier event to order against.
    #[inline]
    pub(super) fn has_pumped(&self) -> bool {
        self.pumped.load(Ordering::Acquire) != 0
    }

    /// The same identity, told the type name of its actor value, which only a
    /// spawn knows.
    pub(crate) fn with_type(mut self, type_name: &'static str) -> Self {
        self.type_name = type_name;
        self
    }

    /// A supervisor whose spawn recorded nothing: only its id is known, so it
    /// reads as a root with the unknown incarnation number.
    pub(crate) fn unknown(id: ActorId) -> Self {
        Self::new(id, UNKNOWN_INCARNATION, None, None, None)
    }

    /// The incarnation's reference, with its ancestry. Built on first use,
    /// when an event needs it, from the supervisor's identity.
    pub(crate) fn actor(&self) -> &ActorRef {
        self.actor.get_or_init(|| match &self.parent {
            Some(parent) => ActorRef::new_child(self.id.clone(), self.incarnation, parent.actor()),
            None => ActorRef::new_root(self.id.clone(), self.incarnation),
        })
    }

    /// Whether the reference has been built.
    #[cfg(test)]
    pub(crate) fn actor_built(&self) -> bool {
        self.actor.get().is_some()
    }

    /// The incarnation number.
    pub(crate) fn incarnation(&self) -> u64 {
        self.incarnation
    }

    /// The time since the incarnation was born, when it was recorded.
    pub(crate) fn lifetime(&self) -> Option<Duration> {
        self.born.map(|born| born.elapsed())
    }

    /// The caller token of the incarnation in the wait graph: its incarnation
    /// number, which is never 0 and is unique in the process; 0 for the
    /// unknown incarnation.
    pub(crate) fn token(&self) -> u64 {
        if self.incarnation == UNKNOWN_INCARNATION {
            0
        } else {
            self.incarnation
        }
    }

    /// The type name of the actor value, empty when the spawn did not record
    /// one.
    pub(crate) fn type_name(&self) -> &'static str {
        self.type_name
    }

    /// The label the actor set for itself, if it did.
    pub(crate) fn label(&self) -> Option<Arc<str>> {
        let cell = self.label.get()?;
        Some(cell.lock().unwrap_or_else(|p| p.into_inner()).clone())
    }

    /// Sets the actor's label, replacing an earlier one.
    pub(crate) fn set_label(&self, label: Arc<str>) {
        let cell = self.label.get_or_init(|| Mutex::new(label.clone()));
        *cell.lock().unwrap_or_else(|p| p.into_inner()) = label;
    }

    /// The identity of the supervisor this incarnation was spawned under.
    pub(crate) fn parent(&self) -> Option<&Arc<Identity>> {
        self.parent.as_ref()
    }

    /// Runs `f` on the death sets and refreshes the two counters.
    fn with_deaths(&self, f: impl FnOnce(&mut Deaths)) {
        let mut deaths = self.deaths.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut deaths);
        let owed = u32::try_from(deaths.owed.len()).unwrap_or(u32::MAX);
        let open = u32::try_from(deaths.claimed.len() + deaths.owed.len()).unwrap_or(u32::MAX);
        self.deaths_owed.store(owed, Ordering::Release);
        self.deaths_open.store(open, Ordering::Release);
    }

    /// A runner now follows the child `incarnation` of this supervisor: its
    /// end will owe a decision. `ended` is true when the child's end is
    /// already recorded, in which case the death is owed at once.
    pub(crate) fn death_claim(&self, incarnation: u64, ended: bool) {
        self.with_deaths(|deaths| {
            if ended {
                deaths.owed.insert(incarnation);
            } else if !deaths.owed.contains(&incarnation) {
                deaths.claimed.insert(incarnation);
            }
        });
    }

    /// The child `incarnation` ended, before its registry entry left: its
    /// death is owed from here if a runner follows it. An incarnation no
    /// runner follows counts nothing, so a stray the supervisor never
    /// adopted leaves no count behind.
    pub(crate) fn death_ended(&self, incarnation: u64) {
        // Pairs with the fence after a claim: of an end and a claim, the one
        // that comes second sees the other.
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.deaths_open.load(Ordering::Acquire) == 0 {
            return;
        }
        self.with_deaths(|deaths| {
            if deaths.claimed.remove(&incarnation) {
                deaths.owed.insert(incarnation);
            }
        });
    }

    /// A runner landed the death of `incarnation` in the supervisor's death
    /// plane: owed if it was not already, whichever path reported it.
    pub(crate) fn death_landed(&self, incarnation: u64) {
        self.with_deaths(|deaths| {
            deaths.claimed.remove(&incarnation);
            deaths.owed.insert(incarnation);
        });
    }

    /// The supervisor decided on the death of `incarnation` (a restart ticket
    /// or the final state took over) or dropped it. Safe to repeat and for an
    /// incarnation never counted.
    pub(crate) fn death_decided(&self, incarnation: u64) {
        if self.deaths_open.load(Ordering::Acquire) == 0 {
            return;
        }
        self.with_deaths(|deaths| {
            deaths.claimed.remove(&incarnation);
            deaths.owed.remove(&incarnation);
        });
    }

    /// The deaths owed a decision, without the group: a test reads the book
    /// apart from the group's flag.
    #[cfg(test)]
    pub(crate) fn owed_deaths_for_test(&self) -> u32 {
        self.deaths_owed.load(Ordering::Acquire)
    }

    /// The deaths tracked, claimed or owed.
    #[cfg(test)]
    pub(crate) fn open_deaths_for_test(&self) -> u32 {
        self.deaths_open.load(Ordering::Acquire)
    }

    /// Whether the group's flag is raised.
    #[cfg(test)]
    pub(crate) fn group_is_pending(&self) -> bool {
        self.group_pending.load(Ordering::Acquire)
    }

    /// A group restart of this supervisor is in flight (`open`) or has
    /// completed or ended (`!open`). The group is owed work from the group
    /// decision until its last member has its fate, so the span between its
    /// stops and its starts is not quiet. Safe to repeat.
    pub(crate) fn group_pending(&self, open: bool) {
        self.group_pending.store(open, Ordering::Release);
    }

    /// The work the supervisor owes the deaths it has seen: one for each
    /// death owed a decision, and one more while a group restart is in
    /// flight.
    pub(crate) fn deaths_owed(&self) -> u32 {
        self.deaths_owed.load(Ordering::Acquire)
            + u32::from(self.group_pending.load(Ordering::Acquire))
    }

    /// A restart attempt began.
    pub(crate) fn restart_started(&self) {
        self.restarts_in_flight.fetch_add(1, Ordering::AcqRel);
    }

    /// A restart attempt settled; never takes the count below zero.
    pub(crate) fn restart_settled(&self) {
        let mut seen = self.restarts_in_flight.load(Ordering::Acquire);
        while seen > 0 {
            match self.restarts_in_flight.compare_exchange_weak(
                seen,
                seen - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(now) => seen = now,
            }
        }
    }

    /// The restart attempts that have begun and not settled.
    pub(crate) fn restarts_in_flight(&self) -> u32 {
        self.restarts_in_flight.load(Ordering::Acquire)
    }
}

/// The identity fields every actor-scoped log statement carries besides the
/// actor's id; each reads as absent for an incarnation with no identity.
#[cfg(feature = "tracing")]
#[derive(Clone, Default)]
pub(crate) struct LogFields<'a> {
    ident: Option<Arc<Identity>>,
    system: Option<std::borrow::Cow<'a, str>>,
    /// Whether the actor is sensitive, known beyond its identity.
    sensitive: bool,
}

#[cfg(feature = "tracing")]
impl<'a> LogFields<'a> {
    /// The fields of an optional identity in the system named `system`.
    pub(crate) fn of(ident: Option<Arc<Identity>>, system: &'a str) -> Self {
        Self {
            system: ident
                .is_some()
                .then_some(std::borrow::Cow::Borrowed(system)),
            sensitive: false,
            ident,
        }
    }

    /// The fields of a line about an actor whose sensitivity the caller knows.
    pub(crate) fn of_sensitive(sensitive: bool) -> LogFields<'static> {
        LogFields {
            sensitive,
            ..LogFields::default()
        }
    }

    /// The fields of a line about code that belongs to no actor's failure: a
    /// handler of the system, a caller's waker. Nothing is hidden.
    pub(crate) fn foreign() -> LogFields<'static> {
        LogFields::default()
    }

    /// The fields of an identity just released at the incarnation's end, in
    /// the system its record names (read only now, past the engaged gate).
    #[inline]
    pub(crate) fn released(
        ident: Option<Arc<Identity>>,
        record: &AbortRecord,
    ) -> LogFields<'static> {
        let sensitive = record.sys_bits() & crate::actor::subtree::SYS_SENSITIVE != 0;
        match ident {
            None => LogFields::of_sensitive(sensitive),
            Some(ident) => Self::released_named(ident, record, sensitive),
        }
    }

    /// The fields of an identity that was bound, out of line: an incarnation
    /// that held none builds the empty fields and goes on.
    #[inline(never)]
    fn released_named(
        ident: Arc<Identity>,
        record: &AbortRecord,
        sensitive: bool,
    ) -> LogFields<'static> {
        let system = record
            .mode()
            .and_then(|mode| mode.system())
            .map(|system| std::borrow::Cow::Owned(system.name().to_string()));
        LogFields {
            ident: Some(ident),
            system,
            sensitive,
        }
    }

    /// The actor's registered name; empty for an anonymous actor.
    pub(crate) fn name(&self) -> Option<&str> {
        self.ident
            .as_ref()
            .map(|ident| ident.name.as_deref().unwrap_or(""))
    }

    /// The incarnation number.
    pub(crate) fn incarnation(&self) -> Option<u64> {
        self.ident.as_ref().map(|ident| ident.incarnation())
    }

    /// The name of the system the actor runs in.
    pub(crate) fn system(&self) -> Option<&str> {
        self.system.as_deref()
    }

    /// The same fields, sensitive when `sensitive` says so or they already
    /// were.
    pub(crate) fn with_sensitive(mut self, sensitive: bool) -> Self {
        self.sensitive |= sensitive;
        self
    }

    /// Whether no log line of this actor prints text derived from its
    /// failures.
    pub(crate) fn is_sensitive(&self) -> bool {
        self.sensitive || self.ident.as_ref().is_some_and(|ident| ident.sensitive())
    }

    /// `text`, the text of an error, as a log line prints it: the text
    /// itself, or, for a sensitive actor, the word `error`.
    pub(crate) fn text<'t>(&self, text: &'t str) -> std::borrow::Cow<'t, str> {
        self.hide("error", text)
    }

    /// `text`, the text of a panic, as a log line prints it: the text itself,
    /// or, for a sensitive actor, the word `panic`.
    pub(crate) fn panic_text<'t>(&self, text: &'t str) -> std::borrow::Cow<'t, str> {
        self.hide("panic", text)
    }

    /// A stop reason as a log line prints it: the reason itself, or, for a
    /// sensitive actor whose reason carries the text of a failure, the word
    /// `panic` or `error`. A reason that carries no text prints as it is.
    pub(crate) fn reason_text(
        &self,
        reason: &crate::types::StopReason,
    ) -> std::borrow::Cow<'static, str> {
        match reason {
            crate::types::StopReason::Failure(err) if self.is_sensitive() => {
                std::borrow::Cow::Borrowed(if matches!(err, crate::error::ActorError::Panic(_)) {
                    "panic"
                } else {
                    "error"
                })
            }
            other => std::borrow::Cow::Owned(other.to_string()),
        }
    }

    fn hide<'t>(&self, kind: &'static str, text: &'t str) -> std::borrow::Cow<'t, str> {
        if self.is_sensitive() {
            std::borrow::Cow::Borrowed(kind)
        } else {
            std::borrow::Cow::Borrowed(text)
        }
    }
}

use super::event::{EventKind, TelemetryEvent};
use super::mode::Mode;
use crate::actor::subtree::AbortRecord;
use crate::system::ActorSystem;

/// Runs `f` with the mode of the system an incarnation belongs to, when that
/// system takes events or holds an identity: two relaxed loads when it does
/// not. The mode is the one of the system in hand, or the one the
/// incarnation's record holds.
#[inline]
pub(crate) fn with_mode<R>(
    record: &AbortRecord,
    system: Option<&ActorSystem>,
    f: impl FnOnce(&Arc<Mode>) -> R,
) -> Option<R> {
    let mode = match system {
        Some(system) => system.telemetry_mode(),
        None => record.mode()?,
    };
    mode.engaged().then(|| f(mode))
}

/// Whether the incarnation's system takes events or holds an identity.
#[inline]
pub(crate) fn engaged_for(record: &AbortRecord, system: Option<&ActorSystem>) -> bool {
    with_mode(record, system, |_| ()).is_some()
}

/// The identity of the incarnation behind `record`: the one recorded for it,
/// or, for a caller that has the incarnation's id (`hint`), the one the
/// registry gives now, recorded for the events that follow (see
/// [`ActorSystem::resolve_identity`]). A caller with no id only finds: while
/// the system resolves identities, the pass of its engagement has recorded
/// every registered incarnation and every later spawn records its own, so a
/// miss names an incarnation the registry does not hold. The pass runs on
/// the call that engaged; a pass still owed when a caller misses runs here,
/// once. `system` is the system in hand, if any. No lock is held when this
/// returns.
pub(crate) fn identity_for(
    mode: &Mode,
    record: &AbortRecord,
    system: Option<&ActorSystem>,
    hint: Option<&ActorId>,
) -> Option<Arc<Identity>> {
    if let Some(found) = mode.find(record.key()) {
        return Some(found);
    }
    match hint {
        Some(id) => match system {
            Some(system) => system.resolve_identity(record, id),
            None => mode.system()?.resolve_identity(record, id),
        },
        None => {
            if !mode.bind_owed() {
                return None;
            }
            match system {
                Some(system) => system.bind_registered(),
                None => mode.system()?.bind_registered(),
            }
            mode.find(record.key())
        }
    }
}

/// Queues an event whose subject is the incarnation behind `record`: `build`
/// receives the incarnation's reference. Nothing is built when no sink takes
/// `kind`, and nothing is raised for an incarnation the registry does not
/// hold and the table has no identity of, which is counted as a dropped
/// event. The identity is cloned out of the
/// table before the push, so no lock is held across it.
#[inline]
pub(crate) fn push_about(
    record: &AbortRecord,
    system: Option<&ActorSystem>,
    kind: EventKind,
    build: impl FnOnce(&ActorRef) -> TelemetryEvent<'static>,
) {
    with_mode(record, system, |mode| {
        if !mode.armed(kind) {
            return;
        }
        let Some(identity) = identity_for(mode, record, system, None) else {
            // The registry does not hold the incarnation and no spawn
            // recorded it, so no event can name it: the loss is counted with
            // the other drops, never silent.
            mode.pump.count_dropped();
            return;
        };
        mode.push_owned(&identity, kind, || build(identity.actor()));
    });
}

/// Counts a runner following the child `incarnation` of the supervisor whose
/// record is `supervisor`, when turn tracking is on; see
/// [`Identity::death_claim`].
///
/// The claim is made first and the child's end is looked at after it: the
/// end flag is stored before the end reads the book, so an end that ran
/// before the claim, when it found nothing to move, is seen here, and the
/// death is owed from the child's end whatever the order. A child whose end
/// the flag did not record (its end ran while turn tracking was off) is
/// found ended by its status plane: a final reason is written, or `Stopped`.
pub(crate) fn death_claim(supervisor: &AbortRecord, incarnation: u64, child: &AbortRecord) {
    if let Some(identity) = tracked_identity(supervisor) {
        let ended = child.has_ended() || child.has_stopped() || child.has_final_reason();
        identity.death_claim(incarnation, ended);
        std::sync::atomic::fence(Ordering::SeqCst);
        if child.has_ended() {
            identity.death_ended(incarnation);
        }
    }
}

/// A runner landed the death of `incarnation` for the supervisor whose record
/// is `supervisor`; see [`Identity::death_landed`].
pub(crate) fn death_landed(supervisor: &AbortRecord, incarnation: u64) {
    if let Some(identity) = tracked_identity(supervisor) {
        identity.death_landed(incarnation);
    }
}

/// The supervisor whose record is `supervisor` decided on the death of
/// `incarnation`; see [`Identity::death_decided`].
pub(crate) fn death_decided(supervisor: &AbortRecord, incarnation: u64) {
    if let Some(identity) = recorded_identity(supervisor) {
        identity.death_decided(incarnation);
    }
}

/// The group restart of the supervisor whose record is `supervisor` is in
/// flight (`open`) or over; see [`Identity::group_pending`]. Raising resolves
/// the identity only while the system tracks turns; lowering finds it if the
/// table holds one.
pub(crate) fn group_pending(supervisor: &AbortRecord, open: bool) {
    let identity = if open {
        tracked_identity(supervisor)
    } else {
        recorded_identity(supervisor)
    };
    if let Some(identity) = identity {
        identity.group_pending(open);
    }
}

/// The identity of the incarnation behind `record`, resolved when needed,
/// while its system tracks turns; `None` otherwise.
fn tracked_identity(record: &AbortRecord) -> Option<Arc<Identity>> {
    let mode = record.mode()?;
    if mode.word() & super::mode::CTL_TRACK_TURNS == 0 {
        return None;
    }
    identity_for(mode, record, None, None)
}

/// The identity of the incarnation behind `record` if the table holds one;
/// nothing is resolved and nothing is read while the system is not engaged.
fn recorded_identity(record: &AbortRecord) -> Option<Arc<Identity>> {
    with_mode(record, None, |mode| mode.find(record.key())).flatten()
}

/// An event built for the incarnation behind a record and not yet queued, so
/// the caller can queue it at a point it chooses (inside a lock it holds).
pub(crate) enum Staged {
    /// The event, ready to queue.
    Event {
        mode: Arc<Mode>,
        kind: EventKind,
        event: TelemetryEvent<'static>,
        owner: Arc<Identity>,
    },
    /// No event can name the incarnation; queueing counts the loss once.
    Lost { mode: Arc<Mode> },
}

impl Staged {
    /// Queues the event, or counts the loss. Never waits and runs no user
    /// code, as [`Mode::push`].
    pub(crate) fn push(self) {
        match self {
            Staged::Event {
                mode,
                kind,
                event,
                owner,
            } => mode.push_built_owned(&owner, kind, event),
            Staged::Lost { mode } => mode.pump.count_dropped(),
        }
    }
}

/// Builds, without queueing it, the event [`push_about`] would queue: `None`
/// when no sink takes `kind`.
#[inline]
pub(crate) fn stage_about(
    record: &AbortRecord,
    system: Option<&ActorSystem>,
    kind: EventKind,
    build: impl FnOnce(&ActorRef) -> TelemetryEvent<'static>,
) -> Option<Staged> {
    with_mode(record, system, |mode| {
        if !mode.armed(kind) {
            return None;
        }
        Some(match identity_for(mode, record, system, None) {
            Some(identity) => Staged::Event {
                mode: mode.clone(),
                kind,
                event: build(identity.actor()),
                owner: identity,
            },
            None => Staged::Lost { mode: mode.clone() },
        })
    })
    .flatten()
}

/// Whether a sink takes events of `kind` in the incarnation's system.
#[inline]
pub(crate) fn armed_for(
    record: &AbortRecord,
    system: Option<&ActorSystem>,
    kind: EventKind,
) -> bool {
    with_mode(record, system, |mode| mode.armed(kind)).unwrap_or(false)
}

/// Builds the event and hands it to the handlers on the calling task, from
/// the loop's cached list, while the pump holds no earlier event about this
/// actor; when it holds one, the event is queued behind it, so the actor's
/// events reach a handler in the order they were raised. Returns without building
/// when no sink takes `kind`. `build` receives the incarnation's reference and
/// may borrow it.
/// Only the sites listed in the sites test call it, each one owing nothing
/// after the call. See [`Mode::emit_cached`]. An incarnation the registry does
/// not hold and the table has no identity of is counted as a dropped event.
/// The cache serves one incarnation: it keeps that incarnation's identity once
/// found, so a later event takes no lock and clones nothing.
#[inline]
pub(crate) fn emit_hot(
    record: &AbortRecord,
    system: Option<&ActorSystem>,
    cache: &mut HotCache,
    kind: EventKind,
    build: impl for<'a> FnOnce(&'a ActorRef) -> TelemetryEvent<'a>,
) {
    with_mode(record, system, |mode| {
        if !mode.armed(kind) {
            return;
        }
        let identity = match cache.take_identity() {
            Some(identity) => identity,
            None => match identity_for(mode, record, system, None) {
                Some(identity) => identity,
                None => {
                    mode.pump.count_dropped();
                    return;
                }
            },
        };
        if identity.has_pumped() {
            // An earlier event about this actor is queued or being
            // delivered: this one goes behind it, so a handler sees the
            // actor's events in the order they were raised. Other actors'
            // events in the pump do not matter.
            mode.push_built_owned(&identity, kind, build(identity.actor()).to_static());
        } else {
            let event = build(identity.actor());
            mode.emit_cached(cache, kind, &event);
        }
        cache.keep_identity(identity);
    });
}

/// Sets the label of the incarnation behind `record`, the actor `id`. The
/// label lives in the incarnation's identity, which the registry gives (and
/// records) when the spawn recorded none, so an actor that sets no label
/// holds nothing for it.
pub(crate) fn set_label(
    record: &AbortRecord,
    system: Option<&ActorSystem>,
    id: &ActorId,
    label: Arc<str>,
) {
    let mode = match system {
        Some(system) => Some(system.telemetry_mode()),
        None => record.mode(),
    };
    if let Some(mode) = mode {
        if let Some(identity) = identity_for(mode, record, system, Some(id)) {
            identity.set_label(label);
        }
    }
}

/// The reference of a supervisor: its recorded identity, or the identity the
/// registry gives for the live actor `id` (see [`identity_for`]), or, for one
/// the registry does not hold, its id read as a root with the unknown
/// incarnation number.
pub(crate) fn supervisor_ref(mode: &Mode, record: &AbortRecord, id: &ActorId) -> ActorRef {
    match identity_for(mode, record, None, Some(id)) {
        Some(identity) => identity.actor().clone(),
        None => ActorRef::new_root(id.clone(), UNKNOWN_INCARNATION),
    }
}

/// [`supervisor_ref`] together with the identity it came from, which an event
/// about the supervisor names as its owner so that the supervisor's busy
/// count covers it; `None` for a supervisor the registry does not hold.
pub(crate) fn supervisor_of(
    mode: &Mode,
    record: &AbortRecord,
    id: &ActorId,
) -> (ActorRef, Option<Arc<Identity>>) {
    match identity_for(mode, record, None, Some(id)) {
        Some(identity) => (identity.actor().clone(), Some(identity)),
        None => (ActorRef::new_root(id.clone(), UNKNOWN_INCARNATION), None),
    }
}

/// Queues an event about the incarnation behind `owner`, counted in its busy
/// count, or, for a subject the registry does not hold (`None`), as an event
/// with no owner. This is the one place outside the spawn-refusal event where
/// an event is queued with no owner (`tests/telemetry_sites.rs`).
#[inline]
pub(crate) fn push_for(
    mode: &Arc<Mode>,
    owner: Option<&Arc<Identity>>,
    kind: EventKind,
    build: impl FnOnce() -> TelemetryEvent<'static>,
) {
    match owner {
        Some(owner) => mode.push_owned(owner, kind, build),
        None => mode.push(kind, build),
    }
}

/// The identity fields of the incarnation behind `record` for a log
/// statement: empty when none is recorded.
#[cfg(feature = "tracing")]
pub(crate) fn log_fields(record: &AbortRecord, system: Option<&ActorSystem>) -> LogFields<'static> {
    let found = with_mode(record, system, |mode| {
        mode.find(record.key())
            .map(|ident| (ident, mode.system().map(|system| system.name().to_string())))
    })
    .flatten();
    match found {
        Some((ident, name)) => LogFields {
            system: name.map(std::borrow::Cow::Owned),
            sensitive: record.sys_bits() & crate::actor::subtree::SYS_SENSITIVE != 0,
            ident: Some(ident),
        },
        None => {
            LogFields::of_sensitive(record.sys_bits() & crate::actor::subtree::SYS_SENSITIVE != 0)
        }
    }
}

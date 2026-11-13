//! A waker belongs to the task that polls with it. Where the library wakes
//! wakers that can belong to another task (a user's `wait_stopped`, a stop's
//! wait for the end, a status query, a subtree observer) the wake sits inside
//! a catch, so a panic of that waker never unwinds into the code that made the
//! wake.
//!
//! What this test checks: it reads the library sources, without comments and
//! without test-only code, and fails on any line that calls
//! `.notify_waiters(`, `.notify_one(`, `.wake()`, `.wake_by_ref()`, `.close()`
//! or `.cancel()`, unless the line is listed here with the reason it is safe:
//! it is inside the catch, or every task that can wait on it is one the library
//! spawned and polls with tokio's own waker, or it closes a queue nothing waits
//! to send on. The scan reads text, so it cannot tell which type a call is made
//! on; the reason beside each entry says.
//!
//! What it does not see: a wake that tokio makes inside a call that names none
//! of those methods. These are the sender of a one-shot channel (sent, or
//! dropped), a channel receiver (dropped, or freeing the slot a parked sender
//! takes), a `watch` send and the end of a task. This scan does not see them;
//! the sources name each one. Tests drive the answers, the queued drops and the
//! closes below with a waker that panics:
//!
//! - The answers to a caller's request, status query, spawn, bounded stop and
//!   shutdown are sent through `send_contained`. Wherever the library drops
//!   such a sender unanswered itself, the drop runs inside
//!   `contain_waker_panic`: a status request held outside the system channel,
//!   the spawn ack a `Kill` cut, the answers of the start bound, the stop task
//!   and the shutdown walk, also when the drop runs while the task unwinds.
//! - A task that is aborted drops what its frames still hold under tokio's own
//!   catch. A task that unwinds from a panic of its own drops it during the
//!   unwind, outside any catch, where a second panic ends the process. What a
//!   frame of the actor's task holds for another task's future (the start ack,
//!   the answer held for a failed request, the sender of the request in hand)
//!   is therefore held in an `AnswerHold`, whose drop runs inside
//!   `contain_waker_panic`, and the actor's reason is the one its status plane
//!   carries. The tests of `src/actor/ack_waker_tests.rs` unwind the task with
//!   each of the three held and a caller's waker that panics, in a child
//!   process.
//! - The requests and messages queued when a queue ends are dropped one at a
//!   time inside a catch.
//! - The closes of tokio's channels (the priority lane, the system channel) run
//!   inside `contain_waker_panic`. Tokio ends its wake of the parked senders at
//!   a waker that panics, so the senders behind it may stay unwoken and find
//!   the queue closed at their next poll.
//! - The wake of a sender parked on the system channel or on the priority lane,
//!   made by tokio inside the run loop's receive when it frees a slot, is not
//!   inside a catch: the receive returns the message it took, which a catch
//!   would drop. A panic of that sender's waker unwinds into the run loop's
//!   poll.
//! - A slot of tokio's priority lane that the library holds as a permit, or as
//!   a pending `reserve` future that may already hold one, wakes the sender
//!   parked behind it when it is dropped (a revoked monitor, a cancelled wait,
//!   a timeout, an aborted task). Every such value is held in a `WakeHold`,
//!   whose drop runs inside `contain_waker_panic`. A second scan below fails on
//!   any library line that reserves a slot (borrowed or owned), names or
//!   imports a tokio permit type, sends on a channel (an awaited send of a
//!   bounded `mpsc` holds a reserve inside), or makes or acquires a
//!   `Semaphore`, and is not listed with the reason its drop is contained or
//!   wakes no user's waker. The scan reads text, so a send on a one-shot or an
//!   unbounded channel is listed too, with that reason.
//! - The waits of the one-shot channels, `watch` channels, cancellation tokens
//!   and `JoinHandle`s the library awaits are the waits of tasks the library
//!   polls, or of the polling task's own future.

use std::collections::BTreeMap;
use std::path::Path;

#[path = "common/source_scan.rs"]
mod source_scan;

use source_scan::{library_code, rust_sources, test_only_files};

/// The calls that wake the waiters of a `Notify` or a waker directly, close a
/// channel, whose close wakes the senders parked on it, or cancel a token.
const CALLS: [&str; 6] = [
    ".notify_waiters(",
    ".notify_one(",
    ".wake()",
    ".wake_by_ref()",
    ".close()",
    ".cancel()",
];

/// One allowed library line: the file, the line as written (trimmed, string
/// literals emptied), how many times it appears, and why it is safe.
struct Allowed {
    file: &'static str,
    line: &'static str,
    times: usize,
    why: &'static str,
}

const ALLOWED: &[Allowed] = &[
    Allowed {
        file: "actor/subtree.rs",
        line: r#"contain_waker_panic("", None, || self.shared.signals.notify_waiters());"#,
        times: 1,
        why: "the one wake of the block's signals (stop lane, status plane), inside the catch",
    },
    Allowed {
        file: "types/stop_lane.rs",
        line: r#"contain_waker_panic("", None, move || waker.wake());"#,
        times: 1,
        why: "the one wake of the run loop's waker slot, inside the catch",
    },
    Allowed {
        file: "actor/mailbox.rs",
        line: "self.contain_waker_panic(move || waker.wake());",
        times: 1,
        why: "the mailbox's one wake of a waiter or of the receiver, inside the catch",
    },
    Allowed {
        file: "actor/mailbox.rs",
        line: "self.contain_waker_panic(|| self.closed_notify.notify_waiters());",
        times: 1,
        why: "the close notice, inside the catch; only crate forwarder tasks wait on it",
    },
    Allowed {
        file: "actor/subtree.rs",
        line: "contain_waker_panic(site, None, || self.changed.notify_waiters());",
        times: 1,
        why: "the one wake of the library's own subtree waits (a restart's, terminate_child's and spawn_child's), inside the catch; no user's waker parks on this list",
    },
    Allowed {
        file: "actor/subtree.rs",
        line: "contain_waker_panic(OBSERVERS, None, || self.observers.notify_waiters());",
        times: 1,
        why: "the one wake of the subtree observers' waits, after the library's, inside the catch",
    },
    Allowed {
        file: "actor/subtree.rs",
        line: "self.shared.reader.wake();",
        times: 6,
        why: "the wake slot contains the wake of the run loop's waker itself",
    },
    Allowed {
        file: "types/stop_lane.rs",
        line: "self.aborted.reader().wake();",
        times: 1,
        why: "the wake slot contains the wake of the run loop's waker itself",
    },
    Allowed {
        file: "actor/supervision/slot.rs",
        line: "self.notify.notify_one();",
        times: 1,
        why: "the supervisor's verdict signal; its own run loop is the one waiter",
    },
    Allowed {
        file: "actor/supervision/slot.rs",
        line: "self.verdicts.notify_one();",
        times: 2,
        why: "the supervisor's verdict signal; its own run loop is the one waiter",
    },
    Allowed {
        file: "actor/runtime.rs",
        line: "cx.waker().wake_by_ref();",
        times: 1,
        why: "the polling task wakes its own waker to be polled again",
    },
    Allowed {
        file: "actor/runtime.rs",
        line: "mailbox.close();",
        times: 1,
        why: "the mailbox wakes each waiter of its own wait lists inside a catch",
    },
    Allowed {
        file: "actor/runtime.rs",
        line: r#"contain_waker_panic("", Some(who), || priority.close());"#,
        times: 1,
        why: "the priority lane is tokio's channel: the close wakes the senders parked for room, inside the catch",
    },
    Allowed {
        file: "actor/runtime.rs",
        line: r#"contain_waker_panic("", Some(owner.id().as_who()), || rx.close());"#,
        times: 1,
        why: "the one close of every queue the end drains (the system channel among them), inside the catch",
    },
    Allowed {
        file: "actor/runtime.rs",
        line: r#"contain_waker_panic("", None, move || parent.wake());"#,
        times: 1,
        why: "a teardown wait's wake of the supervisor task that polls the waits, inside the catch",
    },
    Allowed {
        file: "actor/pg_monitor.rs",
        line: "let landed = entry.counts.close();",
        times: 1,
        why: "the counts of a group monitor are one atomic word: closing it sets a bit and wakes no waker",
    },
    Allowed {
        file: "actor/pg_monitor.rs",
        line: "self.counts.close();",
        times: 1,
        why: "the counts of a group monitor are one atomic word: closing it sets a bit and wakes no waker",
    },
    Allowed {
        file: "actor/context.rs",
        line: r#"contain_waker_panic("", None, || ack.close());"#,
        times: 1,
        why: "the close of a start acknowledgment wakes a sender that waits for it, inside the catch",
    },
    Allowed {
        file: "actor/mailbox.rs",
        line: "self.shared.close();",
        times: 2,
        why: "the mailbox's own close, which wakes each waiter inside a catch",
    },
    Allowed {
        file: "actor/mailbox.rs",
        line: "self.queue.close();",
        times: 2,
        why: "the mailbox's queue is unbounded: no sender waits on it",
    },
    Allowed {
        file: "actor/monitor.rs",
        line: "self.token.cancel();",
        times: 1,
        why: "only the library's own monitor entry task waits on the token",
    },
    Allowed {
        file: "actor/link.rs",
        line: "self.token.cancel();",
        times: 1,
        why: "only the library's own link entry task waits on the token",
    },
    Allowed {
        file: "actor/request.rs",
        line: "self.token.cancel();",
        times: 1,
        why: "only the library's own request task waits on the token",
    },
    Allowed {
        file: "actor/link.rs",
        line: "if !state.cancel() {",
        times: 1,
        why: "LinkState::cancel is a compare-exchange on an atomic: it wakes nothing",
    },
    Allowed {
        file: "actor/pg.rs",
        line: r#"contain_waker_panic("", None, || notify.notify_one());"#,
        times: 1,
        why: "the one wake of a group monitor's entry task, inside the catch, after the scope's lock is released; the only waiter on the notification is the library's own entry task",
    },
    Allowed {
        file: "actor/pg_monitor.rs",
        line: "self.token.cancel();",
        times: 1,
        why: "only the library's own group monitor entry task waits on the token",
    },
    Allowed {
        file: "telemetry/pump.rs",
        line: "self.notify.notify_one();",
        times: 3,
        why: "only the crate's pump helper waits on this notification, and a user's waker never parks on it",
    },
    Allowed {
        file: "telemetry/mode.rs",
        line: "self.0.pump.close();",
        times: 1,
        why: "the pump's close sets a flag and wakes the crate's pump helper, the only waiter",
    },
    Allowed {
        file: "telemetry/monitor.rs",
        line: "old.cancel.cancel();",
        times: 1,
        why: "only the library's own monitor helper task waits on this cancellation token",
    },
    Allowed {
        file: "telemetry/monitor.rs",
        line: "removed.cancel.cancel();",
        times: 1,
        why: "only the library's own monitor helper task waits on this cancellation token",
    },
    Allowed {
        file: "actor/context.rs",
        line: "token.cancel();",
        times: 1,
        why: "only the library's own subscription forwarder task waits on this cancellation token",
    },
    Allowed {
        file: "actor/context.rs",
        line: "entry.token.cancel();",
        times: 6,
        why: "only the library's own timer and stream forwarder tasks wait on a cancellation token",
    },
];

/// The calls and types that produce or consume a slot of a bounded channel
/// whose drop wakes the next parked sender: a reserve (a future, or a permit at
/// once, borrowed or owned), tokio's permit types, a send (an awaited send of a
/// bounded `mpsc` holds a reserve inside, and a `send_timeout` too), and the
/// acquires and the type of a `Semaphore`.
const SLOT_TOKENS: [&str; 12] = [
    ".reserve(",
    ".reserve_owned(",
    ".try_reserve(",
    ".try_reserve_owned(",
    "mpsc::Permit",
    "mpsc::OwnedPermit",
    ".send(",
    ".send_timeout(",
    "Semaphore",
    ".acquire(",
    ".acquire_owned(",
    ".try_acquire",
];

/// Whether a line names a tokio permit type without the `mpsc::` path: an
/// import of the type (`use tokio::sync::mpsc::{Permit, ..}`, also one name
/// to a line of a multi-line import).
fn imports_a_permit_type(line: &str) -> bool {
    let name = line.trim_end_matches(',');
    name == "Permit"
        || name == "OwnedPermit"
        || (line.starts_with("use ")
            && line.contains("tokio::")
            && (line.contains("Permit") || line.contains("OwnedPermit")))
}

/// One allowed slot line, as [`Allowed`].
const SLOT_ALLOWED: &[Allowed] = &[
    Allowed {
        file: "actor/handle.rs",
        line: r#"self.tx.reserve().await.ok().map(ForwardPermit::Normal)"#,
        times: 1,
        why: "the normal mailbox is the library's own MailTx: its unit and its pending reserve give back through the mailbox, which wakes each waiter inside a catch",
    },
    Allowed {
        file: "actor/handle.rs",
        line: r#"Edged::new(pin!(self.tx.reserve()), record).await"#,
        times: 1,
        why: "the normal mailbox is the library's own MailTx: its unit and its pending reserve give back through the mailbox, which wakes each waiter inside a catch",
    },
    Allowed {
        file: "actor/handle.rs",
        line: r#"self.tx.reserve().await"#,
        times: 1,
        why: "the normal mailbox is the library's own MailTx: its unit and its pending reserve give back through the mailbox, which wakes each waiter inside a catch",
    },
    Allowed {
        file: "actor/handle.rs",
        line: r#"let mut reserve = pin!(self.tx.reserve());"#,
        times: 2,
        why: "the normal mailbox is the library's own MailTx: its unit and its pending reserve give back through the mailbox, which wakes each waiter inside a catch",
    },
    Allowed {
        file: "actor/handle.rs",
        line: r#") -> Result<WakeHold<mpsc::Permit<'a, ActorEnvelope<A>>>, SendError> {"#,
        times: 1,
        why: "a cast's slot of the priority lane comes from the two functions that reserve one, each of which returns the permit already held in a WakeHold",
    },
    Allowed {
        file: "actor/handle.rs",
        line: r#"let permit = match time::timeout_at(deadline, self.tx.reserve()).await {"#,
        times: 1,
        why: "the normal mailbox is the library's own MailTx: its unit and its pending reserve give back through the mailbox, which wakes each waiter inside a catch",
    },
    Allowed {
        file: "actor/handle.rs",
        line: r#") -> Option<WakeHold<mpsc::Permit<'a, M>>> {"#,
        times: 1,
        why: "the one function that reserves a slot of the priority lane returns the permit already held in a WakeHold",
    },
    Allowed {
        file: "actor/handle.rs",
        line: r#"WakeHold<mpsc::Permit<'a, ActorEnvelope<A>>>,"#,
        times: 1,
        why: "the monitor forwarder's priority permit is held in a WakeHold, so revoking, firing or aborting drops it inside the catch",
    },
    Allowed {
        file: "actor/handle.rs",
        line: r#"_ => self.tx.try_reserve().map(ForwardPermit::Normal),"#,
        times: 1,
        why: "the normal mailbox is the library's own MailTx: a unit taken without waiting gives back through the mailbox, which wakes each waiter inside a catch",
    },
    Allowed {
        file: "actor/alias.rs",
        line: r#"match self.shared.enter(move || permit.send(msg)) {"#,
        times: 1,
        why: "an alias pushes through the permit it holds, under its read guard: the push uses the unit and wakes only the actor's own receiver, and a permit left unused drops after the guard",
    },
    Allowed {
        file: "actor/panic.rs",
        line: r#") -> Result<WakeHold<mpsc::Permit<'a, M>>, mpsc::error::TrySendError<()>> {"#,
        times: 1,
        why: "the one function that reserves a slot of a tokio channel without waiting returns the permit already held in a WakeHold",
    },
    Allowed {
        file: "actor/panic.rs",
        line: r#"lane.try_reserve()"#,
        times: 1,
        why: "the tokio reserve that is not a future; the permit it returns is moved into a WakeHold in the same expression, and a closed or full channel holds nothing",
    },
    Allowed {
        file: "actor/panic.rs",
        line: r#"match lane.try_reserve() {"#,
        times: 1,
        why: "the one tokio reserve that is not a future; the permit it returns is moved into a WakeHold in the same arm, and a closed or full channel holds nothing",
    },
    Allowed {
        file: "actor/panic.rs",
        line: r#"let mut wait = WakeHold::new(site, owner, Box::pin(lane.reserve()));"#,
        times: 1,
        why: "the one pending reserve of a tokio channel in the library, held in a WakeHold, so a cancelled wait, a timeout or an abort drops it inside the catch",
    },
    Allowed {
        file: "actor/panic.rs",
        line: r#") -> Option<WakeHold<mpsc::Permit<'a, M>>> {"#,
        times: 1,
        why: "the one function that reserves a slot of a tokio channel (the priority lane, the status queue) returns the permit already held in a WakeHold",
    },
    Allowed {
        file: "actor/context.rs",
        line: r#"permit.send(message);"#,
        times: 1,
        why: "a timer or stream forwarder pushes through the mailbox permit it holds: the push uses the unit and wakes no sender",
    },
    Allowed {
        file: "actor/context.rs",
        line: r#"permit.send(msg);"#,
        times: 3,
        why: "a timer, stream or subscription forwarder pushes through the mailbox permit it holds: the push uses the unit and wakes no sender",
    },
    Allowed {
        file: "actor/context.rs",
        line: r#"permit.send(tick);"#,
        times: 1,
        why: "a timer forwarder pushes through the mailbox permit it holds: the push uses the unit and wakes no sender",
    },
    Allowed {
        file: "actor/handle.rs",
        line: r#"ForwardPermit::Normal(permit) => permit.send(envelope),"#,
        times: 1,
        why: "the push through a permit of the library's own mailbox uses the unit and wakes no sender",
    },
    Allowed {
        file: "actor/handle.rs",
        line: r#"permit.into_inner().send(Envelope::Message {"#,
        times: 1,
        why: "the permit comes out of its WakeHold with nothing between it and the push; tokio's Permit::send pushes and forgets the permit, so it wakes no sender (the priority lane)",
    },
    Allowed {
        file: "actor/handle.rs",
        line: r#"permit.into_inner().send(envelope);"#,
        times: 1,
        why: "the permit comes out of its WakeHold with nothing between it and the push; tokio's Permit::send pushes and forgets the permit, so it wakes no sender (the priority lane)",
    },
    Allowed {
        file: "actor/handle.rs",
        line: r#"permit.send(Envelope::Message {"#,
        times: 5,
        why: "the push through a permit uses the unit and wakes no sender: three are permits of the library's own mailbox, two are tokio priority permits taken out of their WakeHold at the call with nothing between them and the push in production code (only a test hook sits there)",
    },
    Allowed {
        file: "actor/request.rs",
        line: r#"Some(within) => callee.send_timeout(msg, within).await,"#,
        times: 1,
        why: "the library's own handle method (a mailbox reserve), not a tokio channel send; the request task is a crate task, so the reserve's drop wakes only the library's own waiters",
    },
    Allowed {
        file: "actor/request.rs",
        line: r#"None => callee.send(msg).await,"#,
        times: 1,
        why: "the library's own handle method (a mailbox reserve), not a tokio channel send; the request task is a crate task, so the reserve's drop wakes only the library's own waiters",
    },
    Allowed {
        file: "actor/handle.rs",
        line: r#"return self.send(msg).await;"#,
        times: 1,
        why: "the library's own handle method (a mailbox reserve), not a tokio channel send",
    },
    Allowed {
        file: "actor/handle.rs",
        line: r#"return self.send_timeout(msg, timeout).await;"#,
        times: 1,
        why: "the library's own handle method (a mailbox reserve), not a tokio channel send",
    },
    Allowed {
        file: "actor/mailbox.rs",
        line: r#"if let Err(mpsc::error::SendError(message)) = self.queue.send(message) {"#,
        times: 1,
        why: "the mailbox's queue is unbounded: no sender waits on it",
    },
    Allowed {
        file: "actor/panic.rs",
        line: r#"match catch_unwind(AssertUnwindSafe(move || tx.send(value))) {"#,
        times: 1,
        why: "the send of a one-shot channel, inside a catch: it reserves nothing and parks no sender",
    },
    Allowed {
        file: "actor/runtime.rs",
        line: r#".map(|cap| cap.min(tokio::sync::Semaphore::MAX_PERMITS));"#,
        times: 1,
        why: "the constant only: no semaphore is made or waited on",
    },
    Allowed {
        file: "actor/runtime.rs",
        line: r#"let mailbox_capacity = mailbox_capacity.min(tokio::sync::Semaphore::MAX_PERMITS);"#,
        times: 1,
        why: "the constant only: no semaphore is made or waited on",
    },
    Allowed {
        file: "actor/subtree.rs",
        line: r#".send(crate::types::SystemMessage::Supervise(Box::new(request)));"#,
        times: 1,
        why: "a supervisor reference's request is pushed through the permit that reserve_held returned, with nothing between them; the push forgets the permit and wakes no sender (the system channel)",
    },
    Allowed {
        file: "actor/subtree.rs",
        line: r#"permit.into_inner().send(message);"#,
        times: 1,
        why: "a sys command is pushed through the permit that reserve_held returned, with nothing between them; the push forgets the permit and wakes no sender (the system channel)",
    },
    Allowed {
        file: "actor/subtree.rs",
        line: r#".send(crate::types::SystemMessage::GetStatus(request));"#,
        times: 1,
        why: "the status request is pushed through the permit that reserve_held returned, with nothing between them; the push forgets the permit and wakes no sender (the system channel)",
    },
    Allowed {
        file: "actor/supervision.rs",
        line: r#"if let Err(mpsc::error::SendError(entry)) = self.reaper.send(entry) {"#,
        times: 1,
        why: "the reaper's channel is unbounded: no sender waits on it",
    },
    Allowed {
        file: "system.rs",
        line: r#"if let Err(mpsc::error::SendError(entry)) = self.reaper_handle(rt).send(entry) {"#,
        times: 1,
        why: "the reaper's channel is unbounded: no sender waits on it",
    },
    Allowed {
        file: "system.rs",
        line: r#"let Err(mpsc::error::SendError(entry)) = self.reaper_handle(rt).send(entry) else {"#,
        times: 1,
        why: "the reaper's channel is unbounded: no sender waits on it",
    },
    Allowed {
        file: "system.rs",
        line: r#"report_tx.send(report);"#,
        times: 1,
        why: "the send of a one-shot channel's wrapper: it reserves nothing and parks no sender",
    },
];

/// The library lines that call one of [`CALLS`].
fn wake_lines(source: &str) -> Vec<String> {
    library_code(source)
        .into_iter()
        .filter(|line| CALLS.iter().any(|call| line.contains(call)))
        .collect()
}

#[test]
fn every_library_wake_of_a_waker_is_contained_or_has_a_waiter_the_library_owns() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = rust_sources(&src);
    assert!(sources.len() > 10, "the library sources were found");
    let (test_files, unresolved) = test_only_files(&sources);
    assert!(
        unresolved.is_empty(),
        "test-only modules without a file: {unresolved:?}"
    );

    let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (relative, source) in &sources {
        if test_files.contains(relative) {
            continue;
        }
        for line in wake_lines(source) {
            *seen.entry((relative.clone(), line)).or_default() += 1;
        }
    }

    let mut problems = Vec::new();
    for ((file, line), times) in &seen {
        match ALLOWED
            .iter()
            .find(|allowed| allowed.file == file && allowed.line == line)
        {
            Some(allowed) if allowed.times == *times => {}
            Some(allowed) => problems.push(format!(
                "{file}: `{line}` appears {times} times, expected {} ({})",
                allowed.times, allowed.why
            )),
            None => problems.push(format!(
                "{file}: `{line}` wakes a waker outside a catch and is not listed"
            )),
        }
    }
    for allowed in ALLOWED {
        if !seen.contains_key(&(allowed.file.to_string(), allowed.line.to_string())) {
            problems.push(format!(
                "{}: `{}` is listed and no longer appears",
                allowed.file, allowed.line
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The library lines that reserve a slot or name a tokio permit type.
fn slot_lines(source: &str) -> Vec<String> {
    library_code(source)
        .into_iter()
        .filter(|line| {
            SLOT_TOKENS.iter().any(|token| line.contains(token)) || imports_a_permit_type(line)
        })
        .collect()
}

#[test]
fn every_slot_of_a_bounded_channel_is_held_where_its_drop_is_contained() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = rust_sources(&src);
    let (test_files, unresolved) = test_only_files(&sources);
    assert!(
        unresolved.is_empty(),
        "test-only modules without a file: {unresolved:?}"
    );

    let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (relative, source) in &sources {
        if test_files.contains(relative) {
            continue;
        }
        for line in slot_lines(source) {
            *seen.entry((relative.clone(), line)).or_default() += 1;
        }
    }

    let mut problems = Vec::new();
    for ((file, line), times) in &seen {
        match SLOT_ALLOWED
            .iter()
            .find(|allowed| allowed.file == file && allowed.line == line)
        {
            Some(allowed) if allowed.times == *times => {}
            Some(allowed) => problems.push(format!(
                "{file}: `{line}` appears {times} times, expected {} ({})",
                allowed.times, allowed.why
            )),
            None => problems.push(format!(
                "{file}: `{line}` holds a slot of a bounded channel outside a catch and is not listed"
            )),
        }
    }
    for allowed in SLOT_ALLOWED {
        if !seen.contains_key(&(allowed.file.to_string(), allowed.line.to_string())) {
            problems.push(format!(
                "{}: `{}` is listed and no longer appears",
                allowed.file, allowed.line
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_slot_scan_finds_a_permit_type_imported_by_name() {
    let source = r#"
use tokio::sync::mpsc::{self, Permit};
use tokio::sync::mpsc::OwnedPermit;
use tokio::sync::{
    mpsc::{
        Permit,
    },
};
use crate::mailbox::Permit as Mine;
fn named(held: Permit<'_, u8>) {}
"#;
    let lines = slot_lines(source);
    assert!(lines.contains(&"use tokio::sync::mpsc::{self, Permit};".to_string()));
    assert!(lines.contains(&"use tokio::sync::mpsc::OwnedPermit;".to_string()));
    assert!(lines.contains(&"Permit,".to_string()));
    assert!(!lines.iter().any(|line| line.contains("crate::mailbox")));
}

#[test]
fn the_slot_scan_finds_reserves_and_permit_types() {
    let source = r#"
fn holds(lane: &Sender<u8>) {
    let wait = lane.reserve();
    let permit = lane.try_reserve();
    let owned = lane.clone().reserve_owned();
    let held: mpsc::Permit<'_, u8>;
    let also: mpsc::OwnedPermit<u8>;
    let owned_now = lane.clone().try_reserve_owned();
    lane.send(1).await;
    lane.send_timeout(1, wait).await;
    let sem = Semaphore::new(1);
    let one = sem.acquire().await;
    let two = sem.clone().acquire_owned().await;
    let three = sem.try_acquire();
}

#[cfg(test)]
fn only_in_tests(lane: &Sender<u8>) {
    lane.reserve();
}
"#;
    assert_eq!(
        slot_lines(source),
        vec![
            "let wait = lane.reserve();".to_string(),
            "let permit = lane.try_reserve();".to_string(),
            "let owned = lane.clone().reserve_owned();".to_string(),
            "let held: mpsc::Permit<'_, u8>;".to_string(),
            "let also: mpsc::OwnedPermit<u8>;".to_string(),
            "let owned_now = lane.clone().try_reserve_owned();".to_string(),
            "lane.send(1).await;".to_string(),
            "lane.send_timeout(1, wait).await;".to_string(),
            "let sem = Semaphore::new(1);".to_string(),
            "let one = sem.acquire().await;".to_string(),
            "let two = sem.clone().acquire_owned().await;".to_string(),
            "let three = sem.try_acquire();".to_string(),
        ]
    );
}

#[test]
fn the_scan_finds_the_wake_calls_and_skips_test_only_code() {
    let source = r#"
fn wakes(slot: &Slot, notify: &Notify) {
    slot.waker.wake(); // a waker
    notify.notify_waiters();
    notify.notify_one();
    cx.waker().wake_by_ref();
    lane.close();
    token.cancel();
}

/// Docs that mention .notify_waiters() are not code.
#[cfg(test)]
fn only_in_tests(notify: &Notify) {
    notify.notify_waiters();
}

fn a_literal() -> &'static str {
    ".wake()"
}
"#;
    assert_eq!(
        wake_lines(source),
        vec![
            "slot.waker.wake();".to_string(),
            "notify.notify_waiters();".to_string(),
            "notify.notify_one();".to_string(),
            "cx.waker().wake_by_ref();".to_string(),
            "lane.close();".to_string(),
            "token.cancel();".to_string(),
        ]
    );
}

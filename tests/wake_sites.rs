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
        line: "self.shared.recorded.notify_waiters();",
        times: 1,
        why: "no library task waits on the abort record's notification",
    },
    Allowed {
        file: "actor/subtree.rs",
        line: "self.shared.reader.wake();",
        times: 2,
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
        times: 1,
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
        line: r#"contain_waker_panic("", Some(ctx.actor_id()), || priority.close());"#,
        times: 1,
        why: "the priority lane is tokio's channel: the close wakes the senders parked for room, inside the catch",
    },
    Allowed {
        file: "actor/runtime.rs",
        line: r#"contain_waker_panic("", Some(owner), || rx.close());"#,
        times: 1,
        why: "the one close of every queue the end drains (the system channel among them), inside the catch",
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
        file: "actor/context.rs",
        line: "entry.token.cancel();",
        times: 6,
        why: "only the library's own timer and stream forwarder tasks wait on a cancellation token",
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

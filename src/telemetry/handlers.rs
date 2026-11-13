//! Handlers attached to a system by id.

use std::borrow::Cow;
use std::cell::Cell;
use std::fmt;
use std::sync::{Arc, MutexGuard};

use super::event::{EventKind, KindSet, TelemetryEvent};
use super::mode::Mode;
use super::sinks::SinkTables;
use crate::actor::panic::{catch_sync, payload_into_string, PanicPayload};

/// One attached handler.
#[derive(Clone)]
pub(crate) struct HandlerEntry {
    pub(crate) id: Arc<str>,
    pub(crate) kinds: KindSet,
    pub(crate) f: Arc<dyn Fn(&TelemetryEvent<'_>) + Send + Sync>,
}

impl fmt::Debug for HandlerEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HandlerEntry")
            .field("id", &self.id)
            .field("kinds", &self.kinds)
            .finish_non_exhaustive()
    }
}

thread_local! {
    /// The identity of the mode whose handler runs on this thread now, or 0
    /// when none does. It is compared by address and never dereferenced, so
    /// an event raised from inside a handler of one system is skipped only by
    /// that system's gate and a handler of another system never makes it skip.
    static RUNNING: Cell<usize> = const { Cell::new(0) };
}

/// The identity of `mode` in the running mark.
fn identity(mode: &Mode) -> usize {
    std::ptr::from_ref(mode) as usize
}

/// Whether a handler of `mode` runs on this thread now. A handler of another
/// system running here does not count.
pub(crate) fn inside_handler_of(mode: &Mode) -> bool {
    RUNNING
        .try_with(|running| running.get() == identity(mode))
        .unwrap_or(false)
}

/// Marks the thread as inside a handler of one mode; puts back the mark it
/// found when dropped, so a handler of another system that runs nested on this
/// thread leaves the outer mark as it was.
struct Running {
    previous: usize,
}

impl Running {
    /// The mark for `mode`, or `None` when a handler of `mode` already runs
    /// on this thread or the thread's locals are gone (thread teardown).
    fn enter(mode: &Mode) -> Option<Running> {
        let me = identity(mode);
        RUNNING
            .try_with(|running| {
                let previous = running.get();
                if previous == me {
                    None
                } else {
                    running.set(me);
                    Some(Running { previous })
                }
            })
            .ok()
            .flatten()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = RUNNING.try_with(|running| running.set(self.previous));
    }
}

pub(super) fn lock(mode: &Mode) -> MutexGuard<'_, SinkTables> {
    mode.sinks
        .tables
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Drops `value` inside a catch. The handlers' captured state is user code, so
/// its drop never unwinds into the caller; the panic is logged with the
/// `tracing` feature.
pub(crate) fn drop_contained<T>(value: T) {
    if let Err(payload) = catch_sync(move || drop(value)) {
        let text = payload_into_string(payload);
        #[cfg(feature = "tracing")]
        caught_log!(
            warn,
            panic = %crate::telemetry::LogFields::foreign().panic_text(&text),
            "the state a telemetry handler captured panicked when it was dropped; the panic is contained"
        );
        let _ = text;
    }
}

/// Runs every handler in `entries` that takes `kind`, one after another, each
/// under a catch. A handler that panics is detached and `HandlerDetached` goes
/// to the handlers that remain. An event delivered while a handler of the same
/// system runs on this thread does not reach the handlers: it is counted as
/// dropped, once. No crate lock is held while a handler runs, and `entries` is
/// dropped inside a catch, since it may hold the last reference to a handler
/// detached meanwhile.
pub(crate) fn run(
    mode: &Mode,
    entries: Arc<[HandlerEntry]>,
    kind: EventKind,
    event: &TelemetryEvent<'static>,
) {
    run_borrowed(mode, &entries, kind, event);
    drop_contained(entries);
}

/// The body of [`run`] over a borrowed list: the entries are not dropped
/// here, so a caller that keeps the list between events can reuse it.
pub(crate) fn run_borrowed(
    mode: &Mode,
    entries: &[HandlerEntry],
    kind: EventKind,
    event: &TelemetryEvent<'_>,
) {
    for entry in entries {
        if !entry.kinds.contains(kind) {
            continue;
        }
        let Some(running) = Running::enter(mode) else {
            // A handler of this system already runs on this thread (or the
            // thread is ending): the handlers do not run for this event, and
            // the skip is counted so the report to the handlers holds it.
            mode.pump.count_dropped();
            break;
        };
        let outcome = catch_sync(|| (entry.f)(event));
        drop(running);
        if let Err(payload) = outcome {
            detach_panicked(mode, entry, payload);
        }
    }
}

/// Detaches the handler `entry`, which panicked with `payload`, and tells the
/// remaining handlers once. A handler that was detached meanwhile (by id or by
/// another call's panic) is not reported twice, and an entry that has since
/// been replaced under the same id is not touched.
fn detach_panicked(mode: &Mode, entry: &HandlerEntry, payload: PanicPayload) {
    let text = payload_into_string(payload);
    #[cfg(feature = "tracing")]
    caught_log!(
        warn,
        handler = %entry.id,
        panic = %crate::telemetry::LogFields::foreign().panic_text(&text),
        "a telemetry handler panicked and is detached; the panic is contained"
    );
    let removed = remove_where(mode, |e| e.id == entry.id && Arc::ptr_eq(&e.f, &entry.f));
    if removed {
        mode.dispatch(EventKind::HandlerDetached, || {
            TelemetryEvent::HandlerDetached {
                handler: Cow::Owned(entry.id.to_string()),
                panic: Cow::Owned(text),
            }
        });
    }
}

/// Removes the first entry `matches` accepts; `false` when none does. The
/// removed entry, and with it the handler's captured state, drops after the
/// lock is released, inside a catch.
fn remove_where(mode: &Mode, matches: impl Fn(&HandlerEntry) -> bool) -> bool {
    let old = {
        let mut tables = lock(mode);
        let Some(at) = tables.handlers.iter().position(matches) else {
            return false;
        };
        let mut next = tables.handlers.to_vec();
        next.remove(at);
        let old = std::mem::replace(&mut tables.handlers, Arc::from(next));
        tables.handler_gen = tables.handler_gen.wrapping_add(1);
        mode.recompute(&tables);
        old
    };
    drop_contained(old);
    true
}

/// Attaches `f` under `id` for the events of `kinds`; `false`, with nothing
/// attached, when `id` is taken. Effective from the next event.
pub(crate) fn attach(
    mode: &Mode,
    id: &str,
    kinds: KindSet,
    f: Arc<dyn Fn(&TelemetryEvent<'_>) + Send + Sync>,
) -> bool {
    let outcome = {
        let mut tables = lock(mode);
        if tables.handlers.iter().any(|e| &*e.id == id) {
            Err(f)
        } else {
            let mut next = tables.handlers.to_vec();
            next.push(HandlerEntry {
                id: Arc::from(id),
                kinds,
                f,
            });
            let old = std::mem::replace(&mut tables.handlers, Arc::from(next));
            tables.handler_gen = tables.handler_gen.wrapping_add(1);
            mode.recompute(&tables);
            Ok(old)
        }
    };
    match outcome {
        Ok(old) => {
            drop_contained(old);
            true
        }
        Err(rejected) => {
            drop_contained(rejected);
            false
        }
    }
}

/// Detaches the handler attached under `id`; `false` when there is none. A
/// call already running that handler finishes; no later call reaches it.
pub(crate) fn detach(mode: &Mode, id: &str) -> bool {
    remove_where(mode, |e| &*e.id == id)
}

/// The number of changes made to the handler list.
#[cfg(test)]
pub(crate) fn generation(mode: &Mode) -> u64 {
    lock(mode).handler_gen
}

#[cfg(test)]
#[path = "handlers_tests.rs"]
mod tests;

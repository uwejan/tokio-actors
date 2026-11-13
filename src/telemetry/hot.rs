//! Delivery of events raised on the path of an actor's own task: a per-loop
//! copy of the handler list, refreshed when the registry changes.

use std::sync::{Arc, Weak};

use super::event::{EventKind, TelemetryEvent};
use super::handlers::{self, HandlerEntry};
use super::identity::Identity;
use super::mode::Mode;

/// A loop's copy of the handler list and the generation it was read at. The
/// copy is made by the first armed event, so an actor that never raises a hot
/// event holds one pointer and no list.
#[derive(Debug)]
pub(crate) struct HotCache {
    inner: Option<Box<Held>>,
}

/// What the first armed event reads.
#[derive(Debug)]
struct Held {
    /// The mode the list belongs to: where the list goes when the cache ends.
    mode: Weak<Mode>,
    generation: u64,
    /// `None` only while the list is being handed to its drop.
    entries: Option<Arc<[HandlerEntry]>>,
    /// The identity of the incarnation the cache serves, kept by the first
    /// event that found it.
    identity: Option<Arc<Identity>>,
    #[cfg(test)]
    refreshes: u32,
}

impl HotCache {
    /// An empty cache: the first armed event reads the list.
    pub(crate) fn new() -> Self {
        Self { inner: None }
    }

    /// How many times the list was read from the registry.
    #[cfg(test)]
    pub(crate) fn refreshes_for_test(&self) -> u32 {
        self.inner.as_ref().map_or(0, |held| held.refreshes)
    }

    /// Takes the identity the cache kept, if it kept one.
    pub(super) fn take_identity(&mut self) -> Option<Arc<Identity>> {
        self.inner.as_mut()?.identity.take()
    }

    /// Keeps `identity` for the next event, once the list is held.
    pub(super) fn keep_identity(&mut self, identity: Arc<Identity>) {
        if let Some(held) = self.inner.as_mut() {
            held.identity = Some(identity);
        }
    }
}

impl Drop for Held {
    /// The list may hold the last reference to a handler detached meanwhile,
    /// and a cache ends on a path that still owes the crate steps (the end of
    /// a run loop, the drop of a cancelled task), so the list is handed to the
    /// pump, which drops it on its helper. Only a mode that has no pump left
    /// to take it drops it here, inside a catch.
    fn drop(&mut self) {
        let Some(entries) = self.entries.take() else {
            return;
        };
        match self.mode.upgrade() {
            Some(mode) => mode.pump.release(&mode, entries),
            None => handlers::drop_contained(entries),
        }
    }
}

impl Mode {
    /// Delivers `event` to the handlers that take `kind`, on the calling
    /// thread, from `cache`'s copy of the list. The registry mutex is taken
    /// only when the generation moved since the cache was read. An event
    /// raised from inside a handler of this system is not delivered: it is
    /// counted as dropped. Never waits for a lock held across user code.
    pub(crate) fn emit_cached(
        self: &Arc<Self>,
        cache: &mut HotCache,
        kind: EventKind,
        event: &TelemetryEvent<'_>,
    ) {
        if handlers::inside_handler_of(self) {
            self.pump.count_dropped();
            return;
        }
        let held = match &mut cache.inner {
            Some(held) => {
                if held.generation != self.generation() {
                    self.refresh(held);
                }
                held
            }
            empty => empty.insert(self.first_read()),
        };
        if let Some(entries) = &held.entries {
            handlers::run_borrowed(self, entries, kind, event);
        }
        #[cfg(test)]
        self.sinks.deliver_test(kind, &event.to_static());
    }

    /// The handler list and the generation it belongs to, read under the
    /// registry mutex.
    fn read_list(&self) -> (Arc<[HandlerEntry]>, u64) {
        let tables = handlers::lock(self);
        (tables.handlers.clone(), self.generation())
    }

    /// The first read of a cache.
    fn first_read(self: &Arc<Self>) -> Box<Held> {
        let (entries, generation) = self.read_list();
        Box::new(Held {
            mode: Arc::downgrade(self),
            generation,
            entries: Some(entries),
            identity: None,
            #[cfg(test)]
            refreshes: 1,
        })
    }

    /// Reads the list again; the one it replaces drops after the mutex is
    /// released.
    fn refresh(&self, held: &mut Held) {
        let (entries, generation) = self.read_list();
        held.generation = generation;
        #[cfg(test)]
        {
            held.refreshes += 1;
        }
        if let Some(old) = held.entries.replace(entries) {
            handlers::drop_contained(old);
        }
    }
}

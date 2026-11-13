//! The registries of handlers and subscriptions, and delivery to them.

use std::sync::{Arc, Mutex};

use super::event::{EventKind, TelemetryEvent};
use super::handlers::{self, HandlerEntry};
use super::mode::Mode;
use super::monitor::MonitorSlot;
use super::subscription::{self, SubEntry};
#[cfg(test)]
use test_sink::TestEntry;

/// Both registries behind one mutex.
#[derive(Debug, Default)]
pub(crate) struct Sinks {
    pub(crate) tables: Mutex<SinkTables>,
    /// The last subscription id handed out.
    pub(crate) next_sub: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    test: Mutex<Vec<TestEntry>>,
    #[cfg(test)]
    test_seq: std::sync::atomic::AtomicU64,
}

/// What the registry mutex guards: immutable snapshots, replaced on change.
#[derive(Debug)]
pub(crate) struct SinkTables {
    pub(crate) handlers: Arc<[HandlerEntry]>,
    pub(crate) subs: Arc<[SubEntry]>,
    pub(crate) handler_gen: u64,
    /// The system monitor, when the system has one.
    pub(crate) monitor: Option<MonitorSlot>,
}

impl Default for SinkTables {
    fn default() -> Self {
        Self {
            handlers: Arc::from(Vec::new()),
            subs: Arc::from(Vec::new()),
            handler_gen: 0,
            monitor: None,
        }
    }
}

impl Drop for SinkTables {
    /// The last holder of a system's telemetry state may be a handle's drop, an
    /// actor's end or the pump task. The attached handlers, and the state
    /// they captured, are user code, so their drop runs inside a catch.
    fn drop(&mut self) {
        let handlers = std::mem::replace(&mut self.handlers, Arc::from(Vec::new()));
        handlers::drop_contained(handlers);
    }
}

impl Sinks {
    /// Delivers `event` to the subscriptions, then the handlers, then the
    /// test sinks. The registry lock is released before any of them runs.
    pub(crate) fn deliver(&self, mode: &Mode, kind: EventKind, event: TelemetryEvent<'static>) {
        let (handlers, subs) = {
            let tables = self
                .tables
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (tables.handlers.clone(), tables.subs.clone())
        };
        let event = Arc::new(event);
        subscription::deliver(&subs, kind, &event);
        handlers::run(mode, handlers, kind, &event);
        #[cfg(test)]
        self.deliver_test(kind, &event);
    }
}

#[cfg(test)]
pub(crate) mod test_sink {
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    use super::super::event::{EventKind, KindSet, TelemetryEvent};
    use super::super::mode::Mode;
    use super::Sinks;

    type TestFn = Arc<dyn Fn(&TelemetryEvent<'static>) + Send + Sync>;

    pub(crate) struct TestEntry {
        id: u64,
        kinds: KindSet,
        f: TestFn,
    }

    impl std::fmt::Debug for TestEntry {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TestEntry")
                .field("id", &self.id)
                .finish_non_exhaustive()
        }
    }

    impl Sinks {
        pub(crate) fn test_bits(&self) -> u64 {
            let entries = self.test.lock().unwrap_or_else(|p| p.into_inner());
            entries.iter().fold(0, |bits, e| bits | e.kinds.bits())
        }

        pub(crate) fn deliver_test(&self, kind: EventKind, event: &TelemetryEvent<'static>) {
            let fns: Vec<TestFn> = {
                let entries = self.test.lock().unwrap_or_else(|p| p.into_inner());
                entries
                    .iter()
                    .filter(|e| e.kinds.contains(kind))
                    .map(|e| e.f.clone())
                    .collect()
            };
            for f in fns {
                f(event);
            }
        }
    }

    /// Arms a test sink for as long as it lives.
    #[derive(Debug)]
    pub(crate) struct TestSinkGuard {
        mode: Arc<Mode>,
        id: u64,
        events: Arc<Mutex<Vec<TelemetryEvent<'static>>>>,
    }

    impl TestSinkGuard {
        /// The events a collecting sink has seen, in order.
        pub(crate) fn events(&self) -> Vec<TelemetryEvent<'static>> {
            self.events
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        }
    }

    impl Drop for TestSinkGuard {
        fn drop(&mut self) {
            let tables = self
                .mode
                .sinks
                .tables
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            self.mode
                .sinks
                .test
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|e| e.id != self.id);
            self.mode.recompute(&tables);
        }
    }

    impl Mode {
        /// Arms `kinds` and calls `f` for each event of those kinds.
        pub(crate) fn test_sink(
            self: &Arc<Self>,
            kinds: KindSet,
            f: impl Fn(&TelemetryEvent<'static>) + Send + Sync + 'static,
        ) -> TestSinkGuard {
            self.add_test(kinds, Arc::new(f), Arc::default())
        }

        /// Arms `kinds` and collects the events of those kinds.
        pub(crate) fn collect(self: &Arc<Self>, kinds: KindSet) -> TestSinkGuard {
            let events: Arc<Mutex<Vec<TelemetryEvent<'static>>>> = Arc::default();
            let sink = events.clone();
            let f = move |event: &TelemetryEvent<'static>| {
                sink.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(event.clone());
            };
            self.add_test(kinds, Arc::new(f), events)
        }

        fn add_test(
            self: &Arc<Self>,
            kinds: KindSet,
            f: TestFn,
            events: Arc<Mutex<Vec<TelemetryEvent<'static>>>>,
        ) -> TestSinkGuard {
            let id = self.sinks.test_seq.fetch_add(1, Ordering::Relaxed);
            let tables = self.sinks.tables.lock().unwrap_or_else(|p| p.into_inner());
            self.sinks
                .test
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(TestEntry { id, kinds, f });
            self.recompute(&tables);
            drop(tables);
            self.engage_pass();
            TestSinkGuard {
                mode: self.clone(),
                id,
                events,
            }
        }
    }
}

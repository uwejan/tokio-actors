#![allow(dead_code)]
//! A process-wide `tracing` subscriber that keeps the events of the crate's
//! targets, for the tests of the log lines. One subscriber serves every test
//! of a binary; a test finds its own events by the ids of its actors.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tracing::field::{Field, Visit};
use tracing::{span, Event, Metadata, Subscriber};

/// One event: its target, its level and its fields as text.
#[derive(Clone, Debug)]
pub struct Seen {
    pub target: String,
    pub level: String,
    pub fields: Vec<(String, String)>,
}

impl Seen {
    /// The text of the field `name`.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }

    /// Every field and the message, as one line.
    pub fn line(&self) -> String {
        let mut line = format!("{} {}", self.level, self.target);
        for (name, value) in &self.fields {
            let _ = write!(line, " {name}={value}");
        }
        line
    }
}

struct Collect(Vec<(String, String)>);

impl Visit for Collect {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push((field.name().to_string(), value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .push((field.name().to_string(), format!("{value:?}")));
    }
}

struct Capture {
    /// The targets that no event is kept for.
    muted: &'static [&'static str],
    seen: Mutex<Vec<Seen>>,
    next_span: AtomicU64,
}

impl Subscriber for Capture {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        let target = metadata.target();
        target.starts_with("tokio_actors") && !self.muted.contains(&target)
    }

    fn new_span(&self, _attrs: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut collect = Collect(Vec::new());
        event.record(&mut collect);
        self.seen.lock().unwrap().push(Seen {
            target: event.metadata().target().to_string(),
            level: event.metadata().level().to_string(),
            fields: collect.0,
        });
    }

    fn enter(&self, _span: &span::Id) {}

    fn exit(&self, _span: &span::Id) {}
}

static CAPTURE: OnceLock<&'static Capture> = OnceLock::new();

/// Installs the process's subscriber (once) with no target muted.
pub fn install() {
    install_muting(&[]);
}

/// Installs the process's subscriber (once); no event of a target in
/// `muted` is enabled, so no `enabled` check of the crate sees it.
pub fn install_muting(muted: &'static [&'static str]) {
    CAPTURE.get_or_init(|| {
        let capture: &'static Capture = Box::leak(Box::new(Capture {
            muted,
            seen: Mutex::new(Vec::new()),
            next_span: AtomicU64::new(0),
        }));
        struct Handle(&'static Capture);
        impl Subscriber for Handle {
            fn enabled(&self, m: &Metadata<'_>) -> bool {
                self.0.enabled(m)
            }
            fn new_span(&self, a: &span::Attributes<'_>) -> span::Id {
                self.0.new_span(a)
            }
            fn record(&self, s: &span::Id, v: &span::Record<'_>) {
                self.0.record(s, v)
            }
            fn record_follows_from(&self, s: &span::Id, f: &span::Id) {
                self.0.record_follows_from(s, f)
            }
            fn event(&self, e: &Event<'_>) {
                self.0.event(e)
            }
            fn enter(&self, s: &span::Id) {
                self.0.enter(s)
            }
            fn exit(&self, s: &span::Id) {
                self.0.exit(s)
            }
        }
        tracing::subscriber::set_global_default(Handle(capture))
            .expect("no other global subscriber is installed in this process");
        capture
    });
}

/// Every event kept so far.
pub fn all() -> Vec<Seen> {
    CAPTURE
        .get()
        .expect("the subscriber is installed")
        .seen
        .lock()
        .unwrap()
        .clone()
}

/// The events for which `keep` holds.
pub fn events(keep: impl Fn(&Seen) -> bool) -> Vec<Seen> {
    all().into_iter().filter(|seen| keep(seen)).collect()
}

/// Whether the end line of the actor `id` (written after its crash report) has
/// been kept.
pub fn ended(id: &str) -> bool {
    all().iter().any(|seen| {
        seen.field("message") == Some("Actor stopped") && seen.field("actor_id") == Some(id)
    })
}

/// Waits until `cond` holds; the bound only limits how long a hang takes to
/// report.
pub async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

//! Subscriptions: filters, queues and what a watcher receives.

use std::sync::Arc;

use super::event::{KindSet, TelemetryEvent};
use crate::types::ActorId;

/// The largest queue a subscription may ask for.
pub const MAX_SUBSCRIPTION_QUEUE: usize = 4096;

/// The queue length a subscription gets when it asks for none.
const DEFAULT_SUBSCRIPTION_QUEUE: usize = 64;

/// The id of one subscription.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SubscriptionId(pub(crate) u64);

/// Whose events a subscription receives.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Scope {
    /// Every actor of the system, and the events that name no actor.
    System,
    /// The events about one actor id, across its incarnations.
    Actor(ActorId),
    /// The events about one actor id and every actor below it.
    Subtree(ActorId),
}

/// Which events a subscription receives, and how many it may queue.
#[derive(Clone, Debug)]
pub struct Filter {
    pub(crate) scope: Scope,
    pub(crate) kinds: KindSet,
    pub(crate) queue: usize,
}

impl Filter {
    /// A filter for `scope` with every cold kind and the default queue. A
    /// subscription never receives a hot kind, whatever its filter names:
    /// hot kinds go to handlers only.
    pub fn new(scope: Scope) -> Self {
        Self {
            scope,
            kinds: KindSet::cold(),
            queue: DEFAULT_SUBSCRIPTION_QUEUE,
        }
    }

    /// Limits the filter to `kinds`.
    pub fn kinds(mut self, kinds: KindSet) -> Self {
        self.kinds = kinds;
        self
    }

    /// Sets the queue length, clamped to `1..=MAX_SUBSCRIPTION_QUEUE`.
    pub fn queue(mut self, queue: usize) -> Self {
        self.queue = queue.clamp(1, MAX_SUBSCRIPTION_QUEUE);
        self
    }
}

/// One event handed to a watcher, with the count of events its queue lost
/// before this one.
#[derive(Clone, Debug)]
pub struct Delivery {
    /// The event.
    pub event: Arc<TelemetryEvent<'static>>,
    /// Events dropped from the queue since the previous delivery.
    pub missed_before: u64,
}

/// A notice a subscription receives about itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscriptionNotice {
    /// The queue overflowed and `missed` events were dropped.
    Overflow {
        /// The number of events dropped.
        missed: u64,
    },
    /// The watcher's map function panicked.
    MapPanicked,
}
// The parts of this module, in the order the generated docs list their methods.

#[cfg(feature = "telemetry")]
mod registry;
#[cfg(feature = "telemetry")]
pub(crate) use self::registry::*;

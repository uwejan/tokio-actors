//! The identity of one incarnation of an actor, with its ancestors.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::types::ActorId;

/// The incarnation number of a supervisor whose spawn recorded nothing: only
/// its id is known, and no system draws this number.
pub(crate) const UNKNOWN_INCARNATION: u64 = u64::MAX;

/// One incarnation of an actor: its id, its incarnation number and the chain
/// of its supervisors up to the root of its tree.
#[derive(Clone, Debug)]
pub struct ActorRef {
    id: ActorId,
    incarnation: u64,
    ancestry: Ancestry,
}

/// The ancestors of an incarnation, parent first; cloning shares the chain.
#[derive(Clone, Debug, Default)]
pub(crate) struct Ancestry(Option<Arc<AncestorNode>>);

#[derive(Debug)]
pub(crate) struct AncestorNode {
    id: ActorId,
    incarnation: u64,
    next: Ancestry,
}

/// The ancestors of an [`ActorRef`], parent first.
#[derive(Debug)]
struct Ancestors<'a> {
    next: Option<&'a AncestorNode>,
}

impl<'a> Iterator for Ancestors<'a> {
    type Item = (&'a ActorId, u64);

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.next?;
        self.next = node.next.0.as_deref();
        Some((&node.id, node.incarnation))
    }
}

impl ActorRef {
    /// A reference to an incarnation with no ancestors.
    pub(crate) fn new_root(id: ActorId, incarnation: u64) -> Self {
        Self {
            id,
            incarnation,
            ancestry: Ancestry(None),
        }
    }

    /// A reference to a child incarnation of `parent`.
    pub(crate) fn new_child(id: ActorId, incarnation: u64, parent: &ActorRef) -> Self {
        let node = AncestorNode {
            id: parent.id.clone(),
            incarnation: parent.incarnation,
            next: parent.ancestry.clone(),
        };
        Self {
            id,
            incarnation,
            ancestry: Ancestry(Some(Arc::new(node))),
        }
    }

    /// The reference of a child of `parent` where only the child's ids are
    /// known.
    pub(crate) fn for_child_of(parent: &ActorRef, id: ActorId, incarnation: u64) -> Self {
        Self::new_child(id, incarnation, parent)
    }

    /// The reference of the supervisor above this incarnation, with its own
    /// ancestry.
    pub(crate) fn parent(&self) -> Option<ActorRef> {
        self.ancestry.0.as_deref().map(|node| ActorRef {
            id: node.id.clone(),
            incarnation: node.incarnation,
            ancestry: node.next.clone(),
        })
    }

    /// The actor's id.
    pub fn id(&self) -> &ActorId {
        &self.id
    }

    /// The incarnation number of the actor: never 0, and unique among the
    /// incarnations of every system in the process. A supervisor the system does
    /// not hold when the event is raised is named with `u64::MAX`, a number no
    /// system draws.
    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }

    /// The supervisors above this incarnation as (id, incarnation), parent
    /// first; a supervisor the system does not hold has the incarnation
    /// `u64::MAX`.
    pub fn ancestors(&self) -> impl Iterator<Item = (&ActorId, u64)> {
        Ancestors {
            next: self.ancestry.0.as_deref(),
        }
    }

    /// Whether `id` is the id of one of this incarnation's ancestors.
    pub fn is_descendant_of(&self, id: &ActorId) -> bool {
        self.ancestors().any(|(ancestor, _)| ancestor == id)
    }
}

impl PartialEq for ActorRef {
    fn eq(&self, other: &Self) -> bool {
        self.incarnation == other.incarnation && self.id == other.id
    }
}

impl Eq for ActorRef {}

impl Hash for ActorRef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.incarnation.hash(state);
    }
}

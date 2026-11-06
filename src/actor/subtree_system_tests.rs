//! The system plane in isolation: the order it answers requests in, how many
//! it queues, and what becomes of the requests and the reserved slots when
//! the run loop ends it. Nothing here is ordered by a sleep.

use std::future::Future;
use std::task::{Context, Poll, Waker};

use tokio::sync::oneshot;

use super::*;
use crate::types::{ActorId, ActorStatusInfo, StatusRequest, SystemMessage};

fn new_plane() -> (AbortRecord, SystemPlane) {
    let record = AbortRecord::new();
    let system = crate::system::ActorSystem::create(format!("plane-{}", uuid::Uuid::new_v4()))
        .expect("a fresh system");
    let plane = SystemPlane::new(
        record.clone(),
        ActorId::from("system-plane".to_string()),
        system,
    );
    (record, plane)
}

/// Polls `submit_status` once: whether the request was queued without
/// waiting. A request that has to wait is dropped, so it queues nothing.
fn submit_now(record: &AbortRecord, reply: oneshot::Sender<ActorStatusInfo>) -> bool {
    let mut cx = Context::from_waker(Waker::noop());
    let mut submit = std::pin::pin!(record.submit_status(reply));
    matches!(submit.as_mut().poll(&mut cx), Poll::Ready(Ok(())))
}

/// Takes the next request now, when one is ready.
fn take_request_now(plane: &mut SystemPlane) -> Option<StatusRequest> {
    let mut cx = Context::from_waker(Waker::noop());
    match plane.poll_recv(&mut cx) {
        Poll::Ready(Some(SystemMessage::GetStatus(request))) => Some(request),
        Poll::Ready(None) | Poll::Pending => None,
    }
}

/// Takes the next request's reply now, when one is ready.
fn take_now(plane: &mut SystemPlane) -> Option<oneshot::Sender<ActorStatusInfo>> {
    take_request_now(plane).map(|request| request.reply)
}

/// A request whose requester is gone reads closed, so a query can be told
/// from the next by whether its requester was dropped.
fn marked() -> (
    oneshot::Sender<ActorStatusInfo>,
    oneshot::Receiver<ActorStatusInfo>,
) {
    oneshot::channel()
}

/// Two first queries that arrive together are answered in arrival order,
/// even when both land between the run loop's read of the held bit and its
/// read of the channel bit in one poll.
#[test]
fn two_concurrent_first_queries_are_answered_in_arrival_order() {
    let (record, mut plane) = new_plane();
    let (first_tx, first_rx) = marked();
    let (second_tx, _second_rx) = marked();
    // The first query's requester is gone, which marks it.
    drop(first_rx);
    let slot = (first_tx, second_tx);
    let queued = record.clone();
    BETWEEN_LOADS.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            assert!(submit_now(&queued, slot.0), "the first query is held");
            assert!(submit_now(&queued, slot.1), "the second opens the channel");
        }));
    });
    let served_first = take_now(&mut plane).expect("a query is ready");
    assert!(
        served_first.is_closed(),
        "the second query was answered before the first"
    );
    let served_second = take_now(&mut plane).expect("the second query is ready");
    assert!(!served_second.is_closed());
    assert!(take_now(&mut plane).is_none());
}

/// The plane queues 64 requests in steady state, as the system channel of an
/// actor without a held slot does. Before the run loop has taken the request
/// held ahead of the channel, that request is queued beside the 64.
#[test]
fn the_plane_queues_sixty_four_in_steady_state() {
    let (record, mut plane) = new_plane();
    let mut cold = 0;
    loop {
        let (reply, _requester) = marked();
        if !submit_now(&record, reply) {
            break;
        }
        cold += 1;
    }
    let mut served = 0;
    while take_now(&mut plane).is_some() {
        served += 1;
    }
    assert_eq!(served, cold, "every queued request is served");

    let mut steady = 0;
    loop {
        let (reply, _requester) = marked();
        if !submit_now(&record, reply) {
            break;
        }
        steady += 1;
    }
    assert_eq!(cold, SYSTEM_CAPACITY + 1, "cold bound");
    assert_eq!(steady, SYSTEM_CAPACITY, "steady-state bound");
    assert_eq!(SYSTEM_CAPACITY, 64);
}

// ---------------------------------------------------------------------------
// The requesters' wakers
// ---------------------------------------------------------------------------

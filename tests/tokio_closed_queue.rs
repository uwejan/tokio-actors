//! An actor's end closes its queues and drains them once. It starts the
//! late-value drain only when a closed queue reports a send still landing,
//! and that report is Tokio's: a closed queue with a send in flight answers
//! `Empty`, and answers `Disconnected` only once nothing more can arrive. A
//! Tokio that answers `Disconnected` while a send is in flight would leave
//! that value in the closed queue, outside the drain. The bounded queue holds
//! the in-flight window open with a permit; the unbounded queue takes the same
//! answer from the same receive path. The mailbox's queue is the unbounded
//! one, and no permit holds its window open: the mailbox's own tests pin it
//! with many ends in a row that race pushes still landing
//! (`ends_racing_pushes_that_are_landing_drop_every_message_and_return_every_unit`).

use tokio::sync::mpsc::{self, error::TryRecvError};

#[test]
fn a_closed_queue_with_a_send_in_flight_answers_empty_until_it_lands() {
    let (tx, mut rx) = mpsc::channel::<u32>(4);
    let permit = tx.try_reserve().expect("a free slot");
    rx.close();
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    permit.send(7);
    assert_eq!(rx.try_recv(), Ok(7));
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
}

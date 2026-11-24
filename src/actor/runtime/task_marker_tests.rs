use super::*;

/// A task whose own poll unwinds past every catch, while it holds an
/// abort guard: the guard drops inside that unwind and writes the panic,
/// and the runner's reading of the join (`joined_reason`) is the plane's
/// reason, one value with one text.
async fn an_unwind_of_the_task_s_own_poll_reads_one_panic() {
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let marker = TaskMarker::default();
    let guard = AbortReason::new(status_tx.clone(), marker.clone(), ActorId::from("unwound"));
    let join = tokio::spawn(MarkedRun::new(
        marker,
        IncarnationRef::new().incarnation(),
        async move {
            let _guard = guard;
            tokio::task::yield_now().await;
            if std::hint::black_box(true) {
                panic!("the crate's own code panics in the task's poll");
            }
            StopReason::Graceful
        },
    ));
    let res = join.await;
    assert!(res.as_ref().is_err_and(|err| err.is_panic()));
    let plane = status_rx.borrow().reason.clone();
    let want = StopReason::Failure(ActorError::Panic(UNCAUGHT_PANIC.to_string()));
    assert_eq!(plane, Some(want.clone()), "the plane reads the panic");
    assert_eq!(
        crate::actor::supervision::joined_reason(res, || plane, None),
        want,
        "the runner reads the plane's reason"
    );
}

/// The guard of a task aborted while suspended writes `Kill`, and the
/// runner reads `Kill`.
async fn an_abort_of_a_suspended_task_reads_kill() {
    let (status_tx, status_rx) = StatusTx::channel(StatusValue::new(ActorStatus::Running));
    let marker = TaskMarker::default();
    let guard = AbortReason::new(status_tx.clone(), marker.clone(), ActorId::from("aborted"));
    let join = tokio::spawn(MarkedRun::new(
        marker,
        IncarnationRef::new().incarnation(),
        async move {
            let _guard = guard;
            std::future::pending::<()>().await;
            StopReason::Graceful
        },
    ));
    tokio::task::yield_now().await;
    join.abort();
    let res = join.await;
    assert!(res.as_ref().is_err_and(|err| err.is_cancelled()));
    let plane = status_rx.borrow().reason.clone();
    assert_eq!(plane, Some(StopReason::Kill));
    assert_eq!(
        crate::actor::supervision::joined_reason(res, || plane, None),
        StopReason::Kill
    );
}

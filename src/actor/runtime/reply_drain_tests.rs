use super::*;

/// The turns before which the loop would hand the thread back, when turn
/// `i` sends a reply exactly if `replies[i]` and a lane always holds
/// messages.
fn hand_backs(replies: &[bool]) -> Vec<usize> {
    let mut drain = ReplyDrain::new();
    let mut at = Vec::new();
    for (i, replied) in replies.iter().enumerate() {
        if drain.0 != 0 && drain.step() {
            at.push(i);
        }
        if *replied {
            drain.replied();
        }
    }
    at
}

fn status_info() -> ActorStatusInfo {
    ActorStatusInfo {
        id: ActorId::from("status".to_string()),
        name: None,
        status: ActorStatus::Running,
        mailbox_len: 0,
        mailbox_capacity: 1,
        priority_len: 0,
        priority_capacity: 0,
        child_count: 0,
        timer_count: 0,
        stream_count: 0,
        incarnation: crate::types::Incarnation::mint(),
        monitors: 0,
        monitored_by: 0,
        trap_exit: false,
        links: 0,
        pg_groups: 0,
        group_monitors: 0,
        aliases: 0,
        requests_in_flight: 0,
        suspended: false,
        state: None,
    }
}

/// What answering one status request leaves behind: whether the window
/// opened and whether the reply is marked for the hand-back.
fn answered(task: bool, same_thread: bool, requester_gone: bool) -> (bool, bool) {
    let (reply, requester) = oneshot::channel();
    let keep = (!requester_gone).then_some(requester);
    let mut fp_skip = 0;
    let mut drain = ReplyDrain::new();
    answer_status(
        crate::types::StatusRequest { reply, task },
        status_info(),
        same_thread,
        &mut fp_skip,
        &mut drain,
    );
    drop(keep);
    (
        fp_skip == STATUS_FAST_PATH_OFF_TURNS,
        drain.0 & ReplyDrain::REPLIED != 0,
    )
}

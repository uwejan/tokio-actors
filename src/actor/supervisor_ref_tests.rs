//! In-crate checks of the commit split that the non-blocking stops and the
//! supervisor reference rest on.

use std::time::Duration;

use crate::actor::supervision::{
    ChildLifecycle, DownPending, GroupMembership, ManualFacts, ManualStop, TaskState,
};
use crate::types::{RestartType, Shutdown};

/// Every ledger state a manual call can meet.
fn lives() -> Vec<Option<ChildLifecycle>> {
    let mut lives = vec![
        None,
        Some(ChildLifecycle::Running(1)),
        Some(ChildLifecycle::Stopping {
            incarnation: 1,
            kind: ManualStop::Bounce,
        }),
        Some(ChildLifecycle::Stopping {
            incarnation: 1,
            kind: ManualStop::Terminate,
        }),
        Some(ChildLifecycle::Restarting {
            incarnation: 1,
            next: 2,
        }),
    ];
    for pending in [
        DownPending::None,
        DownPending::Snapshot,
        DownPending::Evaluate,
    ] {
        lives.push(Some(ChildLifecycle::Down {
            incarnation: 1,
            pending,
        }));
    }
    lives
}

fn grid() -> Vec<ManualFacts> {
    let memberships = [
        GroupMembership::None,
        GroupMembership::Awaited { overridden: false },
        GroupMembership::Awaited { overridden: true },
        GroupMembership::Queued,
        GroupMembership::Released,
    ];
    let tasks = [TaskState::Live, TaskState::RunnerLost, TaskState::Ended];
    let restart_types = [
        RestartType::Permanent,
        RestartType::Transient,
        RestartType::Temporary,
    ];
    let policies = [
        Shutdown::Kill,
        Shutdown::Timeout(Duration::from_millis(100)),
        Shutdown::Infinity,
    ];
    let mut facts = Vec::new();
    for life in lives() {
        for task in tasks {
            for membership in memberships {
                for restart_type in restart_types {
                    for policy in policies {
                        for bits in 0u8..32 {
                            facts.push(ManualFacts {
                                life,
                                task,
                                membership,
                                restart_type,
                                pool: bits & 1 != 0,
                                restarted: bits & 2 != 0,
                                in_keep_scope: bits & 4 != 0,
                                policy,
                                lane_vetoable: bits & 8 != 0,
                                reasked: bits & 16 != 0,
                                has_order: bits & 3 == 0,
                            });
                        }
                    }
                }
            }
        }
    }
    facts
}

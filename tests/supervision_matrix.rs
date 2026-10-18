//! The supervision state-by-call-site matrix: for each way a child can be
//! asked to stop, and each state the child can be in, the test that drives
//! exactly that call in exactly that state, or a plain statement of what the
//! code does there today (marked "no test").
//!
//! A stop of a supervised child belongs to the child's own runner, the task
//! that awaits the child's exit: it raises the first signal, escalates the
//! child's `Shutdown` policy (Kill, then abort) and records the child's end.
//! A caller that waits for the stop never owns any step of it, so abandoning
//! the wait, with `tokio::time::timeout` or by dropping the future, changes
//! nothing about how the child is stopped.
//!
//! Columns, in the order of a child's states: RUN running; STOP stopping by a
//! manual stop; GROUP a member of a pending group restart (awaited, queued or
//! released); ATT1 restart
//! attempt not yet spawned; ATT2 restart attempt initializing; ATT3 restart
//! attempt acked and not yet adopted; ATT4 restart attempt ordered to stop;
//! DOWN down; OWED down with its evaluation owed; VERDICT carrying the
//! Unresponsive verdict.
//!
//! Tests that fail a runner are unit tests (`actor::supervision::lost_runner`
//! and `actor::supervision::slot`): no public call can fail a runner, so a
//! failing task stands in for it there.
//!
//! | call site | RUN | STOP | GROUP |
//! |---|---|---|---|
//! | `terminate_child` | abandoned at the commit: `terminate_child_abandoned_at_commit_still_stops_a_vetoing_child`; mid window: `terminate_child_abandoned_mid_window_still_stops_a_vetoing_child`; after the Kill step: `terminate_child_abandoned_after_the_kill_step_still_aborts_a_parked_child`; a child stuck in a turn: the whole ladder runs and the call answers `ChildUnresponsive`: `terminate_child_of_a_stuck_child_answers_child_unresponsive` | a terminate in progress is joined: `second_terminate_child_waits_for_an_abandoned_stop_of_the_same_child`; a bounce in progress is upgraded to a terminate and the child stays down: `terminate_child_upgrades_an_abandoned_bounce` | `terminate_child_override_runs_pre_stop_once`; a child respawned under the name of a released member is not a member: `a_child_respawned_under_the_name_of_an_overridden_member_is_not_a_member_of_the_group`; a released member is refused by `restart_child` until the group completes, and then revived it is a running child: `a_member_released_from_a_group_and_restarted_is_stopped_by_terminate_child`; a child spawned again under the name of a released member is a child like any other: `a_child_respawned_under_the_name_of_a_released_member_is_stopped_by_terminate_child`; a member waiting in the restart set leaves it: `terminate_child_removes_a_chain_queued_member`, `terminate_child_on_a_retrying_member_cancels_the_retry`; a call made during the chain keeps the member down through the retry: `terminate_during_the_chain_keeps_the_member_down_through_the_retry`; a call made while an attempt of the chain is in flight answers `ChildRestarting`: `stop_child_of_a_pending_group_member_is_absorbed_while_its_manual_stop_is_in_progress` (its second run); terminate, delete and spawn in one handler: `terminate_delete_respawn_in_one_handler_succeeds_in_every_group_phase`; the one more ask a group and the calls that join its member share: `terminate_calls_during_one_group_share_one_more_ask` |
//! | `stop_child` | abandoned at the commit, a permanent child is bounced once: `stop_child_abandoned_at_commit_bounces_a_vetoing_permanent_child_once`; a child whose task already ended is bounced once, free of charge: `stop_child_of_a_permanent_child_whose_task_already_ended_restarts_it_once`; a stop that meets a crash is the crash, reported and charged once, whether the restarted instance initializes at once, slowly or while the handler lingers: `bounce_racing_a_crash_then_restart_child_reports_the_crash_once`, `bounce_racing_a_crash_then_restart_child_with_a_slow_init_reports_the_crash_once`, `bounce_racing_a_crash_then_restart_child_with_a_linger_reports_the_crash_once`; a child spawned again under the name of a member a group released is a non-member while the group pends, and is bounced and kept: `a_child_respawned_under_a_released_name_is_bounced_and_kept_by_stop_child` | a stop in progress is joined and asks a vetoing `Infinity` child once more: `a_second_manual_stop_asks_a_vetoing_infinity_child_once_more` | a `Permanent` member the group still has to restart absorbs it and the answer says so: `stop_child_of_a_pending_group_member_is_absorbed_while_its_manual_stop_is_in_progress`, `stop_child_of_a_queued_member_is_absorbed_and_one_the_chain_restarted_answers_child_restarting`, `stop_child_during_a_retry_of_a_member_the_first_chain_restarted_is_absorbed`; one the chain already restarted, one released and the chain front are answered `ChildRestarting`: `stop_child_of_a_queued_member_is_absorbed_and_one_the_chain_restarted_answers_child_restarting`, `stop_child_of_a_member_released_by_terminate_child_answers_child_restarting_until_the_group_completes`, `stop_child_of_an_awaited_member_whose_terminate_was_abandoned_answers_child_restarting`, `stop_child_during_a_rest_for_one_retry_of_a_member_the_first_chain_restarted_answers_child_restarting`; a `Transient` or `Temporary` member is stopped as `terminate_child` stops it: `stop_child_of_a_transient_member_stopping_under_its_group_stops_it_and_keeps_it_down_through_the_retry`, `stop_child_of_a_temporary_member_stopping_under_its_group_stops_and_removes_it`, and so is such a child started while the group pends: `stop_child_of_a_transient_child_spawned_during_a_pending_group_stays_down_through_the_retry` |
//! | `restart_child` | answered `ChildRunning`: `a_child_respawned_under_the_name_of_a_released_member_is_stopped_by_terminate_child`; once its task ended and its death is not yet handled, answered `ChildRestarting`: `children_reads_a_child_whose_task_ended_as_down_before_its_death_is_handled` | answered `ChildRestarting` whether or not the task ended, and never classified to answer: `restart_child_of_a_child_being_stopped_answers_child_restarting_until_its_death_is_handled`, `restart_and_delete_of_a_child_being_stopped_answer_child_restarting` | a member released by `terminate_child` is answered `ChildRestarting` until the group completes: `restart_child_after_group_override_keeps_the_revived_child_running`, `a_member_released_from_a_group_and_restarted_is_stopped_by_terminate_child`; a member the group awaits or will restart is answered `ChildRestarting` too (no test), and so is a key a chain of the group already restarted: `restart_child_answers_child_restarting_for_a_key_the_chain_restarted_until_the_group_completes` |
//! | `delete_child` | answered `ChildRunning` (no test); once its task ended and its death is not yet handled, answered `ChildRestarting`: `children_reads_a_child_whose_task_ended_as_down_before_its_death_is_handled` | answered `ChildRestarting`: `restart_and_delete_of_a_child_being_stopped_answer_child_restarting` | a member released by `terminate_child` is answered by its own state, so it is deleted once down and its name can be taken again: `a_child_respawned_under_the_name_of_an_overridden_member_is_not_a_member_of_the_group`; a member the group awaits or will restart is answered `ChildRestarting` (no test) |
//! | `children()` | a child whose task ended and whose death is not yet handled reads not alive with its restart pending: `children_reads_a_child_whose_task_ended_as_down_before_its_death_is_handled` | a child being stopped reads alive until its task ends: `children_reports_a_stopping_child_as_alive` | a member released by `terminate_child` reads not alive and not pending: `restart_child_after_group_override_keeps_the_revived_child_running`; a death recorded while the group pends reads pending: `recorded_death_awaiting_its_decision_reads_restart_pending` |
//! | parent graceful stop | `parent_stop_escalates_a_vetoing_running_child_through_its_runner`; a SimpleOneForOne supervisor orders every child at once, each under its own `Shutdown` policy: `simple_one_for_one_shutdown_stops_children_concurrently` | joined, never given a second ladder, and waited for under the child's own `Shutdown` window: `parent_stop_reaches_a_child_whose_manual_stop_is_in_progress` (a `terminate_child` in progress), `parent_stop_reaches_a_child_whose_stop_child_is_in_progress` (a `stop_child` in progress, no restart follows); a vetoing `Infinity` child is asked once more: `parent_stop_asks_an_infinity_child_in_an_abandoned_terminate_once_more`; a SimpleOneForOne supervisor joins the stop in progress and orders a restart attempt: `simple_one_for_one_shutdown_joins_a_stopping_child_and_orders_a_restarting_one` | the pending group is cleared when the teardown starts, and a running member that already holds the group's order is awaited, not ordered a second time, and a vetoing `Infinity` member is asked once more: `parent_teardown_asks_a_vetoing_infinity_member_the_group_ordered_once_more`, `parent_teardown_asks_a_vetoing_infinity_child_once_more_after_its_group_did` |
//! | group evaluation under `OneForAll` | every member with an occupant is committed to the group at once, and the group stops its members one at a time in reverse start order, each ended before the next is asked to stop: `group_members_stop_one_at_a_time_in_reverse_start_order`; a member that crashes before the queue reaches it is part of the group: `a_queued_member_that_crashes_during_a_sequential_group_stop_folds_into_the_group`; a member under `Infinity` holds the members behind it: `an_infinity_member_blocks_the_members_queued_behind_it`; a member revived by `restart_child` whose old incarnation's report is still pending is awaited at its new incarnation, and the old report removes nothing of it: `stale_snapshot_never_erases_a_newer_barrier_entry` | a member whose manual stop is in progress is joined, never given a second ladder, and restarted only after its old task ended (the chain holds at it until its stop completes and then starts it in its slot: `a_chain_front_whose_manual_stop_was_in_progress_does_not_hold_the_group_for_ever`); the restart is charged once: `group_restart_waits_for_a_member_already_stopping` (a `stop_child` in progress), `group_restart_waits_for_a_member_whose_terminate_child_is_in_progress`, `group_restart_never_runs_two_incarnations_of_an_anonymous_member`; a member stopping behind the head keeps stopping and is joined in its turn: `members_already_stopping_keep_stopping_during_a_sequential_group_stop`, and a vetoing `Infinity` one is asked once more then, not at the evaluation: `a_member_stopping_at_the_evaluation_is_asked_once_more_when_the_queue_reaches_it`; a terminate committed before the evaluation rejoins the restart and is reported as it: `terminate_committed_before_the_group_trigger_rejoins_the_group_restart` | `group_stop_escalates_a_vetoing_member_through_its_runner`; a terminate committed while the group pends keeps the member down: `terminate_committed_after_the_group_trigger_stays_down`; a terminate of a member the queue has not reached stops it at once, beside the member the queue is stopping: `terminate_child_of_a_member_the_stop_queue_has_not_reached_returns_once_it_ended`, and so does a `stop_child` of a `Transient` or `Temporary` member: `stop_child_of_a_transient_member_of_a_pending_group_stops_it_and_keeps_it_down_through_the_retry`, `stop_child_of_a_temporary_member_of_a_pending_group_stops_and_removes_it`; a terminate asks a vetoing `Infinity` member once more, the one ask it shares with the group: `terminate_override_gives_an_infinity_member_the_one_more_ask_the_group_has_not_given`, `terminate_override_gives_the_one_more_ask_to_a_member_the_group_ordered`, `a_second_terminate_override_gives_the_one_more_ask_to_a_member_holding_the_first_ones_order` |
//! | a runner that fails | `a_lost_runner_reports_its_childs_end_as_a_kill`, `a_parent_stop_waits_for_a_child_whose_runner_failed` | `a_caller_waiting_on_a_child_whose_runner_fails_returns_ok_once_it_ends` | the wait on the child is the same cell wait (no test) |
//!
//! | call site | ATT1 | ATT2 | ATT3 | ATT4 |
//! |---|---|---|---|---|
//! | `terminate_child`, `stop_child`, `restart_child`, `delete_child` | answered `ChildRestarting`, the same ledger state as ATT2; an attempt that waits for its predecessor's subtree is in this state (no test makes a manual call during the wait, and none holds an attempt before its factory returns) | `manual_stops_of_a_child_whose_restart_is_in_flight_answer_child_restarting` | answered `ChildRestarting`, the same ledger state (no test holds an attempt there; the tests of `tests/supervision_child_mgmt.rs` that bounce a child wait until the roster reads it adopted) | the teardown runs no handler while it orders, so no call meets its attempt, and a call that meets an attempt a group ordered is answered `ChildRestarting`, the member's ledger state (no test) |
//! | parent graceful stop | the attempt honors the order before its factory runs, so the factory is never called: `parent_stop_cancels_a_restart_attempt_before_its_factory_runs`; the same for an attempt that still waits for its predecessor's subtree: `parent_stop_cancels_a_restart_attempt_that_waits_for_its_predecessor_subtree` | the incarnation is stopped within the window the order gives it, which counts from the order, and its start timeout keeps running: `parent_stop_mid_restart_runs_the_fresh_incarnations_on_stopped_before_stopped`, `parent_stop_gives_a_starting_child_its_window_from_the_stop`, `parent_stop_bounds_a_hung_infinity_init_by_its_start_timeout` | the outcome that waited for the supervisor is adopted by the teardown and its runner stops the incarnation: `parent_stop_takes_a_child_whose_restart_acked_while_the_supervisor_was_busy` | an attempt the teardown orders is cancelled before its factory or stopped under the order, driven by the ATT1 and ATT2 tests, which reach this state through the order |
//! | group evaluation under `OneForAll` | an attempt not yet spawned is cancelled at the evaluation: its factory never runs and nothing is delivered: `group_restart_cancels_an_unspawned_attempt_at_the_plan_while_a_slower_member_is_stopped` (and, without telling the cancel flag from the stop queue's order, `group_restart_cancels_a_member_attempt_that_has_not_spawned`); one that waits for its predecessor's subtree is cancelled too, so the group does not wait for the wait: `group_restart_cancels_a_waiting_attempt_at_the_plan_while_a_slower_member_is_stopped` (and, the same way, `group_restart_cancels_a_member_attempt_that_waits_for_its_predecessor_subtree`) | a spawned attempt is stopped in its turn, after the members ahead of it, under its own policies from the group's order and never kept, and the member is started again in its slot after the ones ahead of it: `group_restart_stops_an_in_flight_restart_and_restarts_it_in_its_slot`, `group_restart_stops_a_spawned_attempt_in_its_turn_behind_the_member_ahead_of_it`, `group_stop_ordered_on_an_initializing_member_keeps_its_window_while_the_supervisor_is_busy` | an acked attempt whose outcome waits is adopted: when the queue reached it already, its runner honors the order the cell holds at once, and otherwise the queue orders the adopted incarnation in its turn: `group_restart_stops_a_spawned_attempt_in_its_turn_behind_the_member_ahead_of_it` | an attempt the group orders ends with `Stopped` and is delivered once with `RestartInitiated`: `manual_restart_attempt_stopped_by_a_group_is_reported_once` |
//! | an attempt whose wait for its predecessor's subtree expires is not evaluated: it is recorded down and the supervisor stops with an unresponsive-child failure: `successor_wait_escalates_when_an_old_grandchild_never_yields`; restart attempt outcomes | an attempt that fails before it spawns an incarnation is reported at once and evaluated: `restart_attempt_panic_is_evaluated_instead_of_left_restarting`; one that honored an order or the group's cancel before its factory ran reports a cancel and nothing is delivered, one whose factory panicked during the stop, before the teardown waited on it, builds nothing and the stop completes: `parent_stop_completes_when_an_attempt_s_factory_panics_during_the_stop` | an attempt that fails after its incarnation published a status is followed to the incarnation's end and reported once: `restart_task_join_error_becomes_a_recorded_death_after_the_incarnation_ends` (unit; a stand-in attempt that published its status); an attempt of a `restart_child` whose initialization fails is reported removed with the error, charges nothing and stops no sibling: `failed_manual_restart_child_is_reported_removed_and_charges_no_budget`; the attempt of a group member whose initialization fails before the stop queue reaches the member is reported once as the group's restart of it, with the error its initialization returned, and charges nothing: `a_committed_attempt_whose_init_fails_before_its_turn_reports_the_init_error_through_the_member_route`; an init that outlives its start timeout and its kill gets the verdict and the child is not restarted beside it: `start_timeout_on_a_non_yielding_init_gives_the_verdict_and_never_restarts_beside_it`, `attempt_start_timeout_still_bounds_a_hung_init_after_a_bare_kill` | an incarnation that acked and then crashed is adopted and its death is evaluated like any death: `attempt_adopts_an_incarnation_that_acked_and_then_crashed_and_its_death_is_evaluated` | an attempt ordered to stop, by the teardown or by a group, reports that its incarnation stopped under the order, and a group's report is delivered once with `RestartInitiated` (`attempt_honors_an_order_written_during_init_with_the_window_from_the_order`, unit, `slot.rs`; `manual_restart_attempt_stopped_by_a_group_is_reported_once`) |
//!
//! | call site | DOWN | OWED | VERDICT |
//! |---|---|---|---|
//! | `terminate_child` | `manual_calls_succeed_once_an_abandoned_stop_completes` | dismisses the owed decision, and the death is reported once as a removal after the subtree ended: `terminate_child_on_a_death_awaiting_its_decision_dismisses_the_restart`, `terminate_child_of_a_death_awaiting_its_decision_in_a_group_dismisses_it_without_a_charge`, `terminate_child_after_a_bounce_met_a_crash_dismisses_it_and_the_crash_is_reported_once` (the names of these two are kept; both assert the dismissal) | a call on a child whose verdict came from a terminate is answered `ChildUnresponsive` at once: the second call of `terminate_child_of_a_stuck_child_answers_child_unresponsive`; a child whose verdict came from a `stop_child` is answered `ChildRestarting` (no test) |
//! | `stop_child` | answered `Ok` (no test) | answered `ChildRestarting`: `terminate_child_on_a_death_awaiting_its_decision_dismisses_the_restart` | answered `ChildUnresponsive` after the child's ladder, and the child is not restarted while it lives: `manual_stop_of_a_non_yielding_child_returns_unresponsive_and_never_restarts_it_while_it_lives` |
//! | `restart_child` | the child is started again: `manual_calls_succeed_once_an_abandoned_stop_completes`, `solo_terminate_then_restart_child_in_one_handler_keeps_the_child_running` | answered `ChildRestarting` until the decision is made, for a death in a pending group and for a crash a bounce met: `recorded_death_awaiting_its_decision_reads_restart_pending`, `bounce_racing_a_crash_then_restart_child_reports_the_crash_once` | the child still runs or is being stopped, and is answered by that state (no test) |
//! | `delete_child` | the spec is removed: `a_child_respawned_under_the_name_of_an_overridden_member_is_not_a_member_of_the_group` | answered `ChildRestarting`: `recorded_death_awaiting_its_decision_reads_restart_pending` | the child still runs or is being stopped, and is answered by that state (no test) |
//! | parent graceful stop | skipped: nothing to stop, and a death still pending is delivered once with `SupervisorStopping` and no restart: `a_crash_pending_at_the_supervisors_stop_is_delivered_once_with_supervisor_stopping_before_on_stopped` | the same: the teardown skips the child and its pending death is delivered once, before `on_stopped` | the teardown records the verdict and moves on instead of waiting for the child: `parent_stop_records_a_non_yielding_child_and_completes` |
//! | group evaluation under `OneForAll` | a down member is restarted in start order by the chain (not driven here); a down `Temporary` member does not exist, since its spec is removed when the child ends (no test) | an owed evaluation of a member is absorbed into the group, delivered once and charged nothing, whether its report waits behind the group or is still in the death plane: `a_member_death_queued_before_its_group_is_absorbed_once_without_a_second_charge`, `a_member_death_still_in_the_death_plane_at_its_group_evaluation_is_absorbed_once`, `a_bounce_that_met_a_crash_keeps_its_report_when_the_chain_restarts_the_member`; a report absorbed by a retry: `a_member_death_queued_behind_a_group_is_absorbed_when_the_group_retries`, `retry_with_a_member_death_queued_completes_the_group`; for a `Temporary` member every one of these ends is a removal, reported removed: `temporary_member_stopped_with_its_group_is_removed` (a failed bounce the group joins, a deferred report, a report still in the death plane, a `terminate_child` the group joins). A report waiting among the deferred triggers is checked by the unit test `a_failing_delivery_in_a_group_evaluation_leaves_the_trigger_owed` (`lost_runner.rs`); a report in the worklist by the table test of `absorbable` (`rules.rs`) and the explorer | a group that can make progress only through members carrying the verdict stops the supervisor with `ChildUnresponsive`: `group_blocked_only_by_an_unresponsive_member_stops_the_supervisor` |
//! | restart attempt outcomes | not a restart attempt state | not a restart attempt state | not a restart attempt state |
//! | a runner that fails | a runner that fails after its fate was recorded changes nothing (no test) | same | the replacement task of a failed runner gives the verdict once the incarnation outlived the guard's ladder, and a wait on its closed cell records it first when it expires first: `lost_runner_of_a_stuck_incarnation_gets_the_verdict_after_its_abort` (unit, `slot.rs`), `caller_told_unresponsive_on_a_lost_runner_leaves_one_verdict_for_the_group_barrier` (unit, `lost_runner.rs`) |
//!
//! Through the actor's handle a `Kill` is accepted until the actor seals its
//! stop lane, just before its queued messages and its value are dropped
//! (`ActorSystem::stop` and `kill` answer `NotFound` once the actor has left
//! the registry). It is observed at the actor's next turn boundary, and its
//! task is aborted once the actor's
//! kill grace has passed, whoever raised it and even when the call that raised
//! it is dropped. Tests of the kill paths, by call site and by the state the
//! target is in when the `Kill` is raised:
//!
//! | call site | target parked at an await | call dropped after the raise |
//! |---|---|---|
//! | `ActorSystem::kill(name)` | `kill_by_name_aborts_a_turn_parked_at_an_await` | the call returns at once, so there is nothing to drop (no test) |
//! | `ActorHandle::stop(Kill)` | `stop_kill_aborts_a_turn_parked_at_an_await` | the same (no test) |
//! | `ActorSystem::kill_by_id` | `every_kill_entry_point_waits_the_target_s_own_kill_grace` (`tests/kill_grace.rs`) | `kill_by_id_backstop_survives_a_dropped_call` |
//! | `ActorHandle::stop_within(Kill, within)` | `kill_reason_with_a_short_window_aborts_a_parked_handler` (`tests/stop_within.rs`) | `stop_within_kill_backstop_survives_a_dropped_call` |
//! | system shutdown, the Kill step | `every_kill_entry_point_waits_the_target_s_own_kill_grace` | `shutdown_kill_step_backstop_survives_a_dropped_shutdown` |
//!
//! | state of the target at the `Kill` | test |
//! |---|---|
//! | before or during `pre_start` | `kill_before_or_during_pre_start_skips_every_later_init_hook`, `kill_before_the_first_poll_skips_pre_start`, and a `Kill` in the window before the task's abort handle is stored: `kill_in_the_pre_attach_window_still_ends_in_an_abort` |
//! | a failed initialization | `kill_during_a_failed_init_takes_the_kill_teardown` |
//! | `on_stopped` running | `kill_during_on_stopped_takes_the_kill_teardown` |
//! | its children being stopped | `kill_is_accepted_while_children_are_being_stopped`, `stop_within_kill_is_accepted_while_children_are_being_stopped`; every other reason is refused: `graceful_stop_is_refused_while_children_are_being_stopped` |
//! | already aborted | `stop_and_kill_on_an_aborted_actor_are_refused` |
//!
//! When a supervisor ends without stopping its children, the link guard each
//! child's runner holds acts on the child. A worker is raised `Kill`. A child
//! that is itself a supervisor is raised `ParentKilled` and stops its own
//! children in order within its own `Shutdown`: the reaper raises `Kill` when
//! that window has passed and aborts the task a kill grace after the `Kill`.
//! Tests, by the kind of child:
//!
//! | child | test |
//! |---|---|
//! | a supervisor child | it runs `on_stopped` and stops its children newest first, a pool all at once: `killed_supervisor_s_supervisor_child_runs_on_stopped_and_stops_its_children_in_order`; a death still pending in it is delivered once, before `on_stopped`: `killed_supervisor_s_supervisor_child_delivers_a_pending_death_before_on_stopped`; a stop that outlasts its kill grace is finished: `killed_supervisor_s_supervisor_child_finishes_a_slow_orderly_teardown_past_its_kill_grace`; it cannot veto: `inherited_stop_cannot_be_vetoed`; its own window bounds it: `inherited_stop_is_bounded_by_the_child_s_own_shutdown`; named and anonymous members: `three_level_tree_killed_at_the_root_named_and_anonymous` |
//! | a supervisor child under `Infinity` | its orderly stop has no bound, and its supervisor's supervisor restarts the killed supervisor only after it ended: `infinity_supervisor_child_under_a_killed_parent_follows_its_policy`; the wait on it is bounded again once it ended: `successor_wait_is_bounded_again_once_its_infinity_supervisor_child_ended` |
//! | a worker | it is killed and runs no `on_stopped`: `killed_supervisor_s_worker_children_skip_on_stopped`; under `Infinity` it is aborted once its kill grace has passed: `infinity_supervisor_child_under_a_killed_parent_follows_its_policy` |
//! | the killed supervisor's own restart | it waits for the orderly subtree: `killed_supervisor_restart_waits_for_its_orderly_subtree` |
//! | a `Transient` child given the inherited stop | it is not restarted and nothing is charged: `transient_child_stopped_with_parent_killed_is_not_restarted` |

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::{sleep, timeout, Instant};

use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorError, ActorHandle, ActorResult, ActorStatus, ActorSystem, ChildEvent,
    RestartType, Shutdown, StopReason, SupervisionAction, SupervisionConfig,
};

mod common;
#[path = "supervision_matrix/subtree_wait.rs"]
mod subtree_wait;

// ---------------------------------------------------------------------------
// Timing: every value a test assumes about the crate's ladder is named once.
// ---------------------------------------------------------------------------

/// The time between a child's `Kill` and its abort when its spawn builder sets
/// no `kill_grace`. An integration test cannot read the crate's constant, so
/// the value is named here once.
const ASSUMED_KILL_GRACE: Duration = Duration::from_millis(120);

/// The wait after an abort before the Unresponsive verdict. An integration
/// test cannot read the crate's constant, so the value is named here once.
const ASSUMED_ABORT_SETTLE: Duration = Duration::from_millis(100);

/// The bound on every outcome a test awaits that is not derived from a
/// configured window.
const BOUND: Duration = Duration::from_secs(10);

/// The upper bound of a timed outcome: `precise` where an outcome lands on
/// its exact instant (the paused clock), and a hang bound past it on a
/// real-time runtime, loose enough for a host that stalls its threads.
fn upper(paused: bool, precise: Duration) -> Duration {
    if paused {
        precise
    } else {
        precise + BOUND
    }
}

/// How long the tests that count events watch for a late duplicate.
const QUIET: Duration = Duration::from_millis(300);

/// How far past its exact timing an outcome may land on a real-time runtime
/// in a test whose failing outcome is a turn or an init held until the test
/// releases it: loose enough for a host that stalls its threads, and far
/// short of the held thread.
const STALL_SLACK: Duration = Duration::from_secs(3);

/// The longest a turn or an init held on a gate holds its thread when the
/// test never opens the gate.
const GATE_CAP: Duration = Duration::from_secs(30);

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn uname(base: &str) -> String {
    format!("matrix-{base}-{}", UNIQ.fetch_add(1, Ordering::Relaxed))
}

/// Awaits `fut` for at most `bound`; a reverted build fails here instead of
/// hanging.
async fn within<T>(bound: Duration, what: &str, fut: impl std::future::Future<Output = T>) -> T {
    match timeout(bound, fut).await {
        Ok(value) => value,
        Err(_) => panic!("timed out after {bound:?} waiting for: {what}"),
    }
}

async fn wait_until(bound: Duration, what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + bound;
    loop {
        if pred() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {bound:?} waiting for: {what}"
        );
        sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// The child under test
// ---------------------------------------------------------------------------

/// What the tests observe of a child, across its restarts.
#[derive(Default)]
struct Probe {
    /// Calls of `pre_stop`.
    pre_stops: AtomicUsize,
    /// Instances built by the child's factory.
    built: AtomicUsize,
    /// Set when the most recent instance was dropped.
    ended: AtomicBool,
    /// While set, an instance waits in `on_started` before it acknowledges
    /// its start (for at most `GATE_CAP`).
    held: AtomicBool,
    /// Instances that finished `on_started`.
    started: AtomicUsize,
    /// Instances that entered `on_started` past its `held` wait.
    entered: AtomicUsize,
    /// While set, an instance fails its initialization.
    fail_init: AtomicBool,
    /// Notified by an instance that fails its initialization, right before
    /// it returns the failure.
    fail_signal: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// Set when an instance entered its slow failing turn.
    in_turn: AtomicBool,
    /// While set, a `Block` turn holds its thread until it is cleared (for at
    /// most `GATE_CAP`).
    block_held: AtomicBool,
    /// Set when an instance parked in `handle` on a `Park` message.
    parked: AtomicBool,
    /// Milliseconds an instance awaits in `on_started` before it records its
    /// start.
    init_sleep_ms: AtomicU64,
    /// While set, an instance that entered `on_started` holds its thread
    /// there until it is cleared (for at most `GATE_CAP`).
    init_gate: AtomicBool,
    /// Instances whose `on_started` held on `init_gate` and then went on.
    init_released: AtomicUsize,
    /// Calls of `on_stopped`.
    stopped_runs: AtomicUsize,
    /// While set, the factory panics before it builds an instance.
    factory_panics: AtomicBool,
    /// While set, dropping the child's factory panics.
    factory_drop_panics: AtomicBool,
    /// Set to make the next instance that starts send itself a crashing
    /// message in `on_started`.
    crash_on_start: AtomicBool,
    /// While set, `pre_stop` panics.
    pre_stop_panics: AtomicBool,
    /// The number of upcoming instances whose initialization fails.
    fail_next: AtomicUsize,
    /// The name of a child the next instance waits for, in `on_started`,
    /// until that child is alive.
    wait_alive: Mutex<Option<String>>,
    /// The global order of every call of `pre_stop`, oldest first.
    pre_stop_order: Mutex<Vec<usize>>,
    /// The number of requests each next instance vetoes, front first; the
    /// spawn's own number once empty.
    veto_script: Mutex<std::collections::VecDeque<usize>>,
    /// Milliseconds `pre_stop` awaits before it answers.
    pre_stop_sleep_ms: AtomicU64,
    /// While set, `pre_stop` waits before it answers.
    stop_held: AtomicBool,
    /// While set, the slow failing turn of `LongFail` stays in its turn.
    turn_held: AtomicBool,
    /// While set, `pre_stop` awaits after it recorded its call, before it
    /// answers.
    pre_stop_gate: AtomicBool,
    /// Calls of `pre_stop` that returned.
    pre_stops_returned: AtomicUsize,
    /// The number of upcoming instances that fail their initialization at
    /// once, before any wait of `on_started`.
    fail_first: AtomicUsize,
    /// The global order of the factory calls of this child, oldest first.
    builds: Mutex<Vec<usize>>,
    /// The global order of the drops of this child's instances, oldest first.
    drops: Mutex<Vec<usize>>,
    /// The global order of the end of the latest `on_started`.
    last_start: AtomicUsize,
    /// Instances built and not yet dropped, and the most there ever were.
    live: AtomicUsize,
    max_live: AtomicUsize,
    /// While set, the factory blocks its thread before it builds an
    /// instance. The wait runs in `block_in_place`, so the runtime hands the
    /// worker's queued tasks and its timers to another thread: the rest of
    /// the test runs on any number of workers, one included.
    factory_hold: AtomicBool,
    /// Set when the factory began to wait on `factory_hold`.
    in_factory: AtomicBool,
    /// While set, dropping an instance panics.
    instance_drop_panics: AtomicBool,
    /// The panics instance drops raised.
    instance_drop_panicked: AtomicUsize,
    /// The task each factory call ran in, oldest first.
    build_tasks: Mutex<Vec<Option<tokio::task::Id>>>,
    /// The task each instance dropped in, oldest first.
    drop_tasks: Mutex<Vec<Option<tokio::task::Id>>>,
}

/// One counter orders the events of every probe of a test.
static EVENT_ORDER: AtomicUsize = AtomicUsize::new(0);

impl Probe {
    fn next_in_order() -> usize {
        EVENT_ORDER.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Records a factory call.
    fn note_build(&self) {
        self.build_tasks.lock().unwrap().push(tokio::task::try_id());
        self.built.fetch_add(1, Ordering::SeqCst);
        self.builds.lock().unwrap().push(Self::next_in_order());
    }

    /// The place of the factory call number `index` in the global order.
    fn build_order(&self, index: usize) -> usize {
        self.builds.lock().unwrap()[index]
    }

    /// The place of the drop of instance number `index` in the global order;
    /// an instance not dropped yet comes after every event.
    fn drop_order(&self, index: usize) -> usize {
        self.drops
            .lock()
            .unwrap()
            .get(index)
            .copied()
            .unwrap_or(usize::MAX)
    }

    /// A probed child with a probe of its own.
    fn into_instance(self) -> Probed {
        Arc::new(self).instance(0)
    }

    /// The factory body shared by every spawn of a probed child.
    fn instance(self: &Arc<Self>, vetoes: usize) -> Probed {
        assert!(
            !self.factory_panics.load(Ordering::SeqCst),
            "the factory refuses to build"
        );
        if self.factory_hold.load(Ordering::SeqCst) {
            self.in_factory.store(true, Ordering::SeqCst);
            tokio::task::block_in_place(|| {
                while self.factory_hold.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
        }
        self.note_build();
        let vetoes = self
            .veto_script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(vetoes);
        self.ended.store(false, Ordering::SeqCst);
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_live.fetch_max(live, Ordering::SeqCst);
        Probed {
            probe: self.clone(),
            vetoes,
        }
    }
}

#[derive(Clone, Debug)]
enum ChildMsg {
    /// Await forever inside `handle`: the turn never reaches a boundary.
    Park,
    /// Panic inside `handle`.
    Crash,
    /// Spend 60 ms inside `handle`, then return an error.
    SlowFail,
    /// Block the thread inside `handle` while the probe's `block_held` is
    /// set: a turn that never reaches a boundary and cannot be aborted.
    Block,
    /// Answer at once: the instance has finished its initialization.
    Ping,
    /// Stay inside `handle` while the probe's turn is held, then return an
    /// error.
    LongFail,
}

/// Takes one from `left` when it is above zero, and says whether it did.
fn take_one(left: &AtomicUsize) -> bool {
    let mut seen = left.load(Ordering::SeqCst);
    while seen > 0 {
        match left.compare_exchange_weak(seen, seen - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(now) => seen = now,
        }
    }
    false
}

/// Vetoes the first `vetoes` requests to stop and accepts the rest.
struct Probed {
    probe: Arc<Probe>,
    vetoes: usize,
}

impl Actor for Probed {
    type Message = ChildMsg;
    type Response = ();

    async fn handle(&mut self, msg: ChildMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ChildMsg::Park => {
                self.probe.parked.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                Ok(())
            }
            ChildMsg::Crash => panic!("child crashed on command"),
            ChildMsg::Block => {
                self.probe.in_turn.store(true, Ordering::SeqCst);
                let cap = std::time::Instant::now() + GATE_CAP;
                while self.probe.block_held.load(Ordering::SeqCst)
                    && std::time::Instant::now() < cap
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(())
            }
            ChildMsg::SlowFail => {
                self.probe.in_turn.store(true, Ordering::SeqCst);
                sleep(Duration::from_millis(60)).await;
                Err(ActorError::user("the child failed after a slow turn"))
            }
            ChildMsg::Ping => Ok(()),
            ChildMsg::LongFail => {
                self.probe.in_turn.store(true, Ordering::SeqCst);
                while self.probe.turn_held.load(Ordering::SeqCst) {
                    sleep(Duration::from_millis(5)).await;
                }
                Err(ActorError::user("the child failed after a long turn"))
            }
        }
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if take_one(&self.probe.fail_first) {
            return Err(ActorError::user("the child refused to start at once"));
        }
        let cap = Instant::now() + GATE_CAP;
        while self.probe.held.load(Ordering::SeqCst) && Instant::now() < cap {
            sleep(Duration::from_millis(5)).await;
        }
        self.probe.entered.fetch_add(1, Ordering::SeqCst);
        if self.probe.init_gate.load(Ordering::SeqCst) {
            let cap = std::time::Instant::now() + GATE_CAP;
            while self.probe.init_gate.load(Ordering::SeqCst) && std::time::Instant::now() < cap {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.probe.init_released.fetch_add(1, Ordering::SeqCst);
        }
        let nap = self.probe.init_sleep_ms.load(Ordering::SeqCst);
        if nap > 0 {
            sleep(Duration::from_millis(nap)).await;
        }
        let awaited = self.probe.wait_alive.lock().unwrap().take();
        if let Some(name) = awaited {
            while !ActorSystem::default()
                .get::<Probed>(&name)
                .is_some_and(|handle| handle.is_alive())
            {
                sleep(Duration::from_millis(10)).await;
            }
        }
        if self.probe.fail_init.load(Ordering::SeqCst) || take_one(&self.probe.fail_next) {
            if let Some(signal) = self.probe.fail_signal.lock().unwrap().take() {
                signal.notify_one();
            }
            return Err(ActorError::user("the child refused to start"));
        }
        if self.probe.crash_on_start.swap(false, Ordering::SeqCst) {
            let _ = ctx.self_handle().notify(ChildMsg::Crash).await;
        }
        self.probe.started.fetch_add(1, Ordering::SeqCst);
        self.probe
            .last_start
            .store(Probe::next_in_order(), Ordering::SeqCst);
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.probe.stopped_runs.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        let calls = self.probe.pre_stops.fetch_add(1, Ordering::SeqCst) + 1;
        self.probe
            .pre_stop_order
            .lock()
            .unwrap()
            .push(Probe::next_in_order());
        assert!(
            !self.probe.pre_stop_panics.load(Ordering::SeqCst),
            "pre_stop panics on command"
        );
        let nap = self.probe.pre_stop_sleep_ms.load(Ordering::SeqCst);
        if nap > 0 {
            sleep(Duration::from_millis(nap)).await;
        }
        while self.probe.stop_held.load(Ordering::SeqCst) {
            sleep(Duration::from_millis(5)).await;
        }
        while self.probe.pre_stop_gate.load(Ordering::SeqCst) {
            sleep(Duration::from_millis(5)).await;
        }
        self.probe.pre_stops_returned.fetch_add(1, Ordering::SeqCst);
        calls > self.vetoes
    }
}

impl Drop for Probed {
    fn drop(&mut self) {
        self.probe
            .drops
            .lock()
            .unwrap()
            .push(Probe::next_in_order());
        self.probe
            .drop_tasks
            .lock()
            .unwrap()
            .push(tokio::task::try_id());
        self.probe.live.fetch_sub(1, Ordering::SeqCst);
        self.probe.ended.store(true, Ordering::SeqCst);
        if self.probe.instance_drop_panics.load(Ordering::SeqCst) && !std::thread::panicking() {
            self.probe
                .instance_drop_panicked
                .fetch_add(1, Ordering::SeqCst);
            panic!("{INSTANCE_DROP_PANIC}");
        }
    }
}

// ---------------------------------------------------------------------------
// The supervisor under test
// ---------------------------------------------------------------------------

type Events = Arc<Mutex<Vec<ChildEvent>>>;

struct Sup {
    events: Events,
    /// The supervisor stops itself in the `on_child_stopped` that reports a
    /// restart it initiated.
    stop_on_restart: bool,
    /// The supervisor yields once in `on_stopped`.
    yield_on_stopped: bool,
    /// Milliseconds every `on_child_stopped` awaits after it recorded its
    /// event.
    deliver_delay_ms: u64,
    /// The system the children live in; the default system when unset.
    system: Option<Arc<ActorSystem>>,
    knobs: Arc<Knobs>,
}

/// What a final delivery does, in the order the deliveries run.
#[derive(Clone, Debug)]
enum Fault {
    /// Nothing special.
    Pass,
    /// Returns this error.
    Err(&'static str),
    /// Panics.
    Panic,
    /// Raises `Kill` on the supervisor itself.
    KillSelf,
    /// Makes every supervision call at the named child and records the
    /// answers.
    Calls(String),
    /// Calls `terminate_child` at the named child and records the answer.
    Terminate(String),
}

/// What a supervisor records of its own callbacks, for the tests that read
/// the order of things.
#[derive(Default)]
struct Knobs {
    /// One line per `on_child_stopped` and per `on_stopped`, in order.
    order: Mutex<Vec<String>>,
    /// The reason each `on_stopped` received.
    stopped: Mutex<Vec<StopReason>>,
    /// The last error `ctx.last_error()` held in each `on_stopped`.
    last_errors: Mutex<Vec<Option<String>>>,
    /// The behaviour of the deliveries, front first; `Pass` once empty.
    faults: Mutex<std::collections::VecDeque<Fault>>,
    /// The child whose next delivery keeps the supervisor busy for this many
    /// milliseconds, once; its end is logged with a global order stamp.
    busy_for: Mutex<Option<(String, u64)>>,
}

#[derive(Clone)]
enum Cmd {
    Spawn {
        name: String,
        probe: Arc<Probe>,
        shutdown: Shutdown,
        restart: RestartType,
        vetoes: usize,
    },
    /// `terminate_child`, the wait dropped after `wait`.
    TerminateAbandon {
        name: String,
        wait: Duration,
    },
    /// `stop_child`, the wait dropped after `wait`.
    StopAbandon {
        name: String,
        wait: Duration,
    },
    /// `terminate_child`, awaited.
    Terminate {
        name: String,
    },
    /// `stop_child`, awaited.
    Stop {
        name: String,
    },
    RestartChild {
        name: String,
    },
    /// Answers `Ok` once `children()` reads the child as alive, which it does
    /// only after the supervisor adopted the child's incarnation.
    IsAlive {
        name: String,
    },
    /// One handler: an abandoned `terminate_child`, then an awaited one.
    /// Replies whether the child's instance was already gone on return.
    /// One handler: `terminate_child` (a member the group awaits), then
    /// `delete_child`, then `spawn_child` under the same name, then
    /// `terminate_child` of the new child, bounded by two seconds.
    OverrideDeleteRespawnTerminate {
        name: String,
        probe: Arc<Probe>,
    },
    TerminateTwice {
        name: String,
        probe: Arc<Probe>,
    },
    /// The steps run one after the other in one handler; each leaves one
    /// line in the reply.
    Script(Vec<Op>),
    /// Spawns a supervisor child of the same kind, with its own records.
    SpawnSupervisorChild {
        name: String,
        shutdown: Shutdown,
        restart: RestartType,
        config: SupervisionConfig,
        built: Arc<AtomicUsize>,
        events: Events,
        knobs: Arc<Knobs>,
    },
    /// Spawns a child with its own kill grace.
    SpawnWithGrace {
        name: String,
        probe: Arc<Probe>,
        shutdown: Shutdown,
        kill_grace: Duration,
    },
    /// Spawns a child with a start timeout, named or anonymous; replies with
    /// the child's id.
    SpawnWith {
        name: Option<String>,
        probe: Arc<Probe>,
        shutdown: Shutdown,
        restart: RestartType,
        vetoes: usize,
        start_timeout: Option<Duration>,
    },
}

/// One step of a script run inside a single handler of the supervisor.
#[derive(Clone)]
enum Op {
    Terminate(String),
    Stop(String),
    /// `stop_child`, the outcome printed.
    StopOutcome(String),
    /// `stop_child`, the wait dropped at once.
    StopAbandon(String),
    /// `stop_child`, the outcome printed: the call is polled once, so its
    /// commit is made, then the probe's `pre_stop` gate is opened, then the
    /// call is awaited.
    StopOutcomeThenOpen(String, Arc<Probe>),
    /// `terminate_child`: the call is polled once, so its commit is made,
    /// then the probe's `pre_stop` gate is opened, then the call is awaited.
    TerminateThenOpen(String, Arc<Probe>),
    /// `stop_child`: the call is polled once, so its commit is made, then the
    /// probe's held turn is ended, then the call is awaited.
    StopThenEndTurn(String, Arc<Probe>),
    /// Polls `children()` every 5 ms, for at most one second, until the child
    /// is alive.
    UntilAlive(String),
    Restart(String),
    Delete(String),
    Sleep(Duration),
    /// Stops the child through its own handle with `Kill`, bounded by 200 ms.
    KillThroughHandle(String),
    /// Polls `children()` every 5 ms, for at most one second, until the
    /// child is no longer alive.
    UntilNotAlive(String),
    /// Reads what `children()` says of the child.
    Read(String),
    /// `terminate_child`, the wait dropped at once.
    TerminateAbandon(String),
    /// `terminate_child`, the wait dropped after this long; the line says
    /// whether the call returned in time.
    TerminateWithin(String, Duration),
    /// Sends the child a message that fails it after a slow turn.
    SlowFail(String),
    /// Sends the child a message that fails it once the probe's held turn
    /// is ended.
    HeldFail(String),
    /// Sends the child a message and waits for its answer, which an instance
    /// gives only after its initialization was acknowledged.
    Ping(String),
    /// Yields to the runtime this many times.
    Yield(usize),
    /// Polls every 5 ms, for at most one second, until an instance of the
    /// probe entered its slow failing turn.
    UntilInTurn(Arc<Probe>),
    /// Polls every 5 ms, for at most two seconds, until the probe's instances
    /// finished starting this many times.
    UntilStarted(Arc<Probe>, usize),
    /// Sends the child a crashing message and waits until its task ended.
    CrashAndWait(String),
    /// The supervisor raises its own stop, `Graceful`.
    StopSelf,
    /// Lets the probe's held instance fail its initialization, waits until
    /// it is about to return the failure, and reads whether the given
    /// instance has ended once the supervisor runs again.
    FailInitAndAwaitSignal(Arc<Probe>, ActorHandle<Probed>),
    /// Spawns a child under the name, with its own probe.
    Spawn {
        name: String,
        probe: Arc<Probe>,
        shutdown: Shutdown,
        restart: RestartType,
        vetoes: usize,
    },
    /// Sets `entered`, then waits, polling every 5 ms for at most ten
    /// seconds, until `release` is set.
    Hold {
        entered: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
    },
}

#[derive(Debug)]
enum Reply {
    Done(Result<(), String>),
    GoneOnReturn(Result<(), String>, bool),
    /// The answers of the steps of `OverrideDeleteRespawnTerminate`.
    Steps(Vec<String>),
    /// The id of a spawned child.
    Id(String),
}

fn text<T>(result: Result<T, tokio_actors::SupervisionError>) -> Result<(), String> {
    result.map(|_| ()).map_err(|err| err.to_string())
}

/// The text a probed child's factory panics with when it is dropped.
const FACTORY_DROP_PANIC: &str = "the child's factory panics when dropped";

/// The text a probed child's instance panics with when it is dropped.
const INSTANCE_DROP_PANIC: &str = "the child's instance panics when dropped";

/// What a probed child's factory captures: its drop panics while the probe's
/// `factory_drop_panics` is set.
struct FactoryCapture(Arc<Probe>);

impl Drop for FactoryCapture {
    fn drop(&mut self) {
        if self.0.factory_drop_panics.load(Ordering::SeqCst) && !std::thread::panicking() {
            panic!("{FACTORY_DROP_PANIC}");
        }
    }
}

/// The factory of a probed child.
fn factory_of(probe: Arc<Probe>, vetoes: usize) -> impl Fn() -> Probed + Send + Sync + 'static {
    let capture = FactoryCapture(probe);
    move || capture.0.instance(vetoes)
}

impl Actor for Sup {
    type Message = Cmd;
    type Response = Reply;

    async fn handle(&mut self, cmd: Cmd, ctx: &mut ActorContext<Self>) -> ActorResult<Reply> {
        let reply = match cmd {
            Cmd::Spawn {
                name,
                probe,
                shutdown,
                restart,
                vetoes,
            } => {
                let result = ctx
                    .spawn_child(factory_of(probe, vetoes))
                    .named(name)
                    .restart_type(restart)
                    .shutdown(shutdown)
                    .await;
                Reply::Done(result.map(|_| ()).map_err(|err| err.to_string()))
            }
            Cmd::TerminateAbandon { name, wait } => {
                let _ = timeout(wait, ctx.terminate_child(name)).await;
                Reply::Done(Ok(()))
            }
            Cmd::StopAbandon { name, wait } => {
                let _ = timeout(wait, ctx.stop_child(name)).await;
                Reply::Done(Ok(()))
            }
            Cmd::Terminate { name } => Reply::Done(text(ctx.terminate_child(name).await)),
            Cmd::Stop { name } => Reply::Done(text(ctx.stop_child(name).await)),
            Cmd::RestartChild { name } => Reply::Done(text(ctx.restart_child(name))),
            Cmd::IsAlive { name } => {
                let alive = ctx
                    .children()
                    .iter()
                    .any(|info| info.id.as_str() == name && info.is_alive);
                Reply::Done(if alive {
                    Ok(())
                } else {
                    Err("not alive".to_string())
                })
            }
            Cmd::OverrideDeleteRespawnTerminate { name, probe } => {
                let started = Instant::now();
                let mut steps = vec![format!(
                    "{:?}",
                    text(ctx.terminate_child(name.clone()).await)
                )];
                steps.push(format!("{:?}", text(ctx.delete_child(name.clone()))));
                let respawn_probe = probe.clone();
                let respawned = ctx
                    .spawn_child(move || respawn_probe.instance(0))
                    .named(name.clone())
                    .restart_type(RestartType::Permanent)
                    .shutdown(Shutdown::Timeout(Duration::from_millis(200)))
                    .await;
                steps.push(format!("{:?}", respawned.map(|_| ())));
                let last = Instant::now();
                let stopped = timeout(Duration::from_secs(2), ctx.terminate_child(name)).await;
                steps.push(match stopped {
                    Ok(result) => format!("{:?} after {:?}", text(result), last.elapsed()),
                    Err(_) => "the last terminate_child did not return in two seconds".to_string(),
                });
                steps.push(format!("total {:?}", started.elapsed()));
                Reply::Steps(steps)
            }
            Cmd::Script(ops) => {
                let mut lines = Vec::new();
                for op in ops {
                    let line = match op {
                        Op::Terminate(name) => {
                            format!("{:?}", text(ctx.terminate_child(name).await))
                        }
                        Op::Stop(name) => format!("{:?}", text(ctx.stop_child(name).await)),
                        Op::StopOutcome(name) => format!(
                            "{:?}",
                            ctx.stop_child(name).await.map_err(|err| err.to_string())
                        ),
                        Op::StopAbandon(name) => {
                            let _ = timeout(Duration::ZERO, ctx.stop_child(name)).await;
                            "abandoned".to_string()
                        }
                        Op::StopOutcomeThenOpen(name, probe) => {
                            let mut call = std::pin::pin!(ctx.stop_child(name));
                            std::future::poll_fn(|cx| {
                                let _ = std::future::Future::poll(call.as_mut(), cx);
                                std::task::Poll::Ready(())
                            })
                            .await;
                            probe.pre_stop_gate.store(false, Ordering::SeqCst);
                            format!("{:?}", call.await.map_err(|err| err.to_string()))
                        }
                        Op::TerminateThenOpen(name, probe) => {
                            let mut call = std::pin::pin!(ctx.terminate_child(name));
                            std::future::poll_fn(|cx| {
                                let _ = std::future::Future::poll(call.as_mut(), cx);
                                std::task::Poll::Ready(())
                            })
                            .await;
                            probe.pre_stop_gate.store(false, Ordering::SeqCst);
                            format!("{:?}", call.await.map_err(|err| err.to_string()))
                        }
                        Op::StopThenEndTurn(name, probe) => {
                            let mut call = std::pin::pin!(ctx.stop_child(name));
                            std::future::poll_fn(|cx| {
                                let _ = std::future::Future::poll(call.as_mut(), cx);
                                std::task::Poll::Ready(())
                            })
                            .await;
                            probe.turn_held.store(false, Ordering::SeqCst);
                            format!("{:?}", text(call.await))
                        }
                        Op::UntilAlive(name) => {
                            let deadline = Instant::now() + Duration::from_secs(1);
                            loop {
                                let alive = ctx
                                    .children()
                                    .iter()
                                    .any(|info| info.id.as_str() == name && info.is_alive);
                                if alive {
                                    break "alive".to_string();
                                }
                                if Instant::now() >= deadline {
                                    break "not alive after one second".to_string();
                                }
                                sleep(Duration::from_millis(5)).await;
                            }
                        }
                        Op::Restart(name) => format!("{:?}", text(ctx.restart_child(name))),
                        Op::Delete(name) => format!("{:?}", text(ctx.delete_child(name))),
                        Op::Sleep(wait) => {
                            sleep(wait).await;
                            "slept".to_string()
                        }
                        Op::KillThroughHandle(name) => {
                            let handle = self
                                .system
                                .clone()
                                .unwrap_or_else(ActorSystem::default)
                                .get::<Probed>(&name)
                                .expect("the child is registered");
                            let outcome = handle
                                .stop_within(StopReason::Kill, Duration::from_millis(200))
                                .await;
                            format!("{outcome:?}")
                        }
                        Op::UntilNotAlive(name) => {
                            let deadline = Instant::now() + Duration::from_secs(1);
                            loop {
                                let alive = ctx
                                    .children()
                                    .iter()
                                    .any(|info| info.id.as_str() == name && info.is_alive);
                                if !alive {
                                    break "not alive".to_string();
                                }
                                if Instant::now() >= deadline {
                                    break "still alive after one second".to_string();
                                }
                                sleep(Duration::from_millis(5)).await;
                            }
                        }
                        Op::CrashAndWait(name) => {
                            let handle = self
                                .system
                                .clone()
                                .unwrap_or_else(ActorSystem::default)
                                .get::<Probed>(&name)
                                .expect("the child is registered");
                            let _ = handle.notify(ChildMsg::Crash).await;
                            handle.wait_stopped().await;
                            "ended".to_string()
                        }
                        Op::StopSelf => {
                            let _ = ctx.self_handle().stop(StopReason::Graceful).await;
                            "stop raised".to_string()
                        }
                        Op::FailInitAndAwaitSignal(probe, handle) => {
                            let signal = Arc::new(tokio::sync::Notify::new());
                            *probe.fail_signal.lock().unwrap() = Some(signal.clone());
                            probe.fail_init.store(true, Ordering::SeqCst);
                            probe.held.store(false, Ordering::SeqCst);
                            signal.notified().await;
                            format!(
                                "signalled ended={}",
                                handle.status() == ActorStatus::Stopped
                            )
                        }
                        Op::TerminateAbandon(name) => {
                            let _ = timeout(Duration::ZERO, ctx.terminate_child(name)).await;
                            "abandoned".to_string()
                        }
                        Op::TerminateWithin(name, bound) => {
                            match timeout(bound, ctx.terminate_child(name)).await {
                                Ok(result) => format!("{:?}", text(result)),
                                Err(_) => format!("the call did not return within {bound:?}"),
                            }
                        }
                        Op::SlowFail(name) => {
                            let handle = self
                                .system
                                .clone()
                                .unwrap_or_else(ActorSystem::default)
                                .get::<Probed>(&name)
                                .expect("the child is registered");
                            format!("{:?}", handle.notify(ChildMsg::SlowFail).await.is_ok())
                        }
                        Op::HeldFail(name) => {
                            let handle = self
                                .system
                                .clone()
                                .unwrap_or_else(ActorSystem::default)
                                .get::<Probed>(&name)
                                .expect("the child is registered");
                            format!("{:?}", handle.notify(ChildMsg::LongFail).await.is_ok())
                        }
                        Op::Yield(times) => {
                            for _ in 0..times {
                                tokio::task::yield_now().await;
                            }
                            "yielded".to_string()
                        }
                        Op::Ping(name) => {
                            let handle = ActorSystem::default()
                                .get::<Probed>(&name)
                                .expect("the child is registered");
                            format!("{:?}", handle.send(ChildMsg::Ping).await.is_ok())
                        }
                        Op::UntilInTurn(probe) => {
                            let deadline = Instant::now() + Duration::from_secs(1);
                            while !probe.in_turn.load(Ordering::SeqCst) {
                                if Instant::now() >= deadline {
                                    break;
                                }
                                sleep(Duration::from_millis(5)).await;
                            }
                            format!("in turn: {}", probe.in_turn.load(Ordering::SeqCst))
                        }
                        Op::UntilStarted(probe, count) => {
                            let deadline = Instant::now() + Duration::from_secs(2);
                            while probe.started.load(Ordering::SeqCst) < count {
                                if Instant::now() >= deadline {
                                    break;
                                }
                                sleep(Duration::from_millis(5)).await;
                            }
                            format!("started: {}", probe.started.load(Ordering::SeqCst))
                        }
                        Op::Spawn {
                            name,
                            probe,
                            shutdown,
                            restart,
                            vetoes,
                        } => {
                            let result = ctx
                                .spawn_child(factory_of(probe, vetoes))
                                .named(name)
                                .restart_type(restart)
                                .shutdown(shutdown)
                                .await;
                            format!("{:?}", result.map(|_| ()).map_err(|err| err.to_string()))
                        }
                        Op::Read(name) => ctx
                            .children()
                            .iter()
                            .find(|info| info.id.as_str() == name)
                            .map_or("no such child".to_string(), |info| {
                                format!("alive={} pending={}", info.is_alive, info.restart_pending)
                            }),
                        Op::Hold { entered, release } => {
                            entered.store(true, Ordering::SeqCst);
                            let deadline = Instant::now() + Duration::from_secs(10);
                            while !release.load(Ordering::SeqCst) && Instant::now() < deadline {
                                sleep(Duration::from_millis(5)).await;
                            }
                            format!("released: {}", release.load(Ordering::SeqCst))
                        }
                    };
                    lines.push(line);
                }
                Reply::Steps(lines)
            }
            Cmd::SpawnSupervisorChild {
                name,
                shutdown,
                restart,
                config,
                built,
                events,
                knobs,
            } => {
                let system = self.system.clone();
                let result = ctx
                    .spawn_child(move || {
                        built.fetch_add(1, Ordering::SeqCst);
                        Sup {
                            events: events.clone(),
                            stop_on_restart: false,
                            yield_on_stopped: false,
                            deliver_delay_ms: 0,
                            system: system.clone(),
                            knobs: knobs.clone(),
                        }
                    })
                    .named(name)
                    .restart_type(restart)
                    .shutdown(shutdown)
                    .with_config(ActorConfig::default().with_supervision(config))
                    .await;
                Reply::Done(result.map(|_| ()).map_err(|err| err.to_string()))
            }
            Cmd::SpawnWithGrace {
                name,
                probe,
                shutdown,
                kill_grace,
            } => {
                let result = ctx
                    .spawn_child(move || probe.instance(0))
                    .named(name)
                    .restart_type(RestartType::Permanent)
                    .shutdown(shutdown)
                    .kill_grace(kill_grace)
                    .await;
                Reply::Done(result.map(|_| ()).map_err(|err| err.to_string()))
            }
            Cmd::SpawnWith {
                name,
                probe,
                shutdown,
                restart,
                vetoes,
                start_timeout,
            } => {
                let mut builder = ctx
                    .spawn_child(factory_of(probe, vetoes))
                    .restart_type(restart)
                    .shutdown(shutdown);
                if let Some(name) = name {
                    builder = builder.named(name);
                }
                if let Some(start_timeout) = start_timeout {
                    builder = builder.start_timeout(start_timeout);
                }
                match builder.await {
                    Ok(handle) => Reply::Id(handle.id().as_str().to_string()),
                    Err(err) => Reply::Done(Err(err.to_string())),
                }
            }
            Cmd::TerminateTwice { name, probe } => {
                let _ = timeout(Duration::ZERO, ctx.terminate_child(name.clone())).await;
                let result = text(ctx.terminate_child(name).await);
                Reply::GoneOnReturn(result, probe.ended.load(Ordering::SeqCst))
            }
        };
        Ok(reply)
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.events.lock().unwrap().push(event.clone());
        self.knobs.order.lock().unwrap().push(format!(
            "delivery:{}:{:?}:{:?}",
            event.child_name.as_deref().unwrap_or("?"),
            event.action,
            ctx.status()
        ));
        if self.deliver_delay_ms > 0 {
            sleep(Duration::from_millis(self.deliver_delay_ms)).await;
        }
        let busy = {
            let mut slot = self.knobs.busy_for.lock().unwrap();
            match slot.as_ref() {
                Some((name, _)) if event.child_name.as_deref() == Some(name.as_str()) => {
                    slot.take()
                }
                _ => None,
            }
        };
        if let Some((name, millis)) = busy {
            sleep(Duration::from_millis(millis)).await;
            self.knobs
                .order
                .lock()
                .unwrap()
                .push(format!("end:{name}:{}", Probe::next_in_order()));
        }
        if self.stop_on_restart && event.action == SupervisionAction::RestartInitiated {
            ctx.self_handle()
                .stop(StopReason::Graceful)
                .await
                .expect("the supervisor stops itself");
        }
        let fault = self
            .knobs
            .faults
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Fault::Pass);
        match fault {
            Fault::Pass => Ok(()),
            Fault::Err(text) => Err(ActorError::user(text)),
            Fault::Panic => panic!("the delivery panics"),
            Fault::KillSelf => {
                let _ = ctx.self_handle().stop(StopReason::Kill).await;
                Ok(())
            }
            Fault::Terminate(name) => {
                let answer = text(ctx.terminate_child(name.clone()).await);
                self.knobs
                    .order
                    .lock()
                    .unwrap()
                    .push(format!("terminate:{name}:{answer:?}"));
                Ok(())
            }
            Fault::Calls(name) => {
                let mut lines = Vec::new();
                lines.push(format!(
                    "stop_child {:?}",
                    text(ctx.stop_child(name.clone()).await)
                ));
                lines.push(format!(
                    "terminate_child {:?}",
                    text(ctx.terminate_child(name.clone()).await)
                ));
                lines.push(format!(
                    "restart_child {:?}",
                    text(ctx.restart_child(name.clone()))
                ));
                lines.push(format!(
                    "delete_child {:?}",
                    text(ctx.delete_child(name.clone()))
                ));
                let spawned = ctx
                    .spawn_child(|| Probe::default().into_instance())
                    .named(format!("{name}-spawned"))
                    .await;
                lines.push(format!("spawn_child {:?}", spawned.map(|_| ())));
                lines.push(format!("children {}", ctx.children().len()));
                self.knobs
                    .order
                    .lock()
                    .unwrap()
                    .push(format!("calls:{}", lines.join("|")));
                Ok(())
            }
        }
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.knobs
            .order
            .lock()
            .unwrap()
            .push("on_stopped".to_string());
        self.knobs.stopped.lock().unwrap().push(reason.clone());
        self.knobs
            .last_errors
            .lock()
            .unwrap()
            .push(ctx.last_error().map(|err| err.to_string()));
        if self.yield_on_stopped {
            tokio::task::yield_now().await;
        }
        Ok(())
    }
}

struct Rig {
    sup: ActorHandle<Sup>,
    events: Events,
    knobs: Arc<Knobs>,
}

/// What a test sets on the supervisor it spawns.
#[derive(Clone, Default)]
struct SupOpts {
    stop_on_restart: bool,
    yield_on_stopped: bool,
    deliver_delay_ms: u64,
}

/// The one place a test supervisor is spawned: on `sys` (the default system
/// when none), under `name`.
async fn spawn_sup(
    sys: Option<&Arc<ActorSystem>>,
    name: &str,
    config: SupervisionConfig,
    opts: SupOpts,
) -> Rig {
    let events: Events = Arc::new(Mutex::new(Vec::new()));
    let knobs = Arc::new(Knobs::default());
    let builder = Sup {
        events: events.clone(),
        stop_on_restart: opts.stop_on_restart,
        yield_on_stopped: opts.yield_on_stopped,
        deliver_delay_ms: opts.deliver_delay_ms,
        system: sys.cloned(),
        knobs: knobs.clone(),
    }
    .spawn()
    .named(name)
    .with_supervision(config);
    let sup = match sys {
        Some(sys) => builder.on_system(sys).await,
        None => builder.await,
    }
    .expect("supervisor spawn");
    Rig { sup, events, knobs }
}

async fn supervisor(config: SupervisionConfig) -> Rig {
    spawn_sup(None, &uname("sup"), config, SupOpts::default()).await
}

fn plain() -> SupervisionConfig {
    SupervisionConfig::one_for_one().max_restarts(5, Duration::from_secs(60))
}

impl Rig {
    async fn command(&self, cmd: Cmd) -> Reply {
        within(BOUND, "the supervisor's reply", self.sup.send(cmd))
            .await
            .expect("the supervisor answers")
    }

    async fn ok(&self, cmd: Cmd) {
        match self.command(cmd).await {
            Reply::Done(Ok(())) => {}
            other => panic!("expected Done(Ok), got {other:?}"),
        }
    }

    async fn spawn_child(
        &self,
        name: &str,
        probe: &Arc<Probe>,
        shutdown: Shutdown,
        restart: RestartType,
        vetoes: usize,
    ) {
        self.ok(Cmd::Spawn {
            name: name.to_string(),
            probe: probe.clone(),
            shutdown,
            restart,
            vetoes,
        })
        .await;
    }

    /// Runs `ops` in one handler and returns the line each step left.
    async fn script(&self, ops: Vec<Op>) -> Vec<String> {
        match self.command(Cmd::Script(ops)).await {
            Reply::Steps(lines) => lines,
            other => panic!("expected the steps of a script, got {other:?}"),
        }
    }

    /// Calls `restart_child` from a handler until it is not refused for a
    /// pending group restart, bounded; returns the answer that ended it.
    async fn restart_child_when_the_group_completes(&self, name: &str) -> Vec<String> {
        let deadline = Instant::now() + BOUND;
        loop {
            let lines = self.script(vec![Op::Restart(name.to_string())]).await;
            if !lines[0].contains("is restarting") {
                return lines;
            }
            assert!(
                Instant::now() < deadline,
                "restart_child was refused until the bound"
            );
            sleep(Duration::from_millis(5)).await;
        }
    }

    fn events_for(&self, name: &str) -> Vec<ChildEvent> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.child_name.as_deref() == Some(name))
            .cloned()
            .collect()
    }
}

/// A child that vetoes every request to stop.
fn always_vetoes() -> usize {
    usize::MAX / 2
}

/// Sends a crashing message to a child and waits, bounded, for its answer.
async fn crash<A: Actor<Message = ChildMsg>>(child: &ActorHandle<A>) {
    let _ = within(
        BOUND,
        "the child's answer to a crash",
        child.send(ChildMsg::Crash),
    )
    .await;
}

async fn child_handle(name: &str) -> ActorHandle<Probed> {
    let sys = ActorSystem::default();
    let mut found = None;
    wait_until(BOUND, "the child to be registered", || {
        found = sys.get::<Probed>(name);
        found.as_ref().is_some_and(|handle| handle.is_alive())
    })
    .await;
    found.expect("registered")
}

fn removed_once(rig: &Rig, name: &str) {
    let events = rig.events_for(name);
    assert_eq!(events.len(), 1, "exactly one report: {events:?}");
    assert_eq!(events[0].action, SupervisionAction::Removed, "{events:?}");
}

// ---------------------------------------------------------------------------
// The stop of a child belongs to its runner
// ---------------------------------------------------------------------------

/// The window of the vetoing children below.
const WINDOW: Duration = Duration::from_millis(200);

/// The longest a vetoing child under `WINDOW` may live after its stop was
/// ordered: the window, the Kill step, the abort and a margin.
fn stop_bound(paused: bool) -> Duration {
    upper(
        paused,
        WINDOW + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE + Duration::from_millis(500),
    )
}

async fn abandoned_terminate_still_stops(wait: Duration, park: bool, paused: bool) {
    let rig = supervisor(plain()).await;
    let name = uname("veto");
    let probe = Arc::new(Probe::default());
    rig.spawn_child(
        &name,
        &probe,
        Shutdown::Timeout(WINDOW),
        RestartType::Permanent,
        always_vetoes(),
    )
    .await;
    let child = child_handle(&name).await;
    if park {
        child.notify(ChildMsg::Park).await.expect("park the child");
        sleep(Duration::from_millis(50)).await;
    }
    let started = Instant::now();
    rig.ok(Cmd::TerminateAbandon {
        name: name.clone(),
        wait,
    })
    .await;
    // The bound counts from the command, not from the end of the abandoned
    // wait.
    wait_until(
        stop_bound(paused)
            .saturating_sub(started.elapsed())
            .max(Duration::from_millis(1)),
        "the abandoned stop to end the child",
        || probe.ended.load(Ordering::SeqCst),
    )
    .await;
    assert!(
        started.elapsed() >= WINDOW,
        "the child lived through its window: {:?}",
        started.elapsed()
    );
    assert!(
        started.elapsed() < stop_bound(paused),
        "the child's task ended within its window, the Kill step, the abort and a margin: {:?}",
        started.elapsed()
    );
    wait_until(BOUND, "the report of the child's end", || {
        !rig.events_for(&name).is_empty()
    })
    .await;
    sleep(QUIET).await;
    removed_once(&rig, &name);
}

#[test]
fn terminate_child_abandoned_at_commit_still_stops_a_vetoing_child() {
    // The wait is dropped at the commit: the supervisor's handler polls it
    // once, which commits, and abandons it.
    common::run(
        0,
        abandoned_terminate_still_stops(Duration::ZERO, false, false),
    );
}

#[test]
fn terminate_child_abandoned_at_commit_still_stops_a_vetoing_child_paused() {
    common::run_paused(abandoned_terminate_still_stops(Duration::ZERO, false, true));
}

#[test]
fn terminate_child_abandoned_mid_window_still_stops_a_vetoing_child() {
    common::run(
        0,
        abandoned_terminate_still_stops(Duration::from_millis(100), false, false),
    );
}

#[test]
fn terminate_child_abandoned_mid_window_still_stops_a_vetoing_child_paused() {
    common::run_paused(abandoned_terminate_still_stops(
        Duration::from_millis(100),
        false,
        true,
    ));
}

#[test]
fn terminate_child_abandoned_after_the_kill_step_still_aborts_a_parked_child() {
    // The child is parked at an await inside `handle`, so only the abort ends
    // it; the wait is dropped at 250 ms, after the Kill step at 200 ms and
    // before the abort at 300 ms.
    common::run(
        0,
        abandoned_terminate_still_stops(Duration::from_millis(250), true, false),
    );
}

#[test]
fn terminate_child_abandoned_after_the_kill_step_still_aborts_a_parked_child_paused() {
    common::run_paused(abandoned_terminate_still_stops(
        Duration::from_millis(250),
        true,
        true,
    ));
}

#[test]
fn manual_calls_succeed_once_an_abandoned_stop_completes() {
    common::run(
        0,
        manual_calls_succeed_once_an_abandoned_stop_completes_body(false),
    );
}

#[test]
fn manual_calls_succeed_once_an_abandoned_stop_completes_paused() {
    common::run_paused(manual_calls_succeed_once_an_abandoned_stop_completes_body(
        true,
    ));
}

async fn manual_calls_succeed_once_an_abandoned_stop_completes_body(paused: bool) {
    let rig = supervisor(plain()).await;
    let name = uname("again");
    let probe = Arc::new(Probe::default());
    rig.spawn_child(
        &name,
        &probe,
        Shutdown::Timeout(WINDOW),
        RestartType::Permanent,
        always_vetoes(),
    )
    .await;
    rig.ok(Cmd::TerminateAbandon {
        name: name.clone(),
        wait: Duration::ZERO,
    })
    .await;
    wait_until(
        stop_bound(paused),
        "the abandoned stop to end the child",
        || probe.ended.load(Ordering::SeqCst),
    )
    .await;
    wait_until(BOUND, "the report of the child's end", || {
        !rig.events_for(&name).is_empty()
    })
    .await;
    // The child is gone: every manual call succeeds from here.
    rig.ok(Cmd::Terminate { name: name.clone() }).await;
    rig.ok(Cmd::RestartChild { name: name.clone() }).await;
    wait_until(BOUND, "the child to be alive again", || {
        ActorSystem::default()
            .get::<Probed>(&name)
            .is_some_and(|handle| handle.is_alive())
    })
    .await;
    assert_eq!(probe.built.load(Ordering::SeqCst), 2);
}

#[test]
fn second_terminate_child_waits_for_an_abandoned_stop_of_the_same_child() {
    common::run(0, async {
        let rig = supervisor(plain()).await;
        let name = uname("twice");
        let probe = Arc::new(Probe::default());
        rig.spawn_child(
            &name,
            &probe,
            Shutdown::Timeout(WINDOW),
            RestartType::Permanent,
            always_vetoes(),
        )
        .await;
        let reply = rig
            .command(Cmd::TerminateTwice {
                name: name.clone(),
                probe: probe.clone(),
            })
            .await;
        match reply {
            Reply::GoneOnReturn(result, gone) => {
                assert_eq!(result, Ok(()), "the second call joins the stop in progress");
                assert!(gone, "it returns only after the child's task ended");
            }
            other => panic!("unexpected reply {other:?}"),
        }
        wait_until(BOUND, "the report of the child's end", || {
            !rig.events_for(&name).is_empty()
        })
        .await;
        sleep(QUIET).await;
        removed_once(&rig, &name);
    });
}

#[test]
fn a_second_manual_stop_asks_a_vetoing_infinity_child_once_more() {
    common::run(0, async {
        let rig = supervisor(plain()).await;
        let name = uname("infinity");
        let probe = Arc::new(Probe::default());
        // The child vetoes the first request and accepts every later one.
        rig.spawn_child(&name, &probe, Shutdown::Infinity, RestartType::Permanent, 1)
            .await;
        rig.ok(Cmd::TerminateAbandon {
            name: name.clone(),
            wait: Duration::from_millis(100),
        })
        .await;
        assert_eq!(
            probe.pre_stops.load(Ordering::SeqCst),
            1,
            "the child vetoed the first request"
        );
        assert!(!probe.ended.load(Ordering::SeqCst));
        // A later handler's stop joins the stop in progress and asks once
        // more; the child accepts.
        let answer = within(
            Duration::from_secs(2),
            "the joining stop to complete",
            rig.sup.send(Cmd::Stop { name: name.clone() }),
        )
        .await
        .expect("the supervisor answers");
        assert!(matches!(answer, Reply::Done(Ok(()))), "{answer:?}");
        assert!(probe.ended.load(Ordering::SeqCst));
        sleep(QUIET).await;
        assert_eq!(
            probe.pre_stops.load(Ordering::SeqCst),
            2,
            "`pre_stop` ran exactly twice"
        );
        wait_until(BOUND, "the report of the child's end", || {
            !rig.events_for(&name).is_empty()
        })
        .await;
        removed_once(&rig, &name);
    });
}

#[test]
fn stop_child_abandoned_at_commit_bounces_a_vetoing_permanent_child_once() {
    common::run(
        0,
        stop_child_abandoned_at_commit_bounces_a_vetoing_permanent_child_once_body(false),
    );
}

#[test]
fn stop_child_abandoned_at_commit_bounces_a_vetoing_permanent_child_once_paused() {
    common::run_paused(
        stop_child_abandoned_at_commit_bounces_a_vetoing_permanent_child_once_body(true),
    );
}

async fn stop_child_abandoned_at_commit_bounces_a_vetoing_permanent_child_once_body(paused: bool) {
    // A budget of one restart: a bounce is not a failure and charges
    // nothing, so a later real crash is still absorbed.
    let rig =
        supervisor(SupervisionConfig::one_for_one().max_restarts(1, Duration::from_secs(60))).await;
    let name = uname("bounce");
    let probe = Arc::new(Probe::default());
    rig.spawn_child(
        &name,
        &probe,
        Shutdown::Timeout(WINDOW),
        RestartType::Permanent,
        always_vetoes(),
    )
    .await;
    rig.ok(Cmd::StopAbandon {
        name: name.clone(),
        wait: Duration::ZERO,
    })
    .await;
    wait_until(
        stop_bound(paused),
        "the bounce to restart the child",
        || probe.built.load(Ordering::SeqCst) == 2,
    )
    .await;
    sleep(QUIET).await;
    assert_eq!(probe.built.load(Ordering::SeqCst), 2, "one restart");
    let events = rig.events_for(&name);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].action, SupervisionAction::RestartInitiated);
    // The budget is untouched: a real crash is restarted.
    let fresh = child_handle(&name).await;
    crash(&fresh).await;
    wait_until(BOUND, "the crashed child to be restarted", || {
        probe.built.load(Ordering::SeqCst) == 3
    })
    .await;
    assert!(rig.sup.is_alive(), "the supervisor survived the crash");
}

// ---------------------------------------------------------------------------
// The group's stop and the parent's stop belong to the runners too
// ---------------------------------------------------------------------------

#[test]
fn group_stop_escalates_a_vetoing_member_through_its_runner() {
    common::run(
        0,
        group_stop_escalates_a_vetoing_member_through_its_runner_body(false),
    );
}

#[test]
fn group_stop_escalates_a_vetoing_member_through_its_runner_paused() {
    common::run_paused(group_stop_escalates_a_vetoing_member_through_its_runner_body(true));
}

async fn group_stop_escalates_a_vetoing_member_through_its_runner_body(paused: bool) {
    let rig =
        supervisor(SupervisionConfig::one_for_all().max_restarts(5, Duration::from_secs(60))).await;
    let crasher = uname("crasher");
    let member = uname("member");
    let (crasher_probe, member_probe) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
    rig.spawn_child(
        &crasher,
        &crasher_probe,
        Shutdown::Timeout(Duration::from_secs(2)),
        RestartType::Permanent,
        0,
    )
    .await;
    rig.spawn_child(
        &member,
        &member_probe,
        Shutdown::Timeout(Duration::from_millis(150)),
        RestartType::Permanent,
        always_vetoes(),
    )
    .await;
    let started = Instant::now();
    crash(&child_handle(&crasher).await).await;
    // The member's death is bounded as the ladder gives it: its window,
    // the Kill step, the abort and a margin.
    let member_bound = upper(
        paused,
        Duration::from_millis(150)
            + ASSUMED_KILL_GRACE
            + ASSUMED_ABORT_SETTLE
            + Duration::from_millis(500),
    );
    wait_until(member_bound, "the vetoing member to die", || {
        !rig.events_for(&member).is_empty()
    })
    .await;
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "the member kept its window: {:?}",
        started.elapsed()
    );
    wait_until(BOUND, "the group to restart both members", || {
        crasher_probe.built.load(Ordering::SeqCst) == 2
            && member_probe.built.load(Ordering::SeqCst) == 2
    })
    .await;
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "the member kept its window: {:?}",
        started.elapsed()
    );
    assert!(
        member_probe.pre_stops.load(Ordering::SeqCst) >= 1,
        "the member was asked first"
    );
    let events = rig.events_for(&member);
    assert!(
        events
            .iter()
            .any(|event| matches!(event.reason, StopReason::Kill)
                && event.action == SupervisionAction::RestartInitiated),
        "the vetoing member died by the Kill its runner raised: {events:?}"
    );
}

#[test]
fn parent_stop_escalates_a_vetoing_running_child_through_its_runner() {
    common::run(
        0,
        parent_stop_escalates_a_vetoing_running_child_through_its_runner_body(false),
    );
}

#[test]
fn parent_stop_escalates_a_vetoing_running_child_through_its_runner_paused() {
    common::run_paused(parent_stop_escalates_a_vetoing_running_child_through_its_runner_body(true));
}

async fn parent_stop_escalates_a_vetoing_running_child_through_its_runner_body(paused: bool) {
    let rig = supervisor(plain()).await;
    let name = uname("parent-stop");
    let probe = Arc::new(Probe::default());
    rig.spawn_child(
        &name,
        &probe,
        Shutdown::Timeout(WINDOW),
        RestartType::Permanent,
        always_vetoes(),
    )
    .await;
    let started = Instant::now();
    within(
        BOUND,
        "the supervisor to accept the stop",
        rig.sup.stop(StopReason::Graceful),
    )
    .await
    .expect("stop");
    within(
        stop_bound(paused),
        "the supervisor to stop",
        rig.sup.wait_stopped(),
    )
    .await;
    assert!(
        probe.ended.load(Ordering::SeqCst),
        "the supervisor publishes Stopped only after its child ended"
    );
    assert!(
        started.elapsed() >= WINDOW,
        "the child kept its window: {:?}",
        started.elapsed()
    );
}

#[test]
fn a_child_respawned_under_the_name_of_an_overridden_member_is_not_a_member_of_the_group() {
    common::run(0, async {
        let rig =
            supervisor(SupervisionConfig::one_for_all().max_restarts(5, Duration::from_secs(60)))
                .await;
        let crasher = uname("crasher");
        let member = uname("member");
        let (crasher_probe, member_probe) =
            (Arc::new(Probe::default()), Arc::new(Probe::default()));
        rig.spawn_child(
            &crasher,
            &crasher_probe,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        rig.spawn_child(
            &member,
            &member_probe,
            Shutdown::Timeout(Duration::from_millis(300)),
            RestartType::Permanent,
            always_vetoes(),
        )
        .await;
        crash(&child_handle(&crasher).await).await;
        wait_until(BOUND, "the group to ask the member to stop", || {
            member_probe.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        // One handler: the member is overridden and ends, it is deleted, a
        // new child takes its name, and that child is terminated. The new
        // child is not a member of the group and is stopped like any child.
        let reply = rig
            .command(Cmd::OverrideDeleteRespawnTerminate {
                name: member.clone(),
                probe: member_probe.clone(),
            })
            .await;
        let Reply::Steps(steps) = reply else {
            panic!("unexpected reply {reply:?}");
        };
        assert_eq!(steps[0], "Ok(())", "{steps:?}");
        assert_eq!(steps[1], "Ok(())", "{steps:?}");
        assert_eq!(steps[2], "Ok(())", "{steps:?}");
        assert!(steps[3].starts_with("Ok(()) after"), "{steps:?}");
        // The group then completes: the crashed sibling is rebuilt, and each of
        // the member's two incarnations reported its end exactly once.
        wait_until(BOUND, "the group to restart the crashed sibling", || {
            crasher_probe.built.load(Ordering::SeqCst) == 2
        })
        .await;
        wait_until(BOUND, "the reports of both incarnations", || {
            rig.events_for(&member).len() >= 2
        })
        .await;
        sleep(QUIET).await;
        let reports = rig.events_for(&member);
        assert_eq!(reports.len(), 2, "{reports:?}");
        assert!(
            reports
                .iter()
                .all(|event| event.action == SupervisionAction::Removed),
            "{reports:?}"
        );
    });
}

#[test]
fn a_member_released_from_a_group_and_restarted_is_stopped_by_terminate_child() {
    common::run(
        0,
        a_member_released_from_a_group_and_restarted_is_stopped_by_terminate_child_body(false),
    );
}

#[test]
fn a_member_released_from_a_group_and_restarted_is_stopped_by_terminate_child_paused() {
    common::run_paused(
        a_member_released_from_a_group_and_restarted_is_stopped_by_terminate_child_body(true),
    );
}

async fn a_member_released_from_a_group_and_restarted_is_stopped_by_terminate_child_body(
    paused: bool,
) {
    let rig =
        supervisor(SupervisionConfig::one_for_all().max_restarts(5, Duration::from_secs(60))).await;
    let crasher = uname("crasher");
    let member = uname("member");
    let (crasher_probe, member_probe) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
    rig.spawn_child(
        &crasher,
        &crasher_probe,
        Shutdown::Timeout(Duration::from_secs(2)),
        RestartType::Permanent,
        0,
    )
    .await;
    rig.spawn_child(
        &member,
        &member_probe,
        Shutdown::Timeout(Duration::from_millis(300)),
        RestartType::Permanent,
        always_vetoes(),
    )
    .await;
    // The crasher's restart will hold in its initialization, so the
    // group stays pending while the member is released and revived.
    wait_until(BOUND, "the crasher to finish starting", || {
        crasher_probe.started.load(Ordering::SeqCst) == 1
    })
    .await;
    crasher_probe.held.store(true, Ordering::SeqCst);
    crash(&child_handle(&crasher).await).await;
    wait_until(BOUND, "the group to ask the member to stop", || {
        member_probe.pre_stops.load(Ordering::SeqCst) == 1
    })
    .await;
    rig.ok(Cmd::Terminate {
        name: member.clone(),
    })
    .await;
    // The released member belongs to the pending group until it
    // completes: restart_child is refused and the member stays down.
    match rig
        .command(Cmd::RestartChild {
            name: member.clone(),
        })
        .await
    {
        Reply::Done(Err(text)) => assert!(text.contains("is restarting"), "{text}"),
        other => panic!("expected the refusal of restart_child, got {other:?}"),
    }
    assert_eq!(member_probe.built.load(Ordering::SeqCst), 1);
    // The group still pends: the crashed sibling's restart is held in its
    // initialization, so the sibling is not running yet.
    assert!(
        matches!(
            rig.command(Cmd::IsAlive {
                name: crasher.clone()
            })
            .await,
            Reply::Done(Err(_))
        ),
        "the group must still be pending at the final call"
    );
    // Once the hold is lifted the group ends: the sibling's restart is
    // adopted and the sibling runs again.
    crasher_probe.held.store(false, Ordering::SeqCst);
    wait_until(BOUND, "the crasher to be built again", || {
        crasher_probe.built.load(Ordering::SeqCst) == 2
    })
    .await;
    wait_for_alive(&rig, &crasher).await;
    assert_eq!(crasher_probe.started.load(Ordering::SeqCst), 2);
    // The group's pending marker clears just after the crasher's
    // adoption: restart_child is refused until it does, and then revives
    // the member.
    let deadline = Instant::now() + BOUND;
    loop {
        match rig
            .command(Cmd::RestartChild {
                name: member.clone(),
            })
            .await
        {
            Reply::Done(Ok(())) => break,
            Reply::Done(Err(text)) if text.contains("is restarting") => {
                assert!(
                    Instant::now() < deadline,
                    "restart_child was refused until the bound"
                );
                sleep(Duration::from_millis(5)).await;
            }
            other => panic!("unexpected answer {other:?}"),
        }
    }
    wait_until(BOUND, "the member to be built again", || {
        member_probe.built.load(Ordering::SeqCst) == 2
    })
    .await;
    wait_for_alive(&rig, &member).await;
    // The revived member is a running child like any other.
    let answer = within(
        stop_bound(paused),
        "the revived member to be stopped",
        rig.sup.send(Cmd::Terminate {
            name: member.clone(),
        }),
    )
    .await
    .expect("the supervisor answers");
    assert!(matches!(answer, Reply::Done(Ok(()))), "{answer:?}");
}

#[test]
fn terminate_child_override_runs_pre_stop_once() {
    common::run(0, async {
        let rig =
            supervisor(SupervisionConfig::one_for_all().max_restarts(5, Duration::from_secs(60)))
                .await;
        let crasher = uname("crasher");
        let member = uname("member");
        let (crasher_probe, member_probe) =
            (Arc::new(Probe::default()), Arc::new(Probe::default()));
        rig.spawn_child(
            &crasher,
            &crasher_probe,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        rig.spawn_child(
            &member,
            &member_probe,
            Shutdown::Timeout(Duration::from_millis(300)),
            RestartType::Permanent,
            always_vetoes(),
        )
        .await;
        crash(&child_handle(&crasher).await).await;
        wait_until(BOUND, "the group to ask the member to stop", || {
            member_probe.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        // The member is being stopped by the group. A terminate of it records
        // that it stays down and waits; nothing is raised again.
        match rig
            .command(Cmd::Terminate {
                name: member.clone(),
            })
            .await
        {
            Reply::Done(Ok(())) => {}
            other => panic!("the override must succeed, got {other:?}"),
        }
        assert!(member_probe.ended.load(Ordering::SeqCst));
        sleep(QUIET).await;
        assert_eq!(
            member_probe.pre_stops.load(Ordering::SeqCst),
            1,
            "`pre_stop` ran exactly once before the Kill"
        );
        wait_until(BOUND, "the crashed sibling to be restarted", || {
            crasher_probe.built.load(Ordering::SeqCst) == 2
        })
        .await;
        assert_eq!(
            member_probe.built.load(Ordering::SeqCst),
            1,
            "the member stays down"
        );
        let events = rig.events_for(&member);
        assert!(
            events
                .iter()
                .all(|event| event.action == SupervisionAction::Removed),
            "{events:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// Calls on a child whose restart is in flight, and on a stuck child
// ---------------------------------------------------------------------------

/// A child whose factory blocks, so its restart attempt stays in flight.
struct Slow {
    gate: Arc<AtomicBool>,
}

impl Actor for Slow {
    type Message = ChildMsg;
    type Response = ();

    async fn handle(&mut self, msg: ChildMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ChildMsg::Park => {
                std::future::pending::<()>().await;
                Ok(())
            }
            ChildMsg::Crash | ChildMsg::SlowFail | ChildMsg::LongFail | ChildMsg::Block => {
                panic!("child crashed on command")
            }
            ChildMsg::Ping => Ok(()),
        }
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        // A restarted instance waits here until the test opens the gate.
        while !self.gate.load(Ordering::SeqCst) {
            sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    }
}

struct SlowSup {
    gate: Arc<AtomicBool>,
    built: Arc<AtomicUsize>,
}

#[derive(Clone)]
enum SlowCmd {
    Spawn(String),
    Calls(String),
}

impl Actor for SlowSup {
    type Message = SlowCmd;
    type Response = Vec<String>;

    async fn handle(
        &mut self,
        cmd: SlowCmd,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<Vec<String>> {
        match cmd {
            SlowCmd::Spawn(name) => {
                let gate = self.gate.clone();
                let built = self.built.clone();
                ctx.spawn_child(move || {
                    // The first instance starts at once; later ones wait.
                    if built.fetch_add(1, Ordering::SeqCst) == 0 {
                        gate.store(true, Ordering::SeqCst);
                    } else {
                        gate.store(false, Ordering::SeqCst);
                    }
                    Slow { gate: gate.clone() }
                })
                .named(name)
                .restart_type(RestartType::Permanent)
                .shutdown(Shutdown::Timeout(Duration::from_millis(200)))
                .await?;
                Ok(Vec::new())
            }
            SlowCmd::Calls(name) => {
                let terminate = ctx.terminate_child(name.clone()).await;
                let stop = ctx.stop_child(name.clone()).await;
                let restart = ctx.restart_child(name.clone());
                let delete = ctx.delete_child(name);
                Ok(vec![
                    format!("{terminate:?}"),
                    format!("{stop:?}"),
                    format!("{restart:?}"),
                    format!("{delete:?}"),
                ])
            }
        }
    }
}

#[test]
fn manual_stops_of_a_child_whose_restart_is_in_flight_answer_child_restarting() {
    common::run(0, async {
        let gate = Arc::new(AtomicBool::new(true));
        let built = Arc::new(AtomicUsize::new(0));
        let sup = SlowSup {
            gate: gate.clone(),
            built: built.clone(),
        }
        .spawn()
        .named(uname("slow-sup"))
        .with_supervision(plain())
        .await
        .expect("supervisor spawn");
        let name = uname("slow");
        within(
            BOUND,
            "the spawn command",
            sup.send(SlowCmd::Spawn(name.clone())),
        )
        .await
        .expect("answer")
        .len();
        let sys = ActorSystem::default();
        wait_until(BOUND, "the child", || {
            sys.get::<Slow>(&name).is_some_and(|h| h.is_alive())
        })
        .await;
        // Crash it: the restart attempt initializes and waits for the gate.
        crash(&sys.get::<Slow>(&name).unwrap()).await;
        wait_until(BOUND, "the restart attempt to be built", || {
            built.load(Ordering::SeqCst) == 2
        })
        .await;
        sleep(Duration::from_millis(50)).await;
        let answers = within(BOUND, "the calls", sup.send(SlowCmd::Calls(name.clone())))
            .await
            .expect("answer");
        assert!(
            answers[0].contains("ChildRestarting"),
            "terminate_child: {answers:?}"
        );
        assert!(
            answers[1].contains("ChildRestarting"),
            "stop_child: {answers:?}"
        );
        assert!(
            answers[2].contains("ChildRestarting"),
            "restart_child: {answers:?}"
        );
        assert!(
            answers[3].contains("ChildRestarting"),
            "delete_child: {answers:?}"
        );
        gate.store(true, Ordering::SeqCst);
        wait_until(BOUND, "the restarted child", || {
            sys.get::<Slow>(&name).is_some_and(|h| h.is_alive())
        })
        .await;
    });
}

/// The gate a `Stuck` turn holds its thread on.
#[derive(Default)]
struct StuckGate {
    /// Set by the test to end the turn.
    open: AtomicBool,
    /// Set when the turn ended.
    returned: AtomicBool,
}

/// Opens a `StuckGate` when dropped, so the stuck turn ends however the test
/// ends.
struct OpensTheStuckGate(Arc<StuckGate>);

impl Drop for OpensTheStuckGate {
    fn drop(&mut self) {
        self.0.open.store(true, Ordering::SeqCst);
    }
}

/// A child whose single turn blocks its thread until its gate opens (for at
/// most `GATE_CAP`), so no stop reaches it.
struct Stuck(Arc<StuckGate>);

impl Actor for Stuck {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let cap = std::time::Instant::now() + GATE_CAP;
        while !self.0.open.load(Ordering::SeqCst) && std::time::Instant::now() < cap {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.0.returned.store(true, Ordering::SeqCst);
        Ok(())
    }
}

struct StuckSup(Arc<StuckGate>);

impl Actor for StuckSup {
    type Message = String;
    type Response = String;

    async fn handle(&mut self, name: String, ctx: &mut ActorContext<Self>) -> ActorResult<String> {
        if name == "spawn" {
            let gate = self.0.clone();
            ctx.spawn_child(move || Stuck(gate.clone()))
                .named("stuck-child".to_string())
                .restart_type(RestartType::Temporary)
                .shutdown(Shutdown::Timeout(Duration::from_millis(50)))
                .await?;
            return Ok(String::new());
        }
        Ok(format!("{:?}", ctx.terminate_child(name).await))
    }
}

#[test]
fn terminate_child_of_a_stuck_child_answers_child_unresponsive() {
    // The child's turn blocks a worker thread, so the runner must run on
    // another one. The turn holds its thread until the test opens its gate,
    // after the checks: a call that waited for the turn would not answer
    // inside its bound, and the bound leaves seconds for a stalled host.
    common::run_multi_thread_only(4, async {
        let gate = Arc::new(StuckGate::default());
        let opens = OpensTheStuckGate(gate.clone());
        let sup = StuckSup(gate.clone())
            .spawn()
            .named(uname("stuck-sup"))
            .with_supervision(plain())
            .await
            .expect("supervisor spawn");
        let _ = sup.send("spawn".to_string()).await.expect("spawn");
        let child = ActorSystem::default()
            .get::<Stuck>("stuck-child")
            .expect("the child");
        child.notify(()).await.expect("start the stuck turn");
        sleep(Duration::from_millis(50)).await;
        let started = Instant::now();
        let ladder = Duration::from_millis(50) + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE;
        let answer = within(
            ladder + STALL_SLACK,
            "the verdict",
            sup.send("stuck-child".to_string()),
        )
        .await
        .expect("answer");
        assert!(answer.contains("ChildUnresponsive"), "{answer}");
        assert!(
            started.elapsed() >= ladder,
            "the whole ladder ran before the verdict: {:?}",
            started.elapsed()
        );
        assert!(
            !gate.returned.load(Ordering::SeqCst),
            "the verdict came while the turn held its thread"
        );
        // The child now carries the verdict: a second call on it is answered
        // at once, without another ladder.
        let answer = within(
            STALL_SLACK,
            "the second call on a child that carries the verdict",
            sup.send("stuck-child".to_string()),
        )
        .await
        .expect("answer");
        assert!(answer.contains("ChildUnresponsive"), "{answer}");
        assert!(!gate.returned.load(Ordering::SeqCst));
        drop(opens);
    });
}

// ---------------------------------------------------------------------------
// A restart attempt whose task fails is evaluated, not left restarting
// ---------------------------------------------------------------------------

/// An actor value that panics when dropped once armed.
struct Fragile {
    armed: Arc<AtomicBool>,
}

impl Actor for Fragile {
    type Message = ChildMsg;
    type Response = ();

    async fn handle(&mut self, msg: ChildMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        match msg {
            ChildMsg::Park => {
                std::future::pending::<()>().await;
                Ok(())
            }
            ChildMsg::Crash | ChildMsg::SlowFail | ChildMsg::LongFail | ChildMsg::Block => {
                panic!("child crashed on command")
            }
            ChildMsg::Ping => Ok(()),
        }
    }
}

impl Drop for Fragile {
    fn drop(&mut self) {
        if self.armed.load(Ordering::SeqCst) && !std::thread::panicking() {
            panic!("armed actor dropped");
        }
    }
}

/// What the supervisor saw inside each `on_child_stopped`.
#[derive(Default)]
struct Seen {
    reports: Vec<Report>,
}

struct Report {
    event: ChildEvent,
    /// `last_supervision_task_error()` at the time.
    task_error: Option<ActorError>,
    /// `last_error()` read `None` at the time.
    last_error_clear: bool,
}

struct FragileSup {
    armed: Arc<AtomicBool>,
    entered: Arc<AtomicBool>,
    /// Opened by the test once the name is taken: the factory of a restart
    /// attempt blocks its thread until then.
    release: Arc<(Mutex<bool>, std::sync::Condvar)>,
    built: Arc<AtomicUsize>,
    seen: Arc<Mutex<Seen>>,
}

impl Actor for FragileSup {
    type Message = String;
    type Response = ();

    async fn handle(&mut self, name: String, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let armed = self.armed.clone();
        let entered = self.entered.clone();
        let release = self.release.clone();
        let built = self.built.clone();
        ctx.spawn_child(move || {
            if built.fetch_add(1, Ordering::SeqCst) > 0 {
                // A restart attempt: the factory blocks its thread until the
                // test has taken the child's name.
                entered.store(true, Ordering::SeqCst);
                let (lock, opened) = &*release;
                let mut open = lock.lock().unwrap();
                while !*open {
                    let (guard, waited) =
                        opened.wait_timeout(open, Duration::from_secs(10)).unwrap();
                    open = guard;
                    if waited.timed_out() {
                        break;
                    }
                }
            }
            Fragile {
                armed: armed.clone(),
            }
        })
        .named(name)
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Timeout(Duration::from_secs(1)))
        .start_timeout(Duration::from_secs(1))
        .await?;
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.seen.lock().unwrap().reports.push(Report {
            event: event.clone(),
            task_error: ctx.last_supervision_task_error().cloned(),
            last_error_clear: ctx.last_error().is_none(),
        });
        Ok(())
    }
}

/// An actor that only holds a name.
#[derive(Default)]
struct Squatter;

impl Actor for Squatter {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[test]
fn restart_attempt_panic_is_evaluated_instead_of_left_restarting() {
    // The attempt's factory blocks a worker thread until the test has
    // registered a root under the child's name, so the attempt's spawn finds
    // the name taken and drops the actor value inside the attempt task,
    // outside the factory's own panic capture. The attempt task panics with
    // no incarnation spawned.
    common::run_multi_thread_only(4, async {
        let armed = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let built = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Seen::default()));
        let sup = FragileSup {
            armed: armed.clone(),
            entered: entered.clone(),
            release: release.clone(),
            built: built.clone(),
            seen: seen.clone(),
        }
        .spawn()
        .named(uname("fragile-sup"))
        // A budget of one: the crash below spends it, so the failed attempt's
        // evaluation exhausts it.
        .with_supervision(SupervisionConfig::one_for_one().max_restarts(1, Duration::from_secs(60)))
        .await
        .expect("supervisor spawn");
        let name = uname("fragile");
        within(BOUND, "the spawn command", sup.send(name.clone()))
            .await
            .expect("spawn");
        let child = ActorSystem::default()
            .get::<Fragile>(&name)
            .expect("the child");
        crash(&child).await;
        wait_until(BOUND, "the restart attempt to start", || {
            entered.load(Ordering::SeqCst)
        })
        .await;
        // The first instance is gone: arm the actor value, take the name, and
        // only then let the factory return.
        armed.store(true, Ordering::SeqCst);
        let squatter = Squatter
            .spawn()
            .named(name.clone())
            .await
            .expect("the name is free while the child is down");
        {
            let (lock, opened) = &*release;
            *lock.lock().unwrap() = true;
            opened.notify_all();
        }
        let is_attempt_report = |report: &Report| matches!(&report.event.reason, StopReason::Failure(ActorError::Panic(text)) if text.contains("armed actor dropped"));
        wait_until(
            upper(false, Duration::from_secs(1) + Duration::from_millis(500)),
            "the failed attempt to be reported",
            || seen.lock().unwrap().reports.iter().any(is_attempt_report),
        )
        .await;
        {
            let seen = seen.lock().unwrap();
            let report = seen
                .reports
                .iter()
                .find(|report| is_attempt_report(report))
                .expect("the attempt's report");
            assert_eq!(
                report.event.action,
                SupervisionAction::BudgetExhausted,
                "{:?}",
                report.event
            );
            assert!(
                matches!(&report.task_error, Some(ActorError::Panic(text)) if text.contains("armed actor dropped")),
                "the attempt task's panic is available in its own accessor: {:?}",
                report.task_error
            );
            assert!(
                report.last_error_clear,
                "the panic is not the supervisor's own error"
            );
        }
        within(
            BOUND,
            "the supervisor to stop with its budget exhausted",
            sup.wait_stopped(),
        )
        .await;
        let _ = squatter.stop(StopReason::Graceful).await;
        assert!(!sup.is_alive());
    });
}

// ---------------------------------------------------------------------------
// The ledger: a report is matched to the incarnation it is of, and a restart
// begins only from a child that is down
// ---------------------------------------------------------------------------

/// A one-for-all supervisor with a generous budget.
fn one_for_all() -> SupervisionConfig {
    SupervisionConfig::one_for_all().max_restarts(5, Duration::from_secs(60))
}

fn probes() -> (Arc<Probe>, Arc<Probe>, Arc<Probe>) {
    (
        Arc::new(Probe::default()),
        Arc::new(Probe::default()),
        Arc::new(Probe::default()),
    )
}

async fn is_alive(rig: &Rig, name: &str) -> bool {
    matches!(
        rig.command(Cmd::IsAlive {
            name: name.to_string()
        })
        .await,
        Reply::Done(Ok(()))
    )
}

async fn wait_for_alive(rig: &Rig, name: &str) {
    let deadline = Instant::now() + BOUND;
    while !is_alive(rig, name).await {
        assert!(Instant::now() < deadline, "{name} never ran");
        sleep(Duration::from_millis(5)).await;
    }
}

/// OneForAll `[a, b, d]` with `b` and `d` vetoing under `Timeout(300 ms)`
/// and `Timeout(1500 ms)`: `a` crashes, and `b` is released from the group
/// restart by `terminate_child`. With `linger`, the handler reads `children()`
/// and waits 80 ms between the two calls.
async fn restart_child_after_group_override(linger: bool) {
    let rig = supervisor(one_for_all()).await;
    let (a, b, d) = (uname("a"), uname("b"), uname("d"));
    let (pa, pb, pd) = probes();
    rig.spawn_child(
        &a,
        &pa,
        Shutdown::Timeout(Duration::from_millis(300)),
        RestartType::Permanent,
        0,
    )
    .await;
    rig.spawn_child(
        &b,
        &pb,
        Shutdown::Timeout(Duration::from_millis(300)),
        RestartType::Permanent,
        always_vetoes(),
    )
    .await;
    rig.spawn_child(
        &d,
        &pd,
        Shutdown::Timeout(Duration::from_millis(1500)),
        RestartType::Permanent,
        always_vetoes(),
    )
    .await;
    crash(&child_handle(&a).await).await;
    wait_until(BOUND, "the group to ask b to stop", || {
        pb.pre_stops.load(Ordering::SeqCst) == 1
    })
    .await;
    let mut ops = vec![Op::Terminate(b.clone())];
    if linger {
        ops.push(Op::Sleep(Duration::from_millis(80)));
        ops.push(Op::Read(b.clone()));
    }
    ops.push(Op::Restart(b.clone()));
    let lines = rig.script(ops).await;
    assert_eq!(lines[0], "Ok(())", "{lines:?}");
    if linger {
        // The released member is down and nothing will restart it.
        assert_eq!(lines[2], "alive=false pending=false", "{lines:?}");
    }
    assert!(
        lines
            .last()
            .is_some_and(|line| line.contains("is restarting")),
        "restart_child is refused while the group pends: {lines:?}"
    );
    assert_eq!(pb.built.load(Ordering::SeqCst), 1, "b was not started");
    // Once the group completed, the released member can be restarted.
    let lines = rig.restart_child_when_the_group_completes(&b).await;
    assert_eq!(lines[0], "Ok(())", "{lines:?}");
    wait_until(BOUND, "b to run again", || {
        pb.started.load(Ordering::SeqCst) == 2
    })
    .await;
    wait_for_alive(&rig, &b).await;
    sleep(QUIET).await;
    assert_eq!(pa.built.load(Ordering::SeqCst), 2);
    assert_eq!(pb.built.load(Ordering::SeqCst), 2);
    assert_eq!(pd.built.load(Ordering::SeqCst), 2);
    assert!(is_alive(&rig, &b).await, "the revived member keeps running");
    removed_once(&rig, &b);
}

#[test]
fn restart_child_after_group_override_keeps_the_revived_child_running() {
    // Without and with a linger of 80 ms between the two calls.
    common::run(0, async {
        restart_child_after_group_override(false).await;
        restart_child_after_group_override(true).await;
    });
}

#[test]
fn stale_snapshot_never_erases_a_newer_barrier_entry() {
    common::run(0, async {
        let rig = supervisor(one_for_all()).await;
        let (a, b, d) = (uname("a"), uname("b"), uname("d"));
        let (pa, pb, pd) = probes();
        rig.spawn_child(
            &a,
            &pa,
            Shutdown::Timeout(Duration::from_millis(300)),
            RestartType::Permanent,
            0,
        )
        .await;
        rig.spawn_child(
            &b,
            &pb,
            Shutdown::Timeout(Duration::from_millis(300)),
            RestartType::Permanent,
            always_vetoes(),
        )
        .await;
        rig.spawn_child(
            &d,
            &pd,
            Shutdown::Timeout(Duration::from_millis(300)),
            RestartType::Permanent,
            0,
        )
        .await;
        // One handler terminates b (its ladder runs 300 ms), revives it, and
        // waits until the new incarnation finished starting, so the new
        // incarnation is adopted before the loop drains the report of b's old
        // one.
        let sup = rig.sup.clone();
        let ops = vec![
            Op::Terminate(b.clone()),
            Op::Restart(b.clone()),
            Op::UntilStarted(pb.clone(), 2),
            // The new incarnation answers only after its start was
            // acknowledged, so its attempt's outcome is ready before the loop
            // resumes.
            Op::Ping(b.clone()),
            Op::Yield(50),
        ];
        let handler = tokio::spawn(async move { sup.send(Cmd::Script(ops)).await });
        // While the handler waits inside terminate_child(b), whose first
        // request b has vetoed, a crashes.
        wait_until(BOUND, "the handler to ask b to stop", || {
            pb.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        crash(&child_handle(&a).await).await;
        let reply = within(BOUND, "the handler's reply", handler)
            .await
            .expect("joined")
            .expect("the supervisor answers");
        let Reply::Steps(lines) = reply else {
            panic!("unexpected reply {reply:?}");
        };
        assert_eq!(&lines[..2], ["Ok(())", "Ok(())"], "{lines:?}");
        // The group stops b's new incarnation, which vetoes for its window,
        // and d, then restarts the whole group. The old report of b removes
        // no entry of the new one: the chain starts a only once b's new
        // incarnation ended.
        wait_until(BOUND, "the chain to start a", || {
            pa.built.load(Ordering::SeqCst) == 2
        })
        .await;
        assert!(
            pb.drop_order(1) < pa.build_order(1),
            "the group waited for b's new incarnation to end before it restarted a"
        );
        wait_until(BOUND, "the group to complete", || {
            pb.built.load(Ordering::SeqCst) == 3 && pd.built.load(Ordering::SeqCst) == 2
        })
        .await;
        sleep(QUIET).await;
        assert_eq!(
            pa.built.load(Ordering::SeqCst),
            2,
            "one crash of a causes exactly one group restart"
        );
        assert!(rig.sup.is_alive());
    });
}

#[test]
fn solo_terminate_then_restart_child_in_one_handler_keeps_the_child_running() {
    common::run(0, async {
        let rig = supervisor(plain()).await;
        let name = uname("solo");
        let probe = Arc::new(Probe::default());
        rig.spawn_child(
            &name,
            &probe,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        let lines = rig
            .script(vec![Op::Terminate(name.clone()), Op::Restart(name.clone())])
            .await;
        assert_eq!(lines, ["Ok(())", "Ok(())"]);
        wait_for_alive(&rig, &name).await;
        wait_until(BOUND, "the revived child to finish starting", || {
            probe.started.load(Ordering::SeqCst) == 2
        })
        .await;
        sleep(QUIET).await;
        assert!(
            is_alive(&rig, &name).await,
            "the revived child keeps running"
        );
        assert_eq!(probe.built.load(Ordering::SeqCst), 2);
        // The terminate is reported once, from its snapshot, and nothing else.
        removed_once(&rig, &name);
    });
}

/// `stop_child` races a crash: the child fails after a slow turn that was
/// under way when the call was made, and the same handler then calls
/// `restart_child`. `init_held` holds the restarted instance in its
/// initialization; `linger` keeps the handler busy after the second call.
async fn bounce_racing_a_crash_then_restart_child(init_held: bool, linger: bool) {
    // A budget of two: the crash is charged once, so one more crash is still
    // restarted and the supervisor survives it.
    let rig =
        supervisor(SupervisionConfig::one_for_one().max_restarts(2, Duration::from_secs(60))).await;
    let name = uname("racing");
    let probe = Arc::new(Probe::default());
    rig.spawn_child(
        &name,
        &probe,
        Shutdown::Timeout(Duration::from_secs(2)),
        RestartType::Permanent,
        0,
    )
    .await;
    wait_until(BOUND, "the child to finish starting", || {
        probe.started.load(Ordering::SeqCst) == 1
    })
    .await;
    if init_held {
        probe.held.store(true, Ordering::SeqCst);
    }
    let mut ops = vec![
        Op::SlowFail(name.clone()),
        Op::UntilInTurn(probe.clone()),
        Op::Stop(name.clone()),
        Op::Restart(name.clone()),
    ];
    if linger {
        ops.push(Op::Sleep(Duration::from_millis(40)));
    }
    let lines = rig.script(ops).await;
    assert_eq!(lines[1], "in turn: true", "{lines:?}");
    assert_eq!(lines[2], "Ok(())", "stop_child: {lines:?}");
    assert!(
        lines[3].contains("is restarting"),
        "the crash still owes its restart decision, so restart_child is refused: {lines:?}"
    );
    probe.held.store(false, Ordering::SeqCst);
    wait_until(BOUND, "the child to run again", || {
        probe.started.load(Ordering::SeqCst) == 2
    })
    .await;
    wait_for_alive(&rig, &name).await;
    sleep(QUIET).await;
    // One original instance and one restart; the crash is reported once.
    assert_eq!(probe.built.load(Ordering::SeqCst), 2);
    let events = rig.events_for(&name);
    assert_eq!(events.len(), 1, "the crash is reported once: {events:?}");
    assert_eq!(events[0].action, SupervisionAction::RestartInitiated);
    assert!(matches!(events[0].reason, StopReason::Failure(_)));
    // One charge: a second crash is restarted, and the supervisor lives.
    crash(&child_handle(&name).await).await;
    wait_until(BOUND, "the second crash to be restarted", || {
        probe.built.load(Ordering::SeqCst) == 3
    })
    .await;
    assert!(rig.sup.is_alive(), "the supervisor survived");
}

#[test]
fn bounce_racing_a_crash_then_restart_child_reports_the_crash_once() {
    common::run(0, bounce_racing_a_crash_then_restart_child(false, false));
}

#[test]
fn bounce_racing_a_crash_then_restart_child_with_a_slow_init_reports_the_crash_once() {
    common::run(0, bounce_racing_a_crash_then_restart_child(true, false));
}

#[test]
fn bounce_racing_a_crash_then_restart_child_with_a_linger_reports_the_crash_once() {
    common::run(0, bounce_racing_a_crash_then_restart_child(false, true));
}

#[test]
fn stop_child_of_a_permanent_child_whose_task_already_ended_restarts_it_once() {
    common::run(0, async {
        // A budget of one: the bounce is free, so a later crash is restarted.
        let rig =
            supervisor(SupervisionConfig::one_for_one().max_restarts(1, Duration::from_secs(60)))
                .await;
        let name = uname("died-first");
        let probe = Arc::new(Probe::default());
        rig.spawn_child(
            &name,
            &probe,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        // spawn_child returns before the first instance ran its start hooks,
        // and a Kill that lands first skips them: wait for the start, or the
        // count below never reaches two.
        wait_until(BOUND, "the first instance to finish its start", || {
            probe.started.load(Ordering::SeqCst) == 1
        })
        .await;
        // The handler ends the child through its handle and waits until the
        // supervisor can see that the task ended, with the ledger still
        // reading the dead incarnation as running; then it calls stop_child.
        let lines = rig
            .script(vec![
                Op::KillThroughHandle(name.clone()),
                Op::UntilNotAlive(name.clone()),
                Op::Stop(name.clone()),
            ])
            .await;
        assert_eq!(lines[1], "not alive", "{lines:?}");
        assert_eq!(lines[2], "Ok(())", "{lines:?}");
        // Ordered by observation, not by a quiet window: the restarted
        // instance ran, the supervisor adopted it, and the end of the first
        // instance was reported.
        wait_until(BOUND, "the child to be restarted", || {
            probe.started.load(Ordering::SeqCst) >= 2
        })
        .await;
        wait_for_alive(&rig, &name).await;
        wait_until(
            BOUND,
            "the end of the first instance to be reported",
            || !rig.events_for(&name).is_empty(),
        )
        .await;
        // The run loop reads every supervision report that is ready before it
        // reads its mailbox, so this answer is ordered after anything the
        // restart or the report could still cause.
        let lines = rig.script(vec![Op::Read(name.clone())]).await;
        assert_eq!(lines[0], "alive=true pending=false", "{lines:?}");
        assert_eq!(probe.started.load(Ordering::SeqCst), 2, "restarted once");
        assert_eq!(probe.built.load(Ordering::SeqCst), 2, "restarted once");
        let events = rig.events_for(&name);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].action, SupervisionAction::RestartInitiated);
        assert!(matches!(events[0].reason, StopReason::Kill), "{events:?}");
        // A window that looks for a late duplicate: nothing more restarts the
        // child or reports its end.
        sleep(QUIET).await;
        assert_eq!(probe.started.load(Ordering::SeqCst), 2, "restarted once");
        assert_eq!(probe.built.load(Ordering::SeqCst), 2, "restarted once");
        assert_eq!(rig.events_for(&name).len(), 1, "reported once");
        // The restart was free: a real crash is restarted with a budget of one.
        crash(&child_handle(&name).await).await;
        wait_until(BOUND, "the crash to be restarted", || {
            probe.built.load(Ordering::SeqCst) >= 3
        })
        .await;
        sleep(QUIET).await;
        assert_eq!(
            probe.built.load(Ordering::SeqCst),
            3,
            "the crash is restarted once"
        );
        assert!(rig.sup.is_alive());
    });
}

#[test]
fn children_reports_a_stopping_child_as_alive() {
    common::run(0, async {
        let rig = supervisor(plain()).await;
        let name = uname("stopping");
        let probe = Arc::new(Probe::default());
        rig.spawn_child(
            &name,
            &probe,
            Shutdown::Timeout(Duration::from_millis(300)),
            RestartType::Permanent,
            always_vetoes(),
        )
        .await;
        // An abandoned stop: the child vetoes inside its ladder.
        let lines = rig
            .script(vec![
                Op::TerminateAbandon(name.clone()),
                Op::Read(name.clone()),
            ])
            .await;
        assert_eq!(lines[1], "alive=true pending=false", "{lines:?}");
        assert!(!probe.ended.load(Ordering::SeqCst));
        wait_until(BOUND, "the child to end", || {
            probe.ended.load(Ordering::SeqCst)
        })
        .await;
    });
}

#[test]
fn recorded_death_awaiting_its_decision_reads_restart_pending() {
    common::run(0, async {
        // RestForOne [x, y, z]: y crashes and z, which vetoes under a long
        // window, holds the group's stop phase open. x is not part of the
        // group.
        let rig =
            supervisor(SupervisionConfig::rest_for_one().max_restarts(5, Duration::from_secs(60)))
                .await;
        let (x, y, z) = (uname("x"), uname("y"), uname("z"));
        let (px, py, pz) = probes();
        rig.spawn_child(
            &x,
            &px,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        rig.spawn_child(
            &y,
            &py,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        rig.spawn_child(
            &z,
            &pz,
            Shutdown::Timeout(Duration::from_millis(1500)),
            RestartType::Permanent,
            always_vetoes(),
        )
        .await;
        crash(&child_handle(&y).await).await;
        wait_until(BOUND, "the group to ask z to stop", || {
            pz.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        // x is killed while the group is pending: its restart decision waits.
        let _ = child_handle(&x)
            .await
            .stop_within(StopReason::Kill, Duration::from_millis(200))
            .await;
        let deadline = Instant::now() + BOUND;
        loop {
            let lines = rig.script(vec![Op::Read(x.clone())]).await;
            if lines[0] == "alive=false pending=true" {
                break;
            }
            assert!(Instant::now() < deadline, "x never read pending: {lines:?}");
            sleep(Duration::from_millis(5)).await;
        }
        let lines = rig
            .script(vec![
                Op::Read(x.clone()),
                Op::Restart(x.clone()),
                Op::Delete(x.clone()),
                Op::Stop(x.clone()),
            ])
            .await;
        assert_eq!(lines[0], "alive=false pending=true", "{lines:?}");
        assert!(lines[1].contains("is restarting"), "{lines:?}");
        assert!(lines[2].contains("is restarting"), "{lines:?}");
        assert!(lines[3].contains("is restarting"), "{lines:?}");
        assert_eq!(pz.built.load(Ordering::SeqCst), 1, "the group still pends");
        // The decision about x's death waits for the group: no call decides it
        // meanwhile (a stop of a child that ended on its own, a kill here, would
        // otherwise be a bounce), and x is not started again before the group
        // completes.
        assert_eq!(px.built.load(Ordering::SeqCst), 1, "{lines:?}");
        // The group completes and x is restarted afterwards; x is an earlier
        // sibling, so its own crash then restarts y and z once more.
        wait_until(BOUND, "x to be restarted after the group", || {
            px.built.load(Ordering::SeqCst) == 2
                && py.built.load(Ordering::SeqCst) == 3
                && pz.built.load(Ordering::SeqCst) == 3
        })
        .await;
        wait_for_alive(&rig, &x).await;
    });
}

#[test]
fn failed_manual_restart_child_is_reported_removed_and_charges_no_budget() {
    common::run(0, async {
        // OneForAll [a, b] with a budget of one restart.
        let rig =
            supervisor(SupervisionConfig::one_for_all().max_restarts(1, Duration::from_secs(60)))
                .await;
        let (a, b) = (uname("a"), uname("b"));
        let (pa, pb) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
        rig.spawn_child(
            &a,
            &pa,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        rig.spawn_child(
            &b,
            &pb,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        rig.ok(Cmd::Terminate { name: b.clone() }).await;
        pb.fail_init.store(true, Ordering::SeqCst);
        rig.ok(Cmd::RestartChild { name: b.clone() }).await;
        // The failure is reported once, with the initialization error.
        wait_until(BOUND, "the failed restart to be reported", || {
            rig.events_for(&b)
                .iter()
                .any(|event| matches!(event.reason, StopReason::Failure(_)))
        })
        .await;
        sleep(QUIET).await;
        let events = rig.events_for(&b);
        let failures: Vec<_> = events
            .iter()
            .filter(|event| matches!(event.reason, StopReason::Failure(_)))
            .collect();
        assert_eq!(failures.len(), 1, "{events:?}");
        assert_eq!(failures[0].action, SupervisionAction::Removed);
        assert!(
            failures[0].reason.to_string().contains("refused to start"),
            "{:?}",
            failures[0].reason
        );
        // The failure charged nothing and touched no sibling.
        assert_eq!(
            pa.pre_stops.load(Ordering::SeqCst),
            0,
            "a was never stopped"
        );
        assert_eq!(pa.built.load(Ordering::SeqCst), 1);
        assert!(rig.sup.is_alive());
        // The spec is kept: the child can be started again, and a later real
        // crash of a is still restarted by the group.
        pb.fail_init.store(false, Ordering::SeqCst);
        rig.ok(Cmd::RestartChild { name: b.clone() }).await;
        wait_for_alive(&rig, &b).await;
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "the crash of a to be restarted", || {
            pa.built.load(Ordering::SeqCst) == 2
        })
        .await;
        wait_for_alive(&rig, &a).await;
        assert!(rig.sup.is_alive());
    });
}

#[test]
fn group_restart_stops_an_in_flight_restart_and_restarts_it_in_its_slot() {
    common::run(0, async {
        let rig = supervisor(one_for_all()).await;
        let (a, b, c) = (uname("a"), uname("b"), uname("c"));
        let (pa, pb, pc) = probes();
        for (name, probe) in [(&a, &pa), (&b, &pb), (&c, &pc)] {
            rig.spawn_child(
                name,
                probe,
                Shutdown::Timeout(Duration::from_secs(2)),
                RestartType::Permanent,
                0,
            )
            .await;
        }
        wait_until(BOUND, "the children to finish starting", || {
            [&pa, &pb, &pc]
                .iter()
                .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        // c is bounced, and its new instance stays in its initialization: c's
        // restart attempt is in flight. The stop of c's first instance asked it
        // once.
        pc.held.store(true, Ordering::SeqCst);
        rig.ok(Cmd::Stop { name: c.clone() }).await;
        wait_until(BOUND, "c's new instance to be built", || {
            pc.built.load(Ordering::SeqCst) == 2
        })
        .await;
        assert_eq!(pc.pre_stops.load(Ordering::SeqCst), 1);
        // a crashes while the chain's start of a will stay in its
        // initialization: the group evaluates with c's attempt in flight,
        // commits it and orders it to stop.
        pa.held.store(true, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "the group to be evaluated", || {
            rig.events_for(&a)
                .iter()
                .any(|event| matches!(event.reason, StopReason::Failure(_)))
        })
        .await;
        // The attempt finishes its initialization under the order and is
        // stopped: it is asked once, never adopted and never kept.
        pc.held.store(false, Ordering::SeqCst);
        wait_until(BOUND, "the chain to start a", || {
            pa.built.load(Ordering::SeqCst) == 2
        })
        .await;
        assert_eq!(
            pc.started.load(Ordering::SeqCst),
            2,
            "the attempt finished its initialization"
        );
        assert_eq!(
            pc.pre_stops.load(Ordering::SeqCst),
            2,
            "the attempt's instance was asked to stop, the group's order"
        );
        wait_until(BOUND, "the attempt's instance to be dropped", || {
            pc.drop_order(1) != usize::MAX
        })
        .await;
        // The chain goes on in start order: c is built again only after a and
        // b were restarted.
        sleep(QUIET).await;
        assert_eq!(
            pc.built.load(Ordering::SeqCst),
            2,
            "c waits for a and b in its slot"
        );
        pa.held.store(false, Ordering::SeqCst);
        wait_until(BOUND, "c to be started in its slot", || {
            pc.built.load(Ordering::SeqCst) == 3 && pc.started.load(Ordering::SeqCst) == 3
        })
        .await;
        wait_for_alive(&rig, &c).await;
        sleep(QUIET).await;
        assert_eq!(pa.built.load(Ordering::SeqCst), 2);
        assert_eq!(pb.built.load(Ordering::SeqCst), 2);
        assert_eq!(pc.built.load(Ordering::SeqCst), 3);
        // c's third instance finished starting after the new a and the new b
        // did.
        assert!(
            pc.last_start.load(Ordering::SeqCst) > pa.last_start.load(Ordering::SeqCst)
                && pc.last_start.load(Ordering::SeqCst) > pb.last_start.load(Ordering::SeqCst),
            "a, b, c start in that order: a {} b {} c {}",
            pa.last_start.load(Ordering::SeqCst),
            pb.last_start.load(Ordering::SeqCst),
            pc.last_start.load(Ordering::SeqCst)
        );
        // The instance the group stopped is reported exactly once as part of
        // the restart; the bounce reported the first one.
        let events = rig.events_for(&c);
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(
            events
                .iter()
                .all(|event| event.action == SupervisionAction::RestartInitiated),
            "{events:?}"
        );
        assert!(rig.sup.is_alive());
    });
}

#[test]
fn children_reads_a_child_whose_task_ended_as_down_before_its_death_is_handled() {
    common::run(0, async {
        let rig = supervisor(plain()).await;
        let name = uname("ended");
        let probe = Arc::new(Probe::default());
        rig.spawn_child(
            &name,
            &probe,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        // The handler ends the child through its handle and waits until the
        // supervisor can see that the task ended; the run loop is busy in
        // this handler and has not handled the death.
        let lines = rig
            .script(vec![
                Op::KillThroughHandle(name.clone()),
                Op::UntilNotAlive(name.clone()),
                Op::Read(name.clone()),
                Op::Restart(name.clone()),
                Op::Delete(name.clone()),
            ])
            .await;
        assert_eq!(lines[1], "not alive", "{lines:?}");
        assert_eq!(lines[2], "alive=false pending=true", "{lines:?}");
        assert!(lines[3].contains("is restarting"), "{lines:?}");
        assert!(lines[4].contains("is restarting"), "{lines:?}");
        // The death is then handled: the Permanent child is restarted.
        wait_until(BOUND, "the child to be restarted", || {
            probe.built.load(Ordering::SeqCst) == 2
        })
        .await;
    });
}

#[test]
fn restart_child_of_a_child_being_stopped_answers_child_restarting_until_its_death_is_handled() {
    common::run(0, async {
        let rig = supervisor(plain()).await;
        let name = uname("being-stopped");
        let probe = Arc::new(Probe::default());
        rig.spawn_child(
            &name,
            &probe,
            Shutdown::Timeout(Duration::from_millis(100)),
            RestartType::Permanent,
            always_vetoes(),
        )
        .await;
        // The abandoned stop runs its ladder while the handler sleeps; the
        // child's task ends, and the run loop, busy in this handler, has not
        // handled its death.
        let lines = rig
            .script(vec![
                Op::TerminateAbandon(name.clone()),
                Op::Sleep(Duration::from_millis(600)),
                Op::Restart(name.clone()),
                Op::Read(name.clone()),
            ])
            .await;
        assert!(lines[2].contains("is restarting"), "{lines:?}");
        assert_ne!(lines[3], "no such child", "the spec is kept: {lines:?}");
        // Once the death was handled, the call succeeds and the stop was
        // reported once.
        wait_until(BOUND, "the stop to be reported", || {
            !rig.events_for(&name).is_empty()
        })
        .await;
        let lines = rig.script(vec![Op::Restart(name.clone())]).await;
        assert_eq!(lines[0], "Ok(())", "{lines:?}");
        wait_for_alive(&rig, &name).await;
        removed_once(&rig, &name);
    });
}

#[test]
fn restart_and_delete_of_a_child_being_stopped_answer_child_restarting() {
    common::run(0, async {
        let rig = supervisor(plain()).await;
        let name = uname("stopping-calls");
        let probe = Arc::new(Probe::default());
        rig.spawn_child(
            &name,
            &probe,
            Shutdown::Timeout(Duration::from_millis(300)),
            RestartType::Permanent,
            always_vetoes(),
        )
        .await;
        // The child's task still runs: its cell holds no fate.
        let lines = rig
            .script(vec![
                Op::TerminateAbandon(name.clone()),
                Op::Restart(name.clone()),
                Op::Delete(name.clone()),
                Op::Read(name.clone()),
            ])
            .await;
        assert!(
            !probe.ended.load(Ordering::SeqCst),
            "the child's task still runs, its cell holds no fate"
        );
        assert!(lines[1].contains("is restarting"), "{lines:?}");
        assert!(lines[2].contains("is restarting"), "{lines:?}");
        assert_ne!(lines[3], "no such child", "the spec is kept: {lines:?}");
    });
}

/// OneForAll `[crasher, member, slow]`: `member` and `slow` veto under
/// `Timeout(300 ms)` and `Timeout(1500 ms)`, the crasher crashes, and the
/// group asks `member` to stop. Returns the rig, the names and the probes.
async fn group_with_a_member_being_stopped() -> (Rig, [String; 3], [Arc<Probe>; 3]) {
    let rig = supervisor(one_for_all()).await;
    let names = [uname("crasher"), uname("member"), uname("slow")];
    let (pc, pm, ps) = probes();
    rig.spawn_child(
        &names[0],
        &pc,
        Shutdown::Timeout(Duration::from_secs(2)),
        RestartType::Permanent,
        0,
    )
    .await;
    rig.spawn_child(
        &names[1],
        &pm,
        Shutdown::Timeout(Duration::from_millis(300)),
        RestartType::Permanent,
        always_vetoes(),
    )
    .await;
    rig.spawn_child(
        &names[2],
        &ps,
        Shutdown::Timeout(Duration::from_millis(1500)),
        RestartType::Permanent,
        always_vetoes(),
    )
    .await;
    crash(&child_handle(&names[0]).await).await;
    wait_until(BOUND, "the group to ask the member to stop", || {
        pm.pre_stops.load(Ordering::SeqCst) == 1
    })
    .await;
    (rig, names, [pc, pm, ps])
}

#[test]
fn a_child_respawned_under_a_released_name_is_bounced_and_kept_by_stop_child() {
    common::run(0, async {
        let (rig, [_crasher, member, _slow], [pc, pm, ps]) =
            group_with_a_member_being_stopped().await;
        // One handler: the member is released from the group and ends, it is
        // deleted, and a new child takes its name.
        let respawned = Arc::new(Probe::default());
        let lines = rig
            .script(vec![
                Op::Terminate(member.clone()),
                Op::Delete(member.clone()),
                Op::Spawn {
                    name: member.clone(),
                    probe: respawned.clone(),
                    shutdown: Shutdown::Timeout(Duration::from_millis(200)),
                    restart: RestartType::Permanent,
                    vetoes: 0,
                },
            ])
            .await;
        assert_eq!(lines, ["Ok(())", "Ok(())", "Ok(())"]);
        // A later handler bounces the new child: it is a child like any
        // other, not a member of the group, and is restarted.
        let lines = rig.script(vec![Op::Stop(member.clone())]).await;
        assert_eq!(lines[0], "Ok(())", "{lines:?}");
        wait_until(BOUND, "the new child to be restarted by its bounce", || {
            respawned.started.load(Ordering::SeqCst) == 2
        })
        .await;
        // The group then completes, and it does not stop or keep down the
        // restarted child.
        wait_until(BOUND, "the group to complete", || {
            pc.built.load(Ordering::SeqCst) == 2 && ps.built.load(Ordering::SeqCst) == 2
        })
        .await;
        sleep(QUIET).await;
        assert_eq!(respawned.built.load(Ordering::SeqCst), 2);
        assert!(
            is_alive(&rig, &member).await,
            "the bounced child keeps running"
        );
        assert_eq!(pm.built.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn a_child_respawned_under_the_name_of_a_released_member_is_stopped_by_terminate_child() {
    common::run(0, async {
        // OneForAll [crasher, member]. The crasher's restart is held in its
        // initialization, so the group pends in its restart phase.
        let rig = supervisor(one_for_all()).await;
        let (crasher, member) = (uname("crasher"), uname("member"));
        let (pc, pm) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
        rig.spawn_child(
            &crasher,
            &pc,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        rig.spawn_child(
            &member,
            &pm,
            Shutdown::Timeout(Duration::from_millis(300)),
            RestartType::Permanent,
            always_vetoes(),
        )
        .await;
        wait_until(BOUND, "the crasher to finish starting", || {
            pc.started.load(Ordering::SeqCst) == 1
        })
        .await;
        pc.held.store(true, Ordering::SeqCst);
        crash(&child_handle(&crasher).await).await;
        wait_until(BOUND, "the group to ask the member to stop", || {
            pm.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        rig.ok(Cmd::Terminate {
            name: member.clone(),
        })
        .await;
        // Wait until the group has handled the member's end.
        let deadline = Instant::now() + BOUND;
        loop {
            let lines = rig.script(vec![Op::Read(member.clone())]).await;
            if lines[0] == "alive=false pending=false" {
                break;
            }
            assert!(Instant::now() < deadline, "{lines:?}");
            sleep(Duration::from_millis(5)).await;
        }
        // In a later handler the member is deleted and a new child takes its
        // name.
        let respawned = Arc::new(Probe::default());
        let lines = rig
            .script(vec![
                Op::Delete(member.clone()),
                Op::Spawn {
                    name: member.clone(),
                    probe: respawned.clone(),
                    shutdown: Shutdown::Timeout(Duration::from_millis(200)),
                    restart: RestartType::Permanent,
                    vetoes: 0,
                },
            ])
            .await;
        assert_eq!(lines, ["Ok(())", "Ok(())"]);
        // The new child is a running child like any other: a restart is
        // refused for it as running, and a terminate stops it.
        wait_until(BOUND, "the new child to finish starting", || {
            respawned.started.load(Ordering::SeqCst) == 1
        })
        .await;
        let lines = rig
            .script(vec![
                Op::Restart(member.clone()),
                Op::Terminate(member.clone()),
            ])
            .await;
        assert!(lines[0].contains("is running"), "{lines:?}");
        assert_eq!(lines[1], "Ok(())", "{lines:?}");
        assert!(respawned.ended.load(Ordering::SeqCst), "it was stopped");
        // The terminate of the new child, made while the group pends, keeps
        // it down through the group restart: a bounce of it is refused until
        // the group completes, as a released member's is.
        let lines = rig.script(vec![Op::Stop(member.clone())]).await;
        assert!(restarting(&lines[0]), "{lines:?}");
        // The group completes once the hold is lifted, and the bounce is then
        // answered like any down child's.
        pc.held.store(false, Ordering::SeqCst);
        wait_for_alive(&rig, &crasher).await;
        until_idle(&rig, &[&crasher, &member]).await;
        let lines = rig.script(vec![Op::Stop(member.clone())]).await;
        assert_eq!(lines[0], "Ok(())", "{lines:?}");
    });
}

// ---------------------------------------------------------------------------
// Ownership closure: a supervisor's stop and a group restart take every
// occupant of the registry
// ---------------------------------------------------------------------------

/// A graceful window of 200 ms.
const WINDOW_200: Shutdown = Shutdown::Timeout(Duration::from_millis(200));

/// A supervisor on its own system, under a name the test keeps.
async fn supervisor_named(
    sys: &Arc<ActorSystem>,
    name: &str,
    config: SupervisionConfig,
    stop_on_restart: bool,
    yield_on_stopped: bool,
) -> Rig {
    spawn_sup(
        Some(sys),
        name,
        config,
        SupOpts {
            stop_on_restart,
            yield_on_stopped,
            deliver_delay_ms: 0,
        },
    )
    .await
}

async fn supervisor_on(sys: &Arc<ActorSystem>, config: SupervisionConfig) -> Rig {
    supervisor_named(sys, &uname("sup"), config, false, false).await
}

async fn child_in(sys: &Arc<ActorSystem>, name: &str) -> ActorHandle<Probed> {
    let mut found = None;
    wait_until(BOUND, "the child to be registered", || {
        found = sys.get::<Probed>(name);
        found.as_ref().is_some_and(|handle| handle.is_alive())
    })
    .await;
    found.expect("registered")
}

/// Stops the supervisor and waits, bounded, for its `Stopped`; returns how
/// long that took.
async fn stop_supervisor(rig: &Rig, bound: Duration) -> Duration {
    let started = Instant::now();
    within(
        BOUND,
        "the stop request",
        rig.sup.stop(StopReason::Graceful),
    )
    .await
    .expect("the stop request is accepted");
    within(
        bound,
        "the supervisor to publish Stopped",
        rig.sup.wait_stopped(),
    )
    .await;
    started.elapsed()
}

#[test]
fn parent_stop_mid_restart_runs_the_fresh_incarnations_on_stopped_before_stopped() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("mid-restart")).unwrap();
        let sup_name = uname("sup");
        let rig = supervisor_named(&sys, &sup_name, plain(), false, false).await;
        let probe = Arc::new(Probe::default());
        let name = uname("w");
        rig.spawn_child(
            &name,
            &probe,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        let child = child_in(&sys, &name).await;
        // The second incarnation initializes for 400 ms.
        probe.init_sleep_ms.store(400, Ordering::SeqCst);
        crash(&child).await;
        wait_until(BOUND, "the second incarnation to be built", || {
            probe.built.load(Ordering::SeqCst) == 2
        })
        .await;
        sleep(Duration::from_millis(100)).await;
        stop_supervisor(&rig, Duration::from_secs(5)).await;
        // The crashed incarnation ran `on_stopped`, and so did the fresh one,
        // after its initialization completed: the supervisor is `Stopped`
        // only once both ran.
        assert_eq!(probe.started.load(Ordering::SeqCst), 2);
        assert_eq!(probe.stopped_runs.load(Ordering::SeqCst), 2);
        assert!(
            probe.ended.load(Ordering::SeqCst),
            "the fresh incarnation ended"
        );
        assert!(sys.get::<Probed>(&name).is_none(), "the name is free");
        // An immediate same-name respawn of the supervisor succeeds.
        let again = supervisor_named(&sys, &sup_name, plain(), false, false).await;
        assert!(again.sup.is_alive());
        let report = within(BOUND, "the system shutdown", sys.shutdown()).await;
        assert!(
            report.swept.is_empty(),
            "nothing is left to sweep: {:?}",
            report.swept
        );
    });
}

/// How the abandoned manual stop of the child is made.
#[derive(Clone, Copy)]
enum Abandoned {
    Terminate,
    Stop,
}

/// An always-vetoing `Timeout(2 s)` child in an abandoned manual stop, then
/// the parent's stop: the parent publishes `Stopped` only after the child's
/// task ended, which honors the child's own window.
async fn parent_stop_during_an_abandoned_manual_stop(form: Abandoned, paused: bool) {
    let sys = ActorSystem::create(uname("abandoned-parent")).unwrap();
    let rig = supervisor_on(&sys, plain()).await;
    let probe = Arc::new(Probe::default());
    let name = uname("veto");
    rig.spawn_child(
        &name,
        &probe,
        Shutdown::Timeout(Duration::from_secs(2)),
        RestartType::Permanent,
        always_vetoes(),
    )
    .await;
    let _child = child_in(&sys, &name).await;
    let abandoned = Instant::now();
    rig.ok(match form {
        Abandoned::Terminate => Cmd::TerminateAbandon {
            name: name.clone(),
            wait: Duration::ZERO,
        },
        Abandoned::Stop => Cmd::StopAbandon {
            name: name.clone(),
            wait: Duration::ZERO,
        },
    })
    .await;
    let window = Duration::from_secs(2);
    within(
        upper(
            paused,
            window + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE + Duration::from_millis(500),
        ),
        "the parent to publish Stopped",
        async {
            rig.sup.stop(StopReason::Graceful).await.expect("stop");
            rig.sup.wait_stopped().await;
        },
    )
    .await;
    let took = abandoned.elapsed();
    assert!(
        probe.ended.load(Ordering::SeqCst),
        "the child's task ended before the parent published Stopped"
    );
    assert!(
        took >= window - ASSUMED_KILL_GRACE,
        "the child's own window was honored: {took:?}"
    );
    assert!(
        took < upper(
            paused,
            window + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE + Duration::from_millis(500)
        ),
        "the stop ended within the window, the Kill step, the abort and a margin: {took:?}"
    );
    if matches!(form, Abandoned::Stop) {
        assert_eq!(
            probe.built.load(Ordering::SeqCst),
            1,
            "no restart follows the bounce of a stopping supervisor"
        );
    }
    assert!(sys.get::<Probed>(&name).is_none(), "the name is free");
}

#[test]
fn parent_stop_reaches_a_child_whose_manual_stop_is_in_progress() {
    common::run(
        0,
        parent_stop_during_an_abandoned_manual_stop(Abandoned::Terminate, false),
    );
}

#[test]
fn parent_stop_reaches_a_child_whose_manual_stop_is_in_progress_paused() {
    common::run_paused(parent_stop_during_an_abandoned_manual_stop(
        Abandoned::Terminate,
        true,
    ));
}

#[test]
fn parent_stop_reaches_a_child_whose_stop_child_is_in_progress() {
    common::run(
        0,
        parent_stop_during_an_abandoned_manual_stop(Abandoned::Stop, false),
    );
}

#[test]
fn parent_stop_reaches_a_child_whose_stop_child_is_in_progress_paused() {
    common::run_paused(parent_stop_during_an_abandoned_manual_stop(
        Abandoned::Stop,
        true,
    ));
}

#[test]
fn parent_stop_asks_an_infinity_child_in_an_abandoned_terminate_once_more() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("abandoned-infinity")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let probe = Arc::new(Probe::default());
        let name = uname("veto-once");
        // The child vetoes the first request and accepts the second.
        rig.spawn_child(&name, &probe, Shutdown::Infinity, RestartType::Permanent, 1)
            .await;
        let _child = child_in(&sys, &name).await;
        rig.ok(Cmd::TerminateAbandon {
            name: name.clone(),
            wait: Duration::ZERO,
        })
        .await;
        wait_until(BOUND, "the first request to be vetoed", || {
            probe.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        // The teardown's single re-ask lets the child accept.
        stop_supervisor(&rig, Duration::from_secs(5)).await;
        assert_eq!(probe.pre_stops.load(Ordering::SeqCst), 2);
        assert!(probe.ended.load(Ordering::SeqCst));
    });
}

#[test]
fn parent_stop_cancels_a_restart_attempt_before_its_factory_runs() {
    // The attempt is held in its successor wait: the crashed incarnation was
    // a supervisor whose worker is stuck in a blocked turn under a short
    // `Timeout`, so the incarnation ends on the verdict while the worker
    // still runs, and the attempt waits on that worker for at most its bound
    // (the time the worker may outlive its killed parent, its kill grace,
    // plus the settle time, counted from the start of the wait). The
    // supervisor stops itself as it reports the restart, and its order
    // cancels the waiting attempt at once, so it publishes `Stopped` well
    // inside that bound and the factory is never entered. An attempt that
    // ignored the order would end only when its bound expired, unresponsive
    // and also without calling its factory: the time from the restart report
    // to `Stopped` is what tells the two apart, so the worker's kill grace is
    // long enough to leave a margin of seconds on both sides of half the
    // bound, and the worker's turn holds its thread until the test opens the
    // gate, after the checks. The stuck turn blocks a thread, so the test
    // needs real parallelism; on one thread the interleaving is reached by
    // the explorer's `small_tree_kill_paths` runs (witness "an in-flight
    // attempt ordered during its successor wait").
    const WINDOW: Duration = Duration::from_millis(200);
    const GRACE: Duration = Duration::from_secs(5);
    let bound = GRACE + ASSUMED_ABORT_SETTLE;
    let verdict = WINDOW + GRACE + ASSUMED_ABORT_SETTLE;
    common::run_multi_thread_only(4, async move {
        let run = uname("cancel-attempt");
        let mut worker = Member::worker("g", Shutdown::Timeout(WINDOW));
        worker.kill_grace = Some(GRACE);
        let mut top = Member::supervisor(
            "top",
            Shutdown::Timeout(FIVE),
            vec![Member::supervisor(
                "c",
                Shutdown::Timeout(FIVE),
                vec![worker],
            )],
        )
        .unique(&run);
        top.stops_itself_on_restart = true;
        let label = |name: &str| format!("{name}-{run}");
        let (tape, sys) = (Arc::new(Tape::default()), unique_sys("cancel-attempt"));
        let root = root_of(&sys, &top, &tape).await;
        let (c, g) = (
            node_named(&sys, &label("c")).await,
            node_named(&sys, &label("g")).await,
        );
        // The worker's turn outlasts the verdict on it and then the attempt's
        // whole bound: it ends only when the test opens the gate.
        let gate = OpensTheGate(tape.clone());
        g.notify(NodeMsg::BlockOnGate).await.unwrap();
        wait_until(BOUND, "the worker's turn to begin", || {
            tape.has(&format!("blocked:{}", label("g")))
        })
        .await;
        tape.factory_refuses.store(true, Ordering::SeqCst);
        c.notify(NodeMsg::Crash).await.unwrap();
        within(
            verdict + BOUND,
            "the supervisor to stop itself",
            root.wait_stopped(),
        )
        .await;
        let stopped = Instant::now();
        let reported = tape.when(&format!(
            "child_stopped:{}:{}:failure:RestartInitiated",
            label("top"),
            label("c")
        ));
        let took = stopped.duration_since(reported);
        assert!(
            took < bound / 2,
            "the order cancelled the waiting attempt at once: `Stopped` came {took:?} \
             after the restart report, against a bound of {bound:?}"
        );
        assert!(
            !tape.has(&format!("unblocked:{}", label("g"))),
            "the worker's turn still runs: {:?}",
            tape.all()
        );
        assert!(
            !tape
                .all()
                .iter()
                .any(|what| what.starts_with("failure_text:")
                    && what.contains("did not end in time")),
            "no wait expired: {:?}",
            tape.all()
        );
        sleep(QUIET).await;
        assert!(
            !tape.factory_entered.load(Ordering::SeqCst),
            "the cancelled attempt never entered its factory: {:?}",
            tape.all()
        );
        assert_eq!(
            tape.count(&format!("started:{}", label("c"))),
            1,
            "{:?}",
            tape.all()
        );
        assert_eq!(
            tape.all()
                .iter()
                .filter(|what| what.starts_with(&format!(
                    "child_stopped:{}:{}",
                    label("top"),
                    label("c")
                )))
                .count(),
            1,
            "only the crash is reported: {:?}",
            tape.all()
        );
        drop(gate);
    });
}

#[test]
fn parent_stop_completes_when_an_attempt_s_factory_panics_during_the_stop() {
    // The attempt is held in its successor wait: the crashed incarnation was
    // a supervisor whose worker is stuck in a turn that blocks its thread on
    // a gate. The supervisor stops itself, and its `on_stopped` opens the
    // gate and waits until the attempt entered its factory, so the attempt
    // runs, finds no order and panics in its factory after the final
    // collection and before the teardown waits on its cell: the cell reads no
    // incarnation, and the stop completes. No delivery for the attempt can
    // happen in this order, since its factory runs only after `on_stopped`
    // began and every delivery of a stop runs before `on_stopped`, so the
    // test checks the stop's completion and that the factory built nothing.
    // The blocked turn needs real parallelism, and the explorer models no
    // factory failure, so this interleaving is pinned on multi_thread only.
    common::run_multi_thread_only(4, async {
        let run = uname("factory-panics");
        let mut top = Member::supervisor(
            "top",
            Shutdown::Timeout(FIVE),
            vec![Member::supervisor(
                "c",
                Shutdown::Timeout(TWO),
                vec![Member::worker(
                    "g",
                    Shutdown::Timeout(Duration::from_millis(200)),
                )],
            )],
        )
        .unique(&run);
        top.stops_itself_on_restart = true;
        top.opens_the_gate_on_stopped = true;
        let label = |name: &str| format!("{name}-{run}");
        let (tape, sys) = (Arc::new(Tape::default()), unique_sys("factory-panics"));
        let root = root_of(&sys, &top, &tape).await;
        let (c, g) = (
            node_named(&sys, &label("c")).await,
            node_named(&sys, &label("g")).await,
        );
        g.notify(NodeMsg::BlockOnGate).await.unwrap();
        wait_until(BOUND, "the worker's turn to begin", || {
            tape.has(&format!("blocked:{}", label("g")))
        })
        .await;
        tape.factory_refuses.store(true, Ordering::SeqCst);
        c.notify(NodeMsg::Crash).await.unwrap();
        within(BOUND, "the supervisor to stop itself", root.wait_stopped()).await;
        assert!(
            tape.factory_entered.load(Ordering::SeqCst),
            "the attempt entered its factory during the stop: {:?}",
            tape.all()
        );
        sleep(QUIET).await;
        assert_eq!(
            tape.count(&format!("started:{}", label("c"))),
            1,
            "the factory built nothing: {:?}",
            tape.all()
        );
    });
}

async fn starting_child_gets_its_window_from_the_stop(
    window: Duration,
    expect_completion: bool,
    paused: bool,
) {
    let sys = ActorSystem::create(uname("window")).unwrap();
    let rig = supervisor_on(&sys, plain()).await;
    let probe = Arc::new(Probe::default());
    let name = uname("w");
    rig.spawn_child(
        &name,
        &probe,
        Shutdown::Timeout(window),
        RestartType::Permanent,
        0,
    )
    .await;
    let child = child_in(&sys, &name).await;
    // The first incarnation has read its knobs: what is set below reaches
    // only the restarted one.
    wait_until(BOUND, "the first incarnation to finish starting", || {
        probe.started.load(Ordering::SeqCst) == 1
    })
    .await;
    if expect_completion {
        // The restarted incarnation initializes for 300 ms and records its
        // own completion.
        probe.init_sleep_ms.store(300, Ordering::SeqCst);
    } else {
        // The restarted incarnation's initialization never completes: it
        // waits for a child that is never spawned, so only the Kill at the
        // end of the window and the abort after the grace end it.
        *probe.wait_alive.lock().unwrap() = Some(uname("never-spawned"));
    }
    crash(&child).await;
    wait_until(
        BOUND,
        "the second incarnation to be inside its init",
        || probe.entered.load(Ordering::SeqCst) == 2,
    )
    .await;
    let fresh = child_in_starting(&sys, &name).await;
    // The stop ends by the window, the kill grace and the settle time at
    // the latest.
    let took = stop_supervisor(
        &rig,
        upper(
            paused,
            window + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE + Duration::from_millis(500),
        ),
    )
    .await;
    let completed = probe.started.load(Ordering::SeqCst) == 2;
    assert_eq!(
        completed, expect_completion,
        "the incarnation's initialization completed: {completed}"
    );
    if !expect_completion {
        // The stop gave the starting incarnation its whole window, then its
        // `Kill`, then the abort one kill grace after that `Kill`, and the
        // incarnation reads `Kill`: an initialization that never completes
        // ends by the ladder, not by the stop giving up on it.
        assert!(
            took >= window + ASSUMED_KILL_GRACE,
            "the stop waited the window and the kill grace: {took:?}"
        );
        let reason = within(
            BOUND,
            "the killed incarnation's reason",
            fresh.wait_stopped(),
        )
        .await;
        assert!(matches!(reason, StopReason::Kill), "{reason:?}");
    }
    // The crashed incarnation ran `on_stopped`; the fresh one only if its
    // initialization completed inside its window.
    assert_eq!(
        probe.stopped_runs.load(Ordering::SeqCst),
        if expect_completion { 2 } else { 1 }
    );
}

#[test]
fn parent_stop_gives_a_starting_child_its_window_from_the_stop() {
    // `Timeout(500 ms)`: the init completes inside the window and the
    // incarnation stops cooperatively.
    common::run(
        0,
        starting_child_gets_its_window_from_the_stop(Duration::from_millis(500), true, false),
    );
    // `Timeout(100 ms)`: an init that never completes is killed at the window
    // and aborted after the grace, so the stop does not wait on the init.
    common::run(
        0,
        starting_child_gets_its_window_from_the_stop(Duration::from_millis(100), false, false),
    );
}

#[test]
fn parent_stop_gives_a_starting_child_its_window_from_the_stop_paused() {
    // Both legs on the paused clock, where the stop ends on its exact
    // instant: the bound of the stop is the window, the kill grace and the
    // settle time.
    common::run_paused(starting_child_gets_its_window_from_the_stop(
        Duration::from_millis(500),
        true,
        true,
    ));
    common::run_paused(starting_child_gets_its_window_from_the_stop(
        Duration::from_millis(100),
        false,
        true,
    ));
}

#[test]
fn parent_stop_bounds_a_hung_infinity_init_by_its_start_timeout() {
    common::run(
        0,
        parent_stop_bounds_a_hung_infinity_init_by_its_start_timeout_body(false),
    );
}

#[test]
fn parent_stop_bounds_a_hung_infinity_init_by_its_start_timeout_paused() {
    common::run_paused(parent_stop_bounds_a_hung_infinity_init_by_its_start_timeout_body(true));
}

async fn parent_stop_bounds_a_hung_infinity_init_by_its_start_timeout_body(paused: bool) {
    #[cfg(feature = "tracing")]
    let logged = warnings::install();
    let sys = ActorSystem::create(uname("hung-infinity")).unwrap();
    let rig = supervisor_on(&sys, plain()).await;
    let probe = Arc::new(Probe::default());
    let name = uname("w");
    let reply = rig
        .command(Cmd::SpawnWith {
            name: Some(name.clone()),
            probe: probe.clone(),
            shutdown: Shutdown::Infinity,
            restart: RestartType::Permanent,
            vetoes: 0,
            start_timeout: Some(Duration::from_millis(300)),
        })
        .await;
    assert!(matches!(reply, Reply::Id(_)), "{reply:?}");
    let child = child_in(&sys, &name).await;
    wait_until(BOUND, "the first incarnation to start", || {
        probe.started.load(Ordering::SeqCst) == 1
    })
    .await;
    // The restarted incarnation's `on_started` awaits forever.
    probe.held.store(true, Ordering::SeqCst);
    crash(&child).await;
    wait_until(BOUND, "the second incarnation to be built", || {
        probe.built.load(Ordering::SeqCst) == 2
    })
    .await;
    sleep(Duration::from_millis(50)).await;
    let took = stop_supervisor(
        &rig,
        upper(
            paused,
            Duration::from_millis(300)
                + ASSUMED_KILL_GRACE
                + ASSUMED_ABORT_SETTLE
                + Duration::from_millis(500),
        ),
    )
    .await;
    assert!(took < upper(paused, Duration::from_secs(2)), "{took:?}");
    assert!(
        probe.ended.load(Ordering::SeqCst),
        "the hung incarnation ended"
    );
    #[cfg(feature = "tracing")]
    {
        let lines = logged.lock().unwrap().clone();
        let naming = warnings::naming(&lines, &name);
        assert_eq!(naming.len(), 1, "{lines:?}");
        assert!(
            naming[0].contains("did not finish initializing within the start timeout"),
            "the attempt's end is the start timeout: {lines:?}"
        );
    }
}

#[test]
fn parent_stop_takes_a_child_whose_restart_acked_while_the_supervisor_was_busy() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("acked-busy")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let probe = Arc::new(Probe::default());
        let name = uname("w");
        rig.spawn_child(
            &name,
            &probe,
            Shutdown::Timeout(Duration::from_secs(1)),
            RestartType::Permanent,
            0,
        )
        .await;
        let child = child_in(&sys, &name).await;
        wait_until(BOUND, "the first incarnation to start", || {
            probe.started.load(Ordering::SeqCst) == 1
        })
        .await;
        // The restart's init is held until the supervisor is busy.
        probe.held.store(true, Ordering::SeqCst);
        crash(&child).await;
        wait_until(BOUND, "the restart attempt to be built", || {
            probe.built.load(Ordering::SeqCst) == 2
        })
        .await;
        // The supervisor's handler says it runs and holds until the stop is
        // raised: the restart's init acks while the handler runs, and the
        // stop is pending when the handler returns.
        let (entered, release) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let busy = {
            let sup = rig.sup.clone();
            let op = Op::Hold {
                entered: entered.clone(),
                release: release.clone(),
            };
            tokio::spawn(async move { sup.send(Cmd::Script(vec![op])).await })
        };
        wait_until(BOUND, "the supervisor's handler to run", || {
            entered.load(Ordering::SeqCst)
        })
        .await;
        probe.held.store(false, Ordering::SeqCst);
        // The incarnation publishes Running and acks in one step.
        wait_until(BOUND, "the restart to ack", || {
            probe.started.load(Ordering::SeqCst) == 2
                && sys.actor_status(child.id()) == Some(ActorStatus::Running)
        })
        .await;
        within(
            BOUND,
            "the stop request",
            rig.sup.stop(StopReason::Graceful),
        )
        .await
        .expect("stop");
        release.store(true, Ordering::SeqCst);
        within(
            Duration::from_secs(5),
            "the supervisor to stop",
            rig.sup.wait_stopped(),
        )
        .await;
        let _ = busy.await;
        // The child runs `on_stopped`: the crashed incarnation and the
        // adopted one.
        assert_eq!(probe.stopped_runs.load(Ordering::SeqCst), 2);
        assert!(probe.ended.load(Ordering::SeqCst));
    });
}

/// A permanent child with a start timeout, spawned on `rig`.
async fn spawn_with_start_timeout(
    rig: &Rig,
    name: &str,
    probe: &Arc<Probe>,
    shutdown: Shutdown,
    timeout: Duration,
) {
    let reply = rig
        .command(Cmd::SpawnWith {
            name: Some(name.to_string()),
            probe: probe.clone(),
            shutdown,
            restart: RestartType::Permanent,
            vetoes: 0,
            start_timeout: Some(timeout),
        })
        .await;
    assert!(matches!(reply, Reply::Id(_)), "{reply:?}");
    // The spawn call returns once the child is registered; its first
    // initialization is over only when it has started.
    wait_until(BOUND, "the first incarnation to start", || {
        probe.started.load(Ordering::SeqCst) == 1
    })
    .await;
}

#[test]
fn start_timeout_on_a_non_yielding_init_gives_the_verdict_and_never_restarts_beside_it() {
    // A real non-yielding init blocks a worker thread, so the attempt must run
    // on another one.
    #[cfg(feature = "tracing")]
    let logged = warnings::install();
    common::run_multi_thread_only(4, async {
        let sys = ActorSystem::create(uname("start-timeout")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let probe = Arc::new(Probe::default());
        let name = uname("w");
        spawn_with_start_timeout(&rig, &name, &probe, WINDOW_200, Duration::from_millis(100)).await;
        let child = child_in(&sys, &name).await;
        // The second incarnation holds its thread in `on_started` until the
        // test opens the gate, so every check below runs while it holds.
        probe.init_gate.store(true, Ordering::SeqCst);
        let gate = OpensTheInitGate(probe.clone());
        crash(&child).await;
        wait_until(BOUND, "the second incarnation's init to begin", || {
            probe.entered.load(Ordering::SeqCst) == 2
        })
        .await;
        // Past the start timeout, the kill grace and the abort wait: the
        // verdict is given while the init still holds its thread.
        #[cfg(feature = "tracing")]
        wait_until(BOUND, "the verdict to be logged", || {
            !warnings::naming(&logged.lock().unwrap(), &name).is_empty()
        })
        .await;
        // Without the log, nothing public shows the verdict: the wait runs well
        // past it, with room for a host that stalls, and the init still holds
        // its thread on the gate all the while.
        #[cfg(not(feature = "tracing"))]
        sleep(
            Duration::from_millis(100)
                + ASSUMED_KILL_GRACE
                + ASSUMED_ABORT_SETTLE
                + Duration::from_millis(2300),
        )
        .await;
        let lines = rig.script(vec![Op::Read(name.clone())]).await;
        assert_eq!(lines[0], "alive=false pending=true", "{lines:?}");
        assert_eq!(
            probe.built.load(Ordering::SeqCst),
            2,
            "no second incarnation is built beside the stuck one"
        );
        // Once the init returns, its end is recorded once and the restart
        // follows the child's policy; later incarnations do not hold.
        drop(gate);
        wait_until(BOUND, "the start failure to be reported", || {
            rig.events_for(&name).len() == 2
        })
        .await;
        wait_until(BOUND, "the restart that follows its policy", || {
            probe.built.load(Ordering::SeqCst) == 3
        })
        .await;
        sleep(QUIET).await;
        let events = rig.events_for(&name);
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(
            matches!(&events[1].reason, StopReason::Failure(ActorError::Spawn(_))),
            "the attempt's end is a start failure: {events:?}"
        );
        assert_eq!(events[1].action, SupervisionAction::RestartInitiated);
        let _ = child_in(&sys, &name).await;
        // The verdict reaches no caller: it is logged once.
        #[cfg(feature = "tracing")]
        {
            let lines = logged.lock().unwrap().clone();
            let verdicts = warnings::naming(&lines, &name);
            assert_eq!(verdicts.len(), 1, "{lines:?}");
        }
    });
}

#[cfg(feature = "tracing")]
#[test]
fn a_manual_stop_verdict_logs_once_and_the_caller_still_gets_it() {
    // Each verdict logs one warning when it is made; the caller that waited
    // for the child also receives it as `ChildUnresponsive`.
    let logged = warnings::install();
    common::run_multi_thread_only(4, async {
        let sys = ActorSystem::create(uname("manual-verdict")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let probe = Arc::new(Probe::default());
        let name = uname("a");
        // The turn holds its thread until the test ends, so the verdict is
        // made while the turn is known to run.
        probe.block_held.store(true, Ordering::SeqCst);
        let _release = ReleasesTheBlock(probe.clone());
        let _blocked =
            spawn_blocked_child(&sys, &rig, &name, &probe, Duration::from_millis(100)).await;
        match rig.command(Cmd::Stop { name: name.clone() }).await {
            Reply::Done(Err(text)) => assert_eq!(
                text,
                format!("child `{name}` or an actor it spawned did not end in time")
            ),
            other => panic!("expected ChildUnresponsive, got {other:?}"),
        }
        // Past anything the supervisor's loop would still read.
        rig.script(vec![Op::Sleep(Duration::from_millis(50))]).await;
        sleep(QUIET).await;
        let lines = logged.lock().unwrap().clone();
        let mine = warnings::naming(&lines, &name);
        assert_eq!(mine.len(), 1, "{lines:?}");
        assert!(
            mine[0].contains(&format!("supervisor={} ", rig.sup.id())),
            "the warning names the supervisor: {mine:?}"
        );
    });
}

#[cfg(feature = "tracing")]
#[test]
fn a_group_barrier_verdict_logs_once() {
    // The barrier also waits for a second, slow member: the first member's
    // verdict is made while the barrier still waits, and the supervisor stops
    // with it only when the second member ends. One line, made when the
    // verdict is made.
    let logged = warnings::install();
    common::run_multi_thread_only(4, async {
        let sys = ActorSystem::create(uname("barrier-verdict")).unwrap();
        let rig = supervisor_on(&sys, one_for_all()).await;
        let (pa, pb, pc) = (
            Arc::new(Probe::default()),
            Arc::new(Probe::default()),
            Arc::new(Probe::default()),
        );
        let (a, b, c) = (uname("a"), uname("b"), uname("c"));
        // The turn holds its thread until the test ends, so the verdict is
        // made while the turn is known to run.
        pa.block_held.store(true, Ordering::SeqCst);
        let _release = ReleasesTheBlock(pa.clone());
        let _blocked = spawn_blocked_child(&sys, &rig, &a, &pa, Duration::from_millis(100)).await;
        rig.spawn_child(
            &c,
            &pc,
            Shutdown::Timeout(Duration::from_millis(600)),
            RestartType::Permanent,
            always_vetoes(),
        )
        .await;
        rig.spawn_child(
            &b,
            &pb,
            Shutdown::Timeout(Duration::from_millis(200)),
            RestartType::Permanent,
            0,
        )
        .await;
        crash(&child_in(&sys, &b).await).await;
        within(BOUND, "the supervisor to stop", rig.sup.wait_stopped()).await;
        sleep(QUIET).await;
        let reasons = rig.knobs.stopped.lock().unwrap().clone();
        assert!(
            reasons.iter().any(|reason| matches!(
                reason,
                StopReason::Failure(ActorError::Supervision(
                    tokio_actors::SupervisionError::ChildUnresponsive(_)
                ))
            )),
            "the barrier stopped the supervisor with the verdict: {reasons:?}"
        );
        let lines = logged.lock().unwrap().clone();
        assert_eq!(warnings::naming(&lines, &a).len(), 1, "{lines:?}");
    });
}

#[cfg(feature = "tracing")]
#[test]
fn a_verdict_is_logged_when_the_supervisor_stops_before_its_loop_reads_the_signal() {
    // The verdict is made and its incarnation ends while the supervisor is
    // busy, and a stop arrives in the same stretch: the loop never reads the
    // signal, and the verdict is logged all the same.
    let logged = warnings::install();
    common::run_multi_thread_only(4, async {
        let sys = ActorSystem::create(uname("stop-before-signal")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let probe = Arc::new(Probe::default());
        let name = uname("w");
        spawn_with_start_timeout(&rig, &name, &probe, WINDOW_200, Duration::from_millis(100)).await;
        let child = child_in(&sys, &name).await;
        // The second incarnation holds its thread in `on_started` until the
        // test opens the gate, once its verdict is made.
        probe.init_gate.store(true, Ordering::SeqCst);
        let gate = OpensTheInitGate(probe.clone());
        crash(&child).await;
        wait_until(BOUND, "the second incarnation to reach its gate", || {
            probe.entered.load(Ordering::SeqCst) == 2
        })
        .await;
        // The supervisor is held in one turn while the verdict is made, the
        // incarnation ends and the stop arrives.
        let busy = hold_turn(&rig).await;
        wait_until(BOUND, "the verdict to be logged", || {
            !warnings::naming(&logged.lock().unwrap(), &name).is_empty()
        })
        .await;
        drop(gate);
        wait_until(BOUND, "the incarnation to end", || {
            probe.init_released.load(Ordering::SeqCst) == 1 && probe.ended.load(Ordering::SeqCst)
        })
        .await;
        rig.sup
            .stop(StopReason::Graceful)
            .await
            .expect("the stop is accepted");
        busy.release().await;
        within(BOUND, "the supervisor to stop", rig.sup.wait_stopped()).await;
        sleep(QUIET).await;
        let lines = logged.lock().unwrap().clone();
        assert_eq!(warnings::naming(&lines, &name).len(), 1, "{lines:?}");
    });
}

#[cfg(feature = "tracing")]
#[test]
fn each_verdict_nobody_receives_is_logged_once_for_its_child() {
    let logged = warnings::install();
    common::run_multi_thread_only(4, async {
        let sys = ActorSystem::create(uname("two-verdicts")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let (pa, pb) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
        let (a, b) = (uname("a"), uname("b"));
        spawn_with_start_timeout(&rig, &a, &pa, WINDOW_200, Duration::from_millis(100)).await;
        spawn_with_start_timeout(&rig, &b, &pb, WINDOW_200, Duration::from_millis(100)).await;
        let (child_a, child_b) = (child_in(&sys, &a).await, child_in(&sys, &b).await);
        wait_until(BOUND, "both first incarnations to start", || {
            pa.started.load(Ordering::SeqCst) == 1 && pb.started.load(Ordering::SeqCst) == 1
        })
        .await;
        // Both second incarnations hold their threads in `on_started` until
        // the test ends, so both verdicts are made while both inits hold.
        pa.init_gate.store(true, Ordering::SeqCst);
        pb.init_gate.store(true, Ordering::SeqCst);
        let _gates = (OpensTheInitGate(pa.clone()), OpensTheInitGate(pb.clone()));
        crash(&child_a).await;
        wait_until(BOUND, "a's second incarnation", || {
            pa.built.load(Ordering::SeqCst) == 2
        })
        .await;
        crash(&child_b).await;
        wait_until(BOUND, "b's second incarnation", || {
            pb.built.load(Ordering::SeqCst) == 2
        })
        .await;
        wait_until(
            BOUND,
            "both second incarnations to reach their gate",
            || pa.entered.load(Ordering::SeqCst) == 2 && pb.entered.load(Ordering::SeqCst) == 2,
        )
        .await;
        // Both verdicts are written (start timeout, kill grace, abort wait)
        // while both inits hold their threads, and each is logged.
        wait_until(BOUND, "both verdicts to be logged", || {
            let lines = logged.lock().unwrap();
            !warnings::naming(&lines, &a).is_empty() && !warnings::naming(&lines, &b).is_empty()
        })
        .await;
        // Further events of the loop do not repeat a line.
        rig.script(vec![Op::Sleep(Duration::from_millis(10))]).await;
        let lines = logged.lock().unwrap().clone();
        assert_eq!(warnings::naming(&lines, &a).len(), 1, "{lines:?}");
        assert_eq!(warnings::naming(&lines, &b).len(), 1, "{lines:?}");
    });
}

#[cfg(feature = "tracing")]
#[test]
fn a_verdict_is_logged_although_its_incarnation_ended_before_the_loop_read_the_signal() {
    let logged = warnings::install();
    common::run_multi_thread_only(4, async {
        let sys = ActorSystem::create(uname("ended-verdict")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let probe = Arc::new(Probe::default());
        let name = uname("w");
        spawn_with_start_timeout(&rig, &name, &probe, WINDOW_200, Duration::from_millis(100)).await;
        let child = child_in(&sys, &name).await;
        wait_until(BOUND, "the first incarnation to start", || {
            probe.started.load(Ordering::SeqCst) == 1
        })
        .await;
        // The second incarnation holds its thread in `on_started` until the
        // test opens the gate, once its verdict is made.
        probe.init_gate.store(true, Ordering::SeqCst);
        let gate = OpensTheInitGate(probe.clone());
        crash(&child).await;
        wait_until(BOUND, "the second incarnation to be built", || {
            probe.built.load(Ordering::SeqCst) == 2
        })
        .await;
        wait_until(BOUND, "the second incarnation to reach its gate", || {
            probe.entered.load(Ordering::SeqCst) == 2
        })
        .await;
        // The supervisor is held in one turn while the verdict is written and
        // while the init returns: the loop reads the attempt's outcome and the
        // signal only afterwards.
        let busy = hold_turn(&rig).await;
        wait_until(BOUND, "the verdict to be logged", || {
            !warnings::naming(&logged.lock().unwrap(), &name).is_empty()
        })
        .await;
        drop(gate);
        wait_until(BOUND, "the incarnation to end", || {
            probe.init_released.load(Ordering::SeqCst) == 1 && probe.ended.load(Ordering::SeqCst)
        })
        .await;
        busy.release().await;
        wait_until(BOUND, "the restart that follows", || {
            probe.built.load(Ordering::SeqCst) >= 3
        })
        .await;
        let lines = logged.lock().unwrap().clone();
        assert_eq!(warnings::naming(&lines, &name).len(), 1, "{lines:?}");
    });
}

#[test]
fn attempt_start_timeout_still_bounds_a_hung_init_after_a_bare_kill() {
    common::run_multi_thread_only(4, async {
        let sys = ActorSystem::create(uname("bare-kill")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let probe = Arc::new(Probe::default());
        let name = uname("w");
        // `Infinity` gives the stop of the supervisor no window to end the init
        // with: only the start timeout can.
        spawn_with_start_timeout(
            &rig,
            &name,
            &probe,
            Shutdown::Infinity,
            Duration::from_millis(100),
        )
        .await;
        let child = child_in(&sys, &name).await;
        // The second incarnation holds its thread in `on_started` until the
        // test opens the gate.
        probe.init_gate.store(true, Ordering::SeqCst);
        let gate = OpensTheInitGate(probe.clone());
        crash(&child).await;
        wait_until(BOUND, "the second incarnation to be built", || {
            probe.built.load(Ordering::SeqCst) == 2
        })
        .await;
        // The second incarnation is registered and holds its thread in
        // `on_started`: the kill finds it.
        wait_until(BOUND, "the second incarnation's init to begin", || {
            probe.entered.load(Ordering::SeqCst) == 2
        })
        .await;
        assert!(
            sys.kill_by_id(child.id()).await.is_some(),
            "the kill reaches the incarnation"
        );
        // The start timeout still acts, runs the grace and the abort, and
        // gives the verdict: about 300 ms into the init.
        sleep(
            Duration::from_millis(100)
                + ASSUMED_KILL_GRACE
                + ASSUMED_ABORT_SETTLE
                + Duration::from_millis(100),
        )
        .await;
        let lines = rig.script(vec![Op::Read(name.clone())]).await;
        assert_eq!(lines[0], "alive=false pending=true", "{lines:?}");
        // With the verdict given, the supervisor's stop records it and moves
        // on while the init still holds its thread: a stop that waited for
        // the init would end only once the gate opens, which comes after the
        // stop. The bound is a hang bound.
        stop_supervisor(&rig, BOUND).await;
        assert_eq!(
            probe.init_released.load(Ordering::SeqCst),
            0,
            "the supervisor published Stopped while the init held its thread"
        );
        drop(gate);
    });
}

#[test]
fn attempt_start_timeout_after_a_bare_kill_reports_the_end_once_with_kill() {
    common::run_multi_thread_only(4, async {
        let sys = ActorSystem::create(uname("bare-kill-report")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let probe = Arc::new(Probe::default());
        let name = uname("w");
        // A start timeout no stall of this task can reach before the kill.
        spawn_with_start_timeout(&rig, &name, &probe, WINDOW_200, Duration::from_secs(1)).await;
        let child = child_in(&sys, &name).await;
        // The second incarnation holds its thread in `on_started` until the
        // test opens the gate, so the checks below run while it holds.
        probe.init_gate.store(true, Ordering::SeqCst);
        let gate = OpensTheInitGate(probe.clone());
        crash(&child).await;
        // The second incarnation is registered and holds its thread in
        // `on_started`: the kill finds it.
        wait_until(BOUND, "the second incarnation's init to begin", || {
            probe.entered.load(Ordering::SeqCst) == 2
        })
        .await;
        assert!(sys.kill_by_id(child.id()).await.is_some());
        sleep(Duration::from_millis(500)).await;
        let lines = rig.script(vec![Op::Read(name.clone())]).await;
        assert_eq!(lines[0], "alive=false pending=true", "{lines:?}");
        assert_eq!(probe.built.load(Ordering::SeqCst), 2);
        // The init returns; later incarnations do not hold.
        drop(gate);
        wait_until(BOUND, "the end of the killed start to be reported", || {
            rig.events_for(&name).len() == 2
        })
        .await;
        wait_until(BOUND, "the restart that follows", || {
            probe.built.load(Ordering::SeqCst) == 3
        })
        .await;
        sleep(QUIET).await;
        let events = rig.events_for(&name);
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(
            matches!(events[1].reason, StopReason::Kill),
            "the kill came first: {events:?}"
        );
    });
}

#[test]
fn attempt_adopts_an_incarnation_that_acked_and_then_crashed_and_its_death_is_evaluated() {
    // The incarnation acks and fails in its first turn. The attempt is a
    // manual restart: a start failure of it would be reported removed and
    // charge nothing, while the death of an adopted incarnation is evaluated,
    // so the child is restarted by its policy. A stopping actor yields once
    // before its first callback, so on one thread the attempt reads the ack
    // before the death: this test checks the outcome for that order. The
    // order in which the ack is read after the death is reached by the
    // explorer's `small_manual_restart` runs, which require the witness "a
    // start that acked and then died, adopted before its death".
    common::run_current_thread_only(async {
        let sys = ActorSystem::create(uname("acked-dead")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let probe = Arc::new(Probe::default());
        let name = uname("w");
        rig.spawn_child(
            &name,
            &probe,
            Shutdown::Timeout(Duration::from_millis(200)),
            RestartType::Permanent,
            0,
        )
        .await;
        let _ = child_in(&sys, &name).await;
        rig.ok(Cmd::Terminate { name: name.clone() }).await;
        probe.crash_on_start.store(true, Ordering::SeqCst);
        rig.ok(Cmd::RestartChild { name: name.clone() }).await;
        wait_until(BOUND, "the restart that follows the second death", || {
            probe.built.load(Ordering::SeqCst) == 3
        })
        .await;
        sleep(QUIET).await;
        let events = rig.events_for(&name);
        assert_eq!(events.len(), 2, "one event per end: {events:?}");
        assert_eq!(events[0].action, SupervisionAction::Removed);
        assert!(
            matches!(&events[1].reason, StopReason::Failure(ActorError::Panic(_))),
            "the second incarnation's death is its own crash, never a start failure: {events:?}"
        );
        assert_eq!(events[1].action, SupervisionAction::RestartInitiated);
    });
}

/// A supervisor on `sys` whose `on_child_stopped` awaits `delay_ms`.
async fn slow_delivery_supervisor(
    sys: &Arc<ActorSystem>,
    config: SupervisionConfig,
    delay_ms: u64,
) -> Rig {
    spawn_sup(
        Some(sys),
        &uname("sup"),
        config,
        SupOpts {
            deliver_delay_ms: delay_ms,
            ..SupOpts::default()
        },
    )
    .await
}

/// A member abandoned in a manual stop with its stop still in progress at
/// the sibling's crash: the group restart joins the stop, restarts the
/// member only after its old task ended, and charges the budget once.
async fn group_waits_for_a_member_already_stopping(form: Abandoned) {
    let sys = ActorSystem::create(uname("group-stopping")).unwrap();
    // One restart per minute: a second charge would exhaust the budget.
    let rig = supervisor_on(
        &sys,
        SupervisionConfig::one_for_all().max_restarts(1, Duration::from_secs(60)),
    )
    .await;
    let (pa, pb) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
    let (a, b) = (uname("a"), uname("b"));
    // `b` starts first and crashes; `a` starts after it and is stopping.
    rig.spawn_child(
        &b,
        &pb,
        Shutdown::Timeout(Duration::from_millis(200)),
        RestartType::Permanent,
        0,
    )
    .await;
    rig.spawn_child(
        &a,
        &pa,
        Shutdown::Timeout(Duration::from_secs(1)),
        RestartType::Permanent,
        always_vetoes(),
    )
    .await;
    let child_b = child_in(&sys, &b).await;
    let _ = child_in(&sys, &a).await;
    rig.ok(match form {
        Abandoned::Terminate => Cmd::TerminateAbandon {
            name: a.clone(),
            wait: Duration::ZERO,
        },
        Abandoned::Stop => Cmd::StopAbandon {
            name: a.clone(),
            wait: Duration::ZERO,
        },
    })
    .await;
    let crashed = Instant::now();
    crash(&child_b).await;
    // The group starts no member until every committed member ended: `b` is
    // restarted only after `a`'s old incarnation, which vetoes for its whole
    // window, ended.
    wait_until(
        Duration::from_secs(5),
        "the crashed member to be restarted by the group",
        || pb.built.load(Ordering::SeqCst) == 2,
    )
    .await;
    assert!(
        crashed.elapsed() >= Duration::from_millis(800),
        "the group waited for the stopping member: {:?}",
        crashed.elapsed()
    );
    assert!(
        pa.ended.load(Ordering::SeqCst) || pa.built.load(Ordering::SeqCst) == 2,
        "the stopping member's old incarnation ended first"
    );
    wait_until(
        Duration::from_secs(5),
        "the stopping member to be restarted by the group",
        || pa.built.load(Ordering::SeqCst) == 2,
    )
    .await;
    sleep(QUIET * 2).await;
    assert_eq!(
        pa.built.load(Ordering::SeqCst),
        2,
        "the member is built exactly once more"
    );
    assert_eq!(
        pa.max_live.load(Ordering::SeqCst),
        1,
        "never two incarnations at once"
    );
    assert!(rig.sup.is_alive(), "the supervisor survived, charged once");
    for event in rig.events.lock().unwrap().iter() {
        assert_ne!(event.action, SupervisionAction::BudgetExhausted);
        assert!(
            !format!("{:?}", event.reason).contains("NameTaken"),
            "no restart attempt hit a taken name: {event:?}"
        );
    }
    let _ = child_in(&sys, &a).await;
}

#[test]
fn group_restart_waits_for_a_member_already_stopping() {
    common::run(
        0,
        group_waits_for_a_member_already_stopping(Abandoned::Stop),
    );
}

#[test]
fn group_restart_waits_for_a_member_whose_terminate_child_is_in_progress() {
    common::run(
        0,
        group_waits_for_a_member_already_stopping(Abandoned::Terminate),
    );
}

#[test]
fn group_restart_never_runs_two_incarnations_of_an_anonymous_member() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("anonymous-member")).unwrap();
        let rig = supervisor_on(
            &sys,
            SupervisionConfig::one_for_all().max_restarts(1, Duration::from_secs(60)),
        )
        .await;
        let (pa, pb) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
        let b = uname("b");
        // An anonymous member has no registered name to refuse a second
        // incarnation beside the first.
        let id = match rig
            .command(Cmd::SpawnWith {
                name: None,
                probe: pa.clone(),
                shutdown: Shutdown::Timeout(Duration::from_secs(1)),
                restart: RestartType::Permanent,
                vetoes: always_vetoes(),
                start_timeout: None,
            })
            .await
        {
            Reply::Id(id) => id,
            other => panic!("expected the child's id, got {other:?}"),
        };
        rig.spawn_child(
            &b,
            &pb,
            Shutdown::Timeout(Duration::from_millis(200)),
            RestartType::Permanent,
            0,
        )
        .await;
        let child_b = child_in(&sys, &b).await;
        rig.ok(Cmd::StopAbandon {
            name: id.clone(),
            wait: Duration::ZERO,
        })
        .await;
        crash(&child_b).await;
        wait_until(
            Duration::from_secs(5),
            "the member to be restarted by the group",
            || pa.built.load(Ordering::SeqCst) == 2,
        )
        .await;
        sleep(QUIET * 2).await;
        assert_eq!(pa.built.load(Ordering::SeqCst), 2);
        assert_eq!(pa.max_live.load(Ordering::SeqCst), 1);
        assert!(rig.sup.is_alive(), "the group was charged once");
        // `kill_by_id` reaches the live incarnation.
        assert!(sys
            .kill_by_id(&tokio_actors::ActorId::from(id))
            .await
            .is_some());
    });
}

#[test]
fn terminate_committed_before_the_group_trigger_rejoins_the_group_restart() {
    // Row (2) of the manual completion: a stop of a member the pending group
    // restarts is that group's restart, reported as one.
    common::run(0, async {
        // An abandoned terminate still stopping at the evaluation.
        let sys = ActorSystem::create(uname("rejoin-stopping")).unwrap();
        let rig = supervisor_on(&sys, one_for_all()).await;
        let (pa, pb) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
        let (a, b) = (uname("a"), uname("b"));
        rig.spawn_child(
            &a,
            &pa,
            Shutdown::Timeout(Duration::from_millis(300)),
            RestartType::Permanent,
            always_vetoes(),
        )
        .await;
        rig.spawn_child(
            &b,
            &pb,
            Shutdown::Timeout(Duration::from_millis(200)),
            RestartType::Permanent,
            0,
        )
        .await;
        let child_b = child_in(&sys, &b).await;
        let _ = child_in(&sys, &a).await;
        rig.ok(Cmd::TerminateAbandon {
            name: a.clone(),
            wait: Duration::ZERO,
        })
        .await;
        crash(&child_b).await;
        wait_until(BOUND, "the member to rejoin the restart", || {
            pa.built.load(Ordering::SeqCst) == 2
        })
        .await;
        wait_until(BOUND, "the member's report", || {
            !rig.events_for(&a).is_empty()
        })
        .await;
        sleep(QUIET).await;
        let events = rig.events_for(&a);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(
            events[0].action,
            SupervisionAction::RestartInitiated,
            "{events:?}"
        );

        // A completed terminate whose report is still pending when the group
        // is evaluated: the sibling's death is drained first.
        let sys = ActorSystem::create(uname("rejoin-completed")).unwrap();
        let rig = supervisor_on(&sys, one_for_all()).await;
        let (pa, pb) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
        let (a, b) = (uname("a"), uname("b"));
        rig.spawn_child(
            &a,
            &pa,
            Shutdown::Timeout(Duration::from_millis(200)),
            RestartType::Permanent,
            0,
        )
        .await;
        rig.spawn_child(
            &b,
            &pb,
            Shutdown::Timeout(Duration::from_millis(200)),
            RestartType::Permanent,
            0,
        )
        .await;
        let _ = child_in(&sys, &a).await;
        let _ = child_in(&sys, &b).await;
        let lines = rig
            .script(vec![
                Op::KillThroughHandle(b.clone()),
                // `b`'s runner completes before `a`'s stop begins, so `b`'s
                // report is drained first.
                Op::Sleep(Duration::from_millis(30)),
                Op::Terminate(a.clone()),
            ])
            .await;
        assert!(lines[2].contains("Ok"), "{lines:?}");
        wait_until(BOUND, "the member to rejoin the restart", || {
            pa.built.load(Ordering::SeqCst) == 2
        })
        .await;
        wait_until(BOUND, "the member's report", || {
            !rig.events_for(&a).is_empty()
        })
        .await;
        sleep(QUIET).await;
        let events = rig.events_for(&a);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(
            events[0].action,
            SupervisionAction::RestartInitiated,
            "{events:?}"
        );
    });
}

#[test]
fn terminate_committed_after_the_group_trigger_stays_down() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("terminate-after")).unwrap();
        let rig = supervisor_on(&sys, one_for_all()).await;
        let (pa, pb) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
        let (a, b) = (uname("a"), uname("b"));
        rig.spawn_child(
            &a,
            &pa,
            Shutdown::Timeout(Duration::from_millis(600)),
            RestartType::Permanent,
            always_vetoes(),
        )
        .await;
        rig.spawn_child(
            &b,
            &pb,
            Shutdown::Timeout(Duration::from_millis(200)),
            RestartType::Permanent,
            0,
        )
        .await;
        let child_b = child_in(&sys, &b).await;
        let _ = child_in(&sys, &a).await;
        crash(&child_b).await;
        // The group is pending: `a` is awaited in its vetoing window.
        wait_until(BOUND, "the group to order the member's stop", || {
            pa.pre_stops.load(Ordering::SeqCst) >= 1
        })
        .await;
        rig.ok(Cmd::Terminate { name: a.clone() }).await;
        wait_until(BOUND, "the member's report", || {
            !rig.events_for(&a).is_empty()
        })
        .await;
        wait_until(BOUND, "the sibling's restart", || {
            pb.built.load(Ordering::SeqCst) == 2
        })
        .await;
        sleep(QUIET).await;
        let events = rig.events_for(&a);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].action, SupervisionAction::Removed, "{events:?}");
        assert_eq!(
            pa.built.load(Ordering::SeqCst),
            1,
            "the released member stays down"
        );
    });
}

#[test]
fn a_member_death_queued_before_its_group_is_absorbed_once_without_a_second_charge() {
    #[cfg(feature = "tracing")]
    absorption_log::install();
    common::run(0, async {
        let sys = ActorSystem::create(uname("absorb-queued")).unwrap();
        // Two restarts per minute: a third charge would exhaust the budget.
        let rig = supervisor_on(
            &sys,
            SupervisionConfig::rest_for_one().max_restarts(2, Duration::from_secs(60)),
        )
        .await;
        let probes: Vec<Arc<Probe>> = (0..3).map(|_| Arc::new(Probe::default())).collect();
        let names: Vec<String> = ["a", "b", "c"].iter().map(|n| uname(n)).collect();
        for (name, probe) in names.iter().zip(&probes) {
            rig.spawn_child(
                name,
                probe,
                Shutdown::Timeout(Duration::from_millis(200)),
                RestartType::Permanent,
                0,
            )
            .await;
        }
        let handles: Vec<ActorHandle<Probed>> = {
            let mut v = Vec::new();
            for name in &names {
                v.push(child_in(&sys, name).await);
            }
            v
        };
        // Every first incarnation has read its knobs: what is set below
        // reaches only the restarted ones.
        wait_until(BOUND, "the children to finish starting", || {
            probes
                .iter()
                .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        // `c` crashes: its group (`c` alone) pends while `c`'s restart
        // initializes; `a` and `b` crash meanwhile, outside that group, and
        // wait behind it.
        probes[2].held.store(true, Ordering::SeqCst);
        let held = ReleasesTheHeldInit(probes[2].clone());
        crash(&handles[2]).await;
        wait_until(BOUND, "c's restart to be built", || {
            probes[2].built.load(Ordering::SeqCst) == 2
        })
        .await;
        // `c`'s restart waits in `on_started` until the test releases it: its
        // group pends while `a` and `b` crash.
        crash(&handles[0]).await;
        // The deaths of two different children reach the supervisor in no
        // fixed order: `a`'s is observed as owed its decision before `b`
        // crashes, and `b`'s before `c`'s restart is released.
        until_pending(&rig, &names[0]).await;
        crash(&handles[1]).await;
        until_pending(&rig, &names[1]).await;
        drop(held);
        // `a`'s evaluation starts a group that covers `b`, whose owed
        // evaluation is classified as the death plane: it is absorbed into the
        // group, delivered once, charged nothing. Either route gives one
        // `RestartInitiated` and no charge, so this pins the budget, not the
        // absorption.
        wait_until(BOUND, "b to be restarted", || {
            probes[1].built.load(Ordering::SeqCst) >= 2
        })
        .await;
        sleep(QUIET * 2).await;
        assert_eq!(
            probes[1].built.load(Ordering::SeqCst),
            2,
            "b is restarted once"
        );
        let events = rig.events_for(&names[1]);
        assert_eq!(events.len(), 1, "b is reported once: {events:?}");
        assert_eq!(events[0].action, SupervisionAction::RestartInitiated);
        assert!(rig.sup.is_alive(), "no second charge exhausted the budget");
        assert!(rig
            .events
            .lock()
            .unwrap()
            .iter()
            .all(|event| event.action != SupervisionAction::BudgetExhausted));
        // The completing group released both reports as one worklist, so b's
        // report was taken from the worklist when a's evaluation absorbed it.
        #[cfg(feature = "tracing")]
        assert_eq!(
            absorption_log::of(&names[1]),
            ["worklist"],
            "b's report was taken from the worklist"
        );
    });
}

/// OneForAll `[a, c]`, one handler: `a` crashes and ends, `c` sits in a held
/// turn that fails once ended, and `stop_child(c)` makes its commit before the
/// handler ends that turn, so the call completes inline on the child-died-first
/// path and `c`'s end is recorded as owing its evaluation while its report is
/// still in the death plane. `a`'s report starts the group, whose evaluation
/// absorbs `c` into a snapshot: `c` is built once more only, its old
/// incarnation is reported exactly once, with its own failure, and one charge
/// is taken. The children's `Shutdown` is `Infinity`: no window of the stop
/// can expire and kill `c` first, however late the host runs its turn.
async fn death_plane_report_is_absorbed_at_its_group_evaluation(delay_ms: u64) {
    let sys = ActorSystem::create(uname("absorb-death-plane")).unwrap();
    let rig = slow_delivery_supervisor(
        &sys,
        SupervisionConfig::one_for_all().max_restarts(1, Duration::from_secs(60)),
        delay_ms,
    )
    .await;
    let (pa, pc) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
    let (a, c) = (uname("a"), uname("c"));
    for (name, probe) in [(&a, &pa), (&c, &pc)] {
        rig.spawn_child(name, probe, Shutdown::Infinity, RestartType::Permanent, 0)
            .await;
    }
    let _ = child_in(&sys, &a).await;
    let _ = child_in(&sys, &c).await;
    pc.turn_held.store(true, Ordering::SeqCst);
    let lines = rig
        .script(vec![
            // `a` ends first, so its report is the one that starts the group.
            Op::CrashAndWait(a.clone()),
            Op::HeldFail(c.clone()),
            Op::UntilInTurn(pc.clone()),
            // `c` is still in its turn when the call commits: its failure
            // comes after the stop's request.
            Op::StopThenEndTurn(c.clone(), pc.clone()),
        ])
        .await;
    assert_eq!(lines[2], "in turn: true", "{lines:?}");
    assert!(lines[3].contains("Ok"), "{lines:?}");
    wait_until(BOUND, "c to be restarted", || {
        pc.built.load(Ordering::SeqCst) >= 2
    })
    .await;
    wait_until(BOUND, "c's report", || !rig.events_for(&c).is_empty()).await;
    sleep(QUIET * 2).await;
    assert_eq!(
        pc.built.load(Ordering::SeqCst),
        2,
        "c's factory runs once more only"
    );
    let events = rig.events_for(&c);
    assert_eq!(events.len(), 1, "c is reported exactly once: {events:?}");
    assert_eq!(events[0].action, SupervisionAction::RestartInitiated);
    assert!(
        matches!(events[0].reason, StopReason::Failure(_)),
        "{events:?}"
    );
    assert_eq!(rig.events_for(&a).len(), 1);
    assert!(
        rig.sup.is_alive(),
        "one charge only: the budget of one holds"
    );
}

#[test]
fn a_member_death_still_in_the_death_plane_at_its_group_evaluation_is_absorbed_once() {
    // The supervisor's `on_child_stopped` awaits, so the restart plane can
    // complete the chain's first attempt before the report drains.
    common::run_current_thread_only(death_plane_report_is_absorbed_at_its_group_evaluation(20));
}

#[test]
fn a_bounce_that_met_a_crash_keeps_its_report_when_the_chain_restarts_the_member() {
    // The supervisor's `on_child_stopped` awaits, so the chain can restart the
    // member before its report drains: the absorbed report is delivered from
    // its snapshot, once, on both runtime flavors.
    common::run(
        0,
        death_plane_report_is_absorbed_at_its_group_evaluation(100),
    );
}

#[test]
fn terminate_child_in_a_delivery_removes_a_queued_member_whose_absorbed_report_is_a_snapshot() {
    // As the test above, but the delivery of `a`'s report calls
    // `terminate_child(c)` while `c` is down in the restart set with its old
    // incarnation's report kept as a snapshot that announces a restart. The
    // terminate turns that report into the removal it made, once, and the
    // group never restarts `c`.
    common::run(0, async {
        let sys = ActorSystem::create(uname("snapshot-terminate")).unwrap();
        let rig = slow_delivery_supervisor(
            &sys,
            SupervisionConfig::one_for_all().max_restarts(1, Duration::from_secs(60)),
            50,
        )
        .await;
        let (pa, pc) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
        let (a, c) = (uname("a"), uname("c"));
        for (name, probe) in [(&a, &pa), (&c, &pc)] {
            rig.spawn_child(
                name,
                probe,
                Shutdown::Timeout(Duration::from_millis(200)),
                RestartType::Permanent,
                0,
            )
            .await;
        }
        let _ = child_in(&sys, &a).await;
        let _ = child_in(&sys, &c).await;
        rig.knobs
            .faults
            .lock()
            .unwrap()
            .push_back(Fault::Terminate(c.clone()));
        let lines = rig
            .script(vec![
                Op::CrashAndWait(a.clone()),
                Op::SlowFail(c.clone()),
                Op::UntilInTurn(pc.clone()),
                Op::Stop(c.clone()),
            ])
            .await;
        assert!(lines[3].contains("Ok"), "{lines:?}");
        wait_until(BOUND, "the terminate inside the delivery", || {
            rig.knobs
                .order
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.starts_with("terminate:"))
        })
        .await;
        until_idle(&rig, &[&a, &c]).await;
        wait_for_alive(&rig, &a).await;
        wait_until(BOUND, "c's report", || !rig.events_for(&c).is_empty()).await;
        sleep(QUIET * 2).await;
        let order = rig.knobs.order.lock().unwrap().clone();
        assert!(
            order
                .iter()
                .any(|line| line == &format!("terminate:{c}:Ok(())")),
            "{order:?}"
        );
        assert_eq!(pc.built.load(Ordering::SeqCst), 1, "c stays down");
        let events = rig.events_for(&c);
        assert_eq!(events.len(), 1, "c is reported exactly once: {events:?}");
        assert_eq!(events[0].action, SupervisionAction::Removed, "{events:?}");
        assert!(
            matches!(events[0].reason, StopReason::Failure(_)),
            "{events:?}"
        );
        assert!(rig.sup.is_alive(), "one charge only: the budget holds");
    });
}

/// Clears a probe's `block_held` when dropped, so a held `Block` turn ends
/// however the test ends.
struct ReleasesTheBlock(Arc<Probe>);

impl Drop for ReleasesTheBlock {
    fn drop(&mut self) {
        self.0.block_held.store(false, Ordering::SeqCst);
    }
}

/// Clears a probe's `init_gate` when dropped, so a held init goes on however
/// the test ends.
struct OpensTheInitGate(Arc<Probe>);

impl Drop for OpensTheInitGate {
    fn drop(&mut self) {
        self.0.init_gate.store(false, Ordering::SeqCst);
    }
}

/// Clears a probe's `held` when dropped, so an init waiting on it goes on
/// however the test ends.
struct ReleasesTheHeldInit(Arc<Probe>);

impl Drop for ReleasesTheHeldInit {
    fn drop(&mut self) {
        self.0.held.store(false, Ordering::SeqCst);
    }
}

/// Spawns a probed child and sends it a message that blocks its thread while
/// its probe's `block_held` is set; returns once the turn has started.
async fn spawn_blocked_child(
    sys: &Arc<ActorSystem>,
    rig: &Rig,
    name: &str,
    probe: &Arc<Probe>,
    window: Duration,
) -> ActorHandle<Probed> {
    rig.spawn_child(
        name,
        probe,
        Shutdown::Timeout(window),
        RestartType::Permanent,
        0,
    )
    .await;
    let child = child_in(sys, name).await;
    child
        .notify(ChildMsg::Block)
        .await
        .expect("start the blocked turn");
    wait_until(BOUND, "the turn to start", || {
        probe.in_turn.load(Ordering::SeqCst)
    })
    .await;
    child
}

#[test]
fn group_blocked_only_by_an_unresponsive_member_stops_the_supervisor() {
    // The member's turn blocks a worker thread, so everything else must run
    // on another one.
    common::run_multi_thread_only(4, async {
        let sys = ActorSystem::create(uname("group-verdict")).unwrap();
        let rig = supervisor_on(&sys, one_for_all()).await;
        let (pa, pb) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
        let (a, b) = (uname("a"), uname("b"));
        // The turn holds its thread until the test ends, so the verdict is
        // made while the turn is known to run.
        pa.block_held.store(true, Ordering::SeqCst);
        let _release = ReleasesTheBlock(pa.clone());
        let _blocked = spawn_blocked_child(&sys, &rig, &a, &pa, Duration::from_millis(100)).await;
        rig.spawn_child(
            &b,
            &pb,
            Shutdown::Timeout(Duration::from_millis(200)),
            RestartType::Permanent,
            0,
        )
        .await;
        let child_b = child_in(&sys, &b).await;
        crash(&child_b).await;
        within(
            Duration::from_secs(10),
            "the supervisor to stop with the verdict",
            rig.sup.wait_stopped(),
        )
        .await;
        let reasons = rig.knobs.stopped.lock().unwrap().clone();
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        match &reasons[0] {
            StopReason::Failure(ActorError::Supervision(
                tokio_actors::SupervisionError::ChildUnresponsive(id),
            )) => assert_eq!(id.as_str(), a),
            other => panic!("expected ChildUnresponsive({a}), got {other:?}"),
        }
    });
}

#[test]
fn manual_stop_of_a_non_yielding_child_returns_unresponsive_and_never_restarts_it_while_it_lives() {
    common::run_multi_thread_only(4, async {
        let sys = ActorSystem::create(uname("manual-verdict")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let probe = Arc::new(Probe::default());
        let name = uname("a");
        // The turn holds its thread until the test releases it, so the
        // verdict is read while the turn is known to run.
        probe.block_held.store(true, Ordering::SeqCst);
        let release = ReleasesTheBlock(probe.clone());
        let _blocked =
            spawn_blocked_child(&sys, &rig, &name, &probe, Duration::from_millis(100)).await;
        let reply = rig.command(Cmd::Stop { name: name.clone() }).await;
        match reply {
            Reply::Done(Err(text)) => assert_eq!(
                text,
                format!("child `{name}` or an actor it spawned did not end in time")
            ),
            other => panic!("expected ChildUnresponsive, got {other:?}"),
        }
        assert!(
            !probe.ended.load(Ordering::SeqCst),
            "the verdict came while the stuck turn still runs"
        );
        // The stuck incarnation still lives: no factory call, no second
        // incarnation under its id.
        sleep(Duration::from_millis(300)).await;
        assert_eq!(probe.built.load(Ordering::SeqCst), 1);
        // Once the turn ends, the bounce completes and the policy restarts it.
        drop(release);
        wait_until(BOUND, "the restart after the turn ended", || {
            probe.built.load(Ordering::SeqCst) == 2
        })
        .await;
        let _ = child_in(&sys, &name).await;
    });
}

#[test]
fn parent_stop_records_a_non_yielding_child_and_completes() {
    #[cfg(feature = "tracing")]
    let logged = warnings::install();
    common::run_multi_thread_only(4, async {
        let sys = ActorSystem::create(uname("parent-verdict")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let probe = Arc::new(Probe::default());
        let name = uname("a");
        // The turn holds its thread until the test releases it.
        probe.block_held.store(true, Ordering::SeqCst);
        let _release = ReleasesTheBlock(probe.clone());
        let _blocked =
            spawn_blocked_child(&sys, &rig, &name, &probe, Duration::from_millis(100)).await;
        let started = Instant::now();
        stop_supervisor(&rig, Duration::from_secs(5)).await;
        // After the child's ladder and grace, before its turn ends.
        assert!(
            started.elapsed() >= Duration::from_millis(100) + ASSUMED_KILL_GRACE,
            "{:?}",
            started.elapsed()
        );
        assert!(
            !probe.ended.load(Ordering::SeqCst),
            "the stuck turn still runs"
        );
        // The teardown's verdict reaches no caller: it is logged once.
        #[cfg(feature = "tracing")]
        {
            let lines = logged.lock().unwrap().clone();
            let verdicts = warnings::naming(&lines, &name);
            assert_eq!(verdicts.len(), 1, "{lines:?}");
        }
    });
}

// ---------------------------------------------------------------------------
// The final deliveries: the child deaths still pending when a supervisor's
// message loop ends are delivered once, before its `on_stopped`
// ---------------------------------------------------------------------------

/// The supervisor's handler held in one turn until `release`: everything
/// raised on the supervisor or ended below it meanwhile is read by its loop
/// only once the turn ends, however late the host runs the test.
struct HeldTurn {
    release: Arc<AtomicBool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl HeldTurn {
    /// Ends the held turn and waits for its reply.
    async fn release(mut self) {
        self.release.store(true, Ordering::SeqCst);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for HeldTurn {
    fn drop(&mut self) {
        self.release.store(true, Ordering::SeqCst);
    }
}

/// Holds the supervisor's handler and returns once the turn has started.
async fn hold_turn(rig: &Rig) -> HeldTurn {
    let (entered, release) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let sup = rig.sup.clone();
    let op = Op::Hold {
        entered: entered.clone(),
        release: release.clone(),
    };
    let task = tokio::spawn(async move {
        let _ = sup.send(Cmd::Script(vec![op])).await;
    });
    wait_until(BOUND, "the supervisor's handler to run", || {
        entered.load(Ordering::SeqCst)
    })
    .await;
    HeldTurn {
        release,
        task: Some(task),
    }
}

/// Crashes each of `kids` and waits until its instance is gone, so its
/// death is pending to a supervisor held in a turn.
async fn crash_and_wait(kids: &[&(String, Arc<Probe>, ActorHandle<Probed>)]) {
    for (_, probe, child) in kids {
        crash(child).await;
        wait_until(BOUND, "the crashed child's instance to drop", || {
            probe.ended.load(Ordering::SeqCst)
        })
        .await;
    }
}

/// Spawns `names` as permanent children with a probe each.
async fn spawn_children(
    sys: &Arc<ActorSystem>,
    rig: &Rig,
    names: &[&str],
) -> Vec<(String, Arc<Probe>, ActorHandle<Probed>)> {
    let mut out = Vec::new();
    for base in names {
        let name = uname(base);
        let probe = Arc::new(Probe::default());
        rig.spawn_child(
            &name,
            &probe,
            Shutdown::Timeout(Duration::from_millis(300)),
            RestartType::Permanent,
            0,
        )
        .await;
        let handle = child_in(sys, &name).await;
        out.push((name, probe, handle));
    }
    out
}

fn deliveries(rig: &Rig) -> Vec<String> {
    rig.knobs
        .order
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.starts_with("delivery:") || *line == "on_stopped")
        .cloned()
        .collect()
}

#[test]
fn a_crash_pending_at_the_supervisors_stop_is_delivered_once_with_supervisor_stopping_before_on_stopped(
) {
    common::run(0, async {
        let sys = ActorSystem::create(uname("final-pending")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let kids = spawn_children(&sys, &rig, &["a"]).await;
        let (a, probe, _) = &kids[0];
        // The crash and the stop land while the supervisor is in a turn.
        let held = hold_turn(&rig).await;
        crash_and_wait(&[&kids[0]]).await;
        rig.sup.stop(StopReason::Graceful).await.expect("stop");
        held.release().await;
        within(BOUND, "the supervisor to stop", rig.sup.wait_stopped()).await;
        assert_eq!(
            deliveries(&rig),
            vec![
                format!("delivery:{a}:SupervisorStopping:Stopping"),
                "on_stopped".to_string()
            ]
        );
        let events = rig.events_for(a);
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(
            matches!(events[0].reason, StopReason::Failure(_)),
            "{events:?}"
        );
        assert_eq!(probe.built.load(Ordering::SeqCst), 1, "no restart follows");
        assert!(matches!(
            rig.knobs.stopped.lock().unwrap()[0],
            StopReason::Graceful
        ));
    });
}

#[test]
fn a_kill_during_the_final_deliveries_stops_the_rest() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("final-kill")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let kids = spawn_children(&sys, &rig, &["a", "b"]).await;
        rig.knobs.faults.lock().unwrap().push_back(Fault::KillSelf);
        // Both crashes and the stop land while the supervisor is in a turn,
        // so both deaths are still pending when its loop ends.
        let held = hold_turn(&rig).await;
        crash_and_wait(&[&kids[0], &kids[1]]).await;
        rig.sup.stop(StopReason::Graceful).await.expect("stop");
        held.release().await;
        within(BOUND, "the supervisor to end", rig.sup.wait_stopped()).await;
        let lines = deliveries(&rig);
        assert_eq!(lines.len(), 1, "exactly one final delivery ran: {lines:?}");
        assert!(lines[0].starts_with("delivery:"));
        assert!(
            rig.knobs.stopped.lock().unwrap().is_empty(),
            "on_stopped never ran"
        );
    });
}

#[test]
fn a_kill_before_the_final_drain_delivers_nothing() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("final-kill-first")).unwrap();
        let name = uname("sup");
        let rig = spawn_sup(Some(&sys), &name, plain(), SupOpts::default()).await;
        let kids = spawn_children(&sys, &rig, &["a"]).await;
        // The supervisor's handler holds until the stop and the kill are
        // raised, so the child's death is still pending to it when its loop
        // ends.
        let (entered, release) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let busy = {
            let sup = rig.sup.clone();
            let op = Op::Hold {
                entered: entered.clone(),
                release: release.clone(),
            };
            tokio::spawn(async move { sup.send(Cmd::Script(vec![op])).await })
        };
        wait_until(BOUND, "the supervisor's handler to run", || {
            entered.load(Ordering::SeqCst)
        })
        .await;
        crash(&kids[0].2).await;
        wait_until(BOUND, "the child's task to end", || {
            kids[0].1.ended.load(Ordering::SeqCst)
        })
        .await;
        rig.sup.stop(StopReason::Graceful).await.expect("stop");
        sys.kill(&name).await.expect("kill");
        release.store(true, Ordering::SeqCst);
        within(BOUND, "the supervisor to end", rig.sup.wait_stopped()).await;
        let _ = busy.await;
        assert!(deliveries(&rig).is_empty(), "{:?}", deliveries(&rig));
        assert!(rig.knobs.stopped.lock().unwrap().is_empty());
    });
}

#[test]
fn supervision_calls_inside_a_final_delivery_are_refused() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("final-calls")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let kids = spawn_children(&sys, &rig, &["a", "b"]).await;
        let b = kids[1].0.clone();
        rig.knobs
            .faults
            .lock()
            .unwrap()
            .push_back(Fault::Calls(b.clone()));
        // The crash and the stop land while the supervisor is in a turn.
        let held = hold_turn(&rig).await;
        crash_and_wait(&[&kids[0]]).await;
        rig.sup.stop(StopReason::Graceful).await.expect("stop");
        held.release().await;
        within(BOUND, "the supervisor to stop", rig.sup.wait_stopped()).await;
        let calls = rig
            .knobs
            .order
            .lock()
            .unwrap()
            .iter()
            .find(|line| line.starts_with("calls:"))
            .cloned()
            .expect("the delivery made its calls");
        for call in [
            "stop_child",
            "terminate_child",
            "restart_child",
            "delete_child",
            "spawn_child",
        ] {
            let answer = calls
                .split('|')
                .find(|part| part.contains(call))
                .unwrap_or_else(|| panic!("no answer for {call}: {calls}"));
            assert!(
                answer.contains("the supervisor is stopping")
                    || answer.contains("SupervisorStopping"),
                "{call}: {answer}"
            );
        }
        assert!(
            calls.contains("children 2"),
            "children() stays readable: {calls}"
        );
        // The teardown stops the remaining child under its policy as usual.
        assert!(kids[1].1.ended.load(Ordering::SeqCst));
        assert_eq!(kids[1].1.stopped_runs.load(Ordering::SeqCst), 1);
        assert!(sys.get::<Probed>(&format!("{b}-spawned")).is_none());
    });
}

/// How the child supervisor of `failing_final_delivery` ends and how its
/// parent sees it.
struct Nested {
    root: Rig,
    child_name: String,
    child_events: Events,
    child_knobs: Arc<Knobs>,
    built: Arc<AtomicUsize>,
    sys: Arc<ActorSystem>,
    inner: ActorHandle<Sup>,
}

async fn nested_supervisor(restart: RestartType, faults: Vec<Fault>) -> Nested {
    let sys = ActorSystem::create(uname("nested")).unwrap();
    let root = supervisor_on(&sys, plain()).await;
    let child_name = uname("inner");
    let child_events: Events = Arc::new(Mutex::new(Vec::new()));
    let child_knobs = Arc::new(Knobs::default());
    child_knobs.faults.lock().unwrap().extend(faults);
    let built = Arc::new(AtomicUsize::new(0));
    root.ok(Cmd::SpawnSupervisorChild {
        name: child_name.clone(),
        shutdown: Shutdown::Timeout(Duration::from_secs(5)),
        restart,
        config: plain(),
        built: built.clone(),
        events: child_events.clone(),
        knobs: child_knobs.clone(),
    })
    .await;
    let inner = sys
        .get::<Sup>(&child_name)
        .expect("the child supervisor is registered");
    Nested {
        root,
        child_name,
        child_events,
        child_knobs,
        built,
        sys,
        inner,
    }
}

impl Nested {
    fn inner(&self) -> ActorHandle<Sup> {
        self.inner.clone()
    }

    /// Two children of the inner supervisor crash during a busy handler of
    /// it, then the inner supervisor is stopped.
    async fn crash_two_then_stop(&self) -> Vec<String> {
        let inner = self.inner();
        let rig = Rig {
            sup: inner.clone(),
            events: self.child_events.clone(),
            knobs: self.child_knobs.clone(),
        };
        let kids = spawn_children(&self.sys, &rig, &["x", "y"]).await;
        let held = hold_turn(&rig).await;
        crash_and_wait(&[&kids[0], &kids[1]]).await;
        inner.stop(StopReason::Graceful).await.expect("stop");
        held.release().await;
        within(BOUND, "the inner supervisor to stop", inner.wait_stopped()).await;
        kids.iter().map(|k| k.0.clone()).collect()
    }
}

#[test]
fn a_failing_final_delivery_makes_the_exit_reason_failure_and_the_rest_still_run() {
    common::run(0, async {
        // The first final delivery returns an error: the second still runs,
        // `on_stopped` receives the reason the supervisor accepted and reads
        // the error, and the parent sees a failure.
        let nested = nested_supervisor(RestartType::Permanent, vec![Fault::Err("first")]).await;
        let _kids = nested.crash_two_then_stop().await;
        let lines = deliveries(&Rig {
            sup: nested.inner(),
            events: nested.child_events.clone(),
            knobs: nested.child_knobs.clone(),
        });
        assert_eq!(lines.len(), 3, "two deliveries and on_stopped: {lines:?}");
        assert!(lines[0].contains("SupervisorStopping") && lines[1].contains("SupervisorStopping"));
        assert_eq!(lines[2], "on_stopped");
        assert!(matches!(
            nested.child_knobs.stopped.lock().unwrap()[0],
            StopReason::Graceful
        ));
        let seen = nested.child_knobs.last_errors.lock().unwrap()[0].clone();
        assert!(
            seen.is_some_and(|e| e.contains("first")),
            "last_error is the failed delivery's error"
        );
        wait_until(BOUND, "the parent to see the inner supervisor end", || {
            !nested.root.events_for(&nested.child_name).is_empty()
        })
        .await;
        let reported = nested.root.events_for(&nested.child_name);
        assert!(
            matches!(&reported[0].reason, StopReason::Failure(err) if err.to_string().contains("first")),
            "{reported:?}"
        );

        // Two failing deliveries: the last error wins.
        let nested = nested_supervisor(
            RestartType::Permanent,
            vec![Fault::Err("one"), Fault::Err("two")],
        )
        .await;
        nested.crash_two_then_stop().await;
        wait_until(BOUND, "the parent to see the inner supervisor end", || {
            !nested.root.events_for(&nested.child_name).is_empty()
        })
        .await;
        let reported = nested.root.events_for(&nested.child_name);
        assert!(
            matches!(&reported[0].reason, StopReason::Failure(err) if err.to_string().contains("two")),
            "{reported:?}"
        );

        // A panic in the first delivery ends the same way.
        let nested = nested_supervisor(RestartType::Permanent, vec![Fault::Panic]).await;
        nested.crash_two_then_stop().await;
        wait_until(BOUND, "the parent to see the inner supervisor end", || {
            !nested.root.events_for(&nested.child_name).is_empty()
        })
        .await;
        let reported = nested.root.events_for(&nested.child_name);
        assert!(
            matches!(
                &reported[0].reason,
                StopReason::Failure(ActorError::Panic(_))
            ),
            "{reported:?}"
        );

        // A transient supervisor is then restarted by its parent.
        let nested = nested_supervisor(RestartType::Transient, vec![Fault::Err("again")]).await;
        nested.crash_two_then_stop().await;
        wait_until(BOUND, "the parent to restart the inner supervisor", || {
            nested.built.load(Ordering::SeqCst) == 2
        })
        .await;
    });
}

#[test]
fn final_drain_awaits_the_runner_of_a_child_that_ended_in_the_same_turn() {
    // With one thread, the child's task has ended while its runner has not
    // run yet when the supervisor raises its own stop.
    common::run_current_thread_only(async {
        let sys = ActorSystem::create(uname("final-runner")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let kids = spawn_children(&sys, &rig, &["a"]).await;
        let a = kids[0].0.clone();
        let lines = rig
            .script(vec![Op::CrashAndWait(a.clone()), Op::StopSelf])
            .await;
        assert_eq!(lines, vec!["ended".to_string(), "stop raised".to_string()]);
        within(BOUND, "the supervisor to stop", rig.sup.wait_stopped()).await;
        assert_eq!(
            deliveries(&rig),
            vec![
                format!("delivery:{a}:SupervisorStopping:Stopping"),
                "on_stopped".to_string()
            ]
        );
        assert_eq!(rig.events_for(&a).len(), 1);
    });
}

/// The restarted incarnation fails its init while the supervisor's handler
/// waits for that failure's signal, and the handler then raises the
/// supervisor's own stop. On one thread the signal wakes the supervisor
/// before the incarnation's end wakes its restart attempt (the init ack is
/// answered in that end), so the supervisor's final drain begins with the
/// attempt still in flight and its incarnation already ended, and must wait
/// for it to deliver the failure. Several threads cannot hold that order by
/// observation: no callback runs between the ack and the end of the
/// incarnation's task, so this runs on one thread only.
#[test]
fn final_drain_awaits_a_restart_attempt_whose_incarnation_ended_in_the_same_turn() {
    common::run_current_thread_only(async {
        let sys = ActorSystem::create(uname("final-attempt")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let kids = spawn_children(&sys, &rig, &["a"]).await;
        let (a, probe, child) = (&kids[0].0, &kids[0].1, &kids[0].2);
        // The restarted incarnation waits in its init until released.
        probe.held.store(true, Ordering::SeqCst);
        crash(child).await;
        wait_until(BOUND, "the second incarnation to be built", || {
            probe.built.load(Ordering::SeqCst) == 2
        })
        .await;
        let second = child_in_starting(&sys, a).await;
        let lines = rig
            .script(vec![
                Op::FailInitAndAwaitSignal(probe.clone(), second),
                Op::StopSelf,
            ])
            .await;
        // The incarnation had ended when the supervisor woke.
        assert_eq!(
            lines,
            vec![
                "signalled ended=true".to_string(),
                "stop raised".to_string()
            ]
        );
        within(BOUND, "the supervisor to stop", rig.sup.wait_stopped()).await;
        let log = deliveries(&rig);
        assert_eq!(
            log.last().map(String::as_str),
            Some("on_stopped"),
            "{log:?}"
        );
        assert!(
            log.contains(&format!("delivery:{a}:SupervisorStopping:Stopping")),
            "the failed attempt is delivered once with its real reason: {log:?}"
        );
        let events = rig.events_for(a);
        let last = events.last().expect("an event");
        assert_eq!(last.action, SupervisionAction::SupervisorStopping);
        assert!(
            matches!(&last.reason, StopReason::Failure(err) if err.to_string().contains("refused to start")),
            "{events:?}"
        );
    });
}

/// The handle of a child incarnation that is still initializing.
async fn child_in_starting(sys: &Arc<ActorSystem>, name: &str) -> ActorHandle<Probed> {
    let mut found = None;
    wait_until(BOUND, "the child to be registered", || {
        found = sys.get::<Probed>(name);
        found.is_some()
    })
    .await;
    found.expect("registered")
}

#[test]
fn final_drain_delivers_an_adopted_restart_that_ended_in_transit() {
    common::run_current_thread_only(async {
        let sys = ActorSystem::create(uname("final-transit")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let kids = spawn_children(&sys, &rig, &["a"]).await;
        let (a, probe, child) = (&kids[0].0, &kids[0].1, &kids[0].2);
        // The restarted incarnation waits in its initialization, then acks
        // and fails in its first turn.
        probe.crash_on_start.store(true, Ordering::SeqCst);
        probe.held.store(true, Ordering::SeqCst);
        crash(child).await;
        wait_until(BOUND, "the second incarnation to be built", || {
            probe.built.load(Ordering::SeqCst) == 2
        })
        .await;
        // The ack and the failure come while the supervisor is in a turn, and
        // the stop lands in that turn too.
        let held = hold_turn(&rig).await;
        probe.held.store(false, Ordering::SeqCst);
        wait_until(BOUND, "the second incarnation's instance to drop", || {
            probe.ended.load(Ordering::SeqCst)
        })
        .await;
        rig.sup.stop(StopReason::Graceful).await.expect("stop");
        held.release().await;
        within(BOUND, "the supervisor to stop", rig.sup.wait_stopped()).await;
        let log = deliveries(&rig);
        assert!(
            log.contains(&format!("delivery:{a}:SupervisorStopping:Stopping")),
            "the death of the adopted incarnation is delivered once: {log:?}"
        );
        assert_eq!(log.last().map(String::as_str), Some("on_stopped"));
        let stopping = log
            .iter()
            .filter(|line| line.contains("SupervisorStopping"))
            .count();
        assert_eq!(stopping, 1, "{log:?}");
    });
}

#[test]
fn a_report_pending_at_budget_exhaustion_is_delivered_once_with_supervisor_stopping() {
    // UNVERIFIED shape: it rests on the current-thread scheduler completing
    // `a`'s runner before `b`'s, so that the loop drains `a`'s report first.
    common::run_current_thread_only(async {
        let sys = ActorSystem::create(uname("final-exhausted")).unwrap();
        let rig = supervisor_on(
            &sys,
            SupervisionConfig::one_for_one().max_restarts(1, Duration::from_secs(60)),
        )
        .await;
        let kids = spawn_children(&sys, &rig, &["a", "b"]).await;
        let (a, b) = (kids[0].0.clone(), kids[1].0.clone());
        // Spend the budget of one.
        crash(&kids[0].2).await;
        wait_until(BOUND, "the budget's one restart", || {
            kids[0].1.built.load(Ordering::SeqCst) == 2
        })
        .await;
        let _ = child_in(&sys, &a).await;
        wait_until(BOUND, "the first crash to be reported", || {
            !rig.events_for(&a).is_empty()
        })
        .await;
        let lines = rig
            .script(vec![Op::CrashAndWait(a.clone()), Op::Stop(b.clone())])
            .await;
        assert!(lines[1].contains("Ok"), "{lines:?}");
        within(BOUND, "the supervisor to stop", rig.sup.wait_stopped()).await;
        let log = deliveries(&rig);
        let exhausted = log
            .iter()
            .position(|line| line.starts_with(&format!("delivery:{a}:BudgetExhausted")));
        let stopping = log
            .iter()
            .position(|line| line.starts_with(&format!("delivery:{b}:SupervisorStopping")));
        assert!(
            exhausted.is_some() && stopping.is_some() && exhausted < stopping,
            "a's exhaustion, then b's report as a final delivery, whichever drained first: {log:?}"
        );
        assert_eq!(log.last().map(String::as_str), Some("on_stopped"));
        assert_eq!(
            log.iter()
                .filter(|l| l.contains(&format!("delivery:{b}")))
                .count(),
            1,
            "{log:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// A child that ends unexpectedly while its supervisor stops it, and a
// supervisor whose initialization fails
// ---------------------------------------------------------------------------

/// How a supervisor's initialization fails.
#[derive(Clone)]
enum InitMode {
    /// `c1` fails after a slow turn before the initialization returns an
    /// error; `c2` is healthy.
    ChildFailedDuring,
    /// `c3`'s own initialization fails, which `spawn_child` never awaits.
    ChildInitFails,
    /// `spawn_child` is refused for a taken name and the error is returned.
    NameTaken(String),
}

#[derive(Default)]
struct InitCounts {
    child_stopped: AtomicUsize,
    stopped: AtomicUsize,
}

struct InitFails {
    mode: InitMode,
    probes: (Arc<Probe>, Arc<Probe>),
    names: (String, String),
    counts: Arc<InitCounts>,
}

impl Actor for InitFails {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let (p1, p2) = self.probes.clone();
        let (n1, n2) = self.names.clone();
        match self.mode.clone() {
            InitMode::ChildFailedDuring => {
                let c1 = ctx
                    .spawn_child(move || p1.instance(0))
                    .named(n1.clone())
                    .await?;
                ctx.spawn_child(move || p2.instance(0)).named(n2).await?;
                c1.notify(ChildMsg::SlowFail).await.ok();
                let deadline = Instant::now() + Duration::from_secs(1);
                while ctx
                    .children()
                    .iter()
                    .any(|info| info.id.as_str() == n1 && info.is_alive)
                    && Instant::now() < deadline
                {
                    sleep(Duration::from_millis(5)).await;
                }
            }
            InitMode::ChildInitFails => {
                p1.fail_init.store(true, Ordering::SeqCst);
                ctx.spawn_child(move || p1.instance(0))
                    .named(n1.clone())
                    .await?;
                let deadline = Instant::now() + Duration::from_secs(1);
                while ctx
                    .children()
                    .iter()
                    .any(|info| info.id.as_str() == n1 && info.is_alive)
                    && Instant::now() < deadline
                {
                    sleep(Duration::from_millis(5)).await;
                }
            }
            InitMode::NameTaken(taken) => {
                ctx.spawn_child(move || p1.instance(0)).named(taken).await?;
            }
        }
        Err(ActorError::user("the supervisor fails its initialization"))
    }

    async fn on_child_stopped(
        &mut self,
        _event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.counts.child_stopped.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.counts.stopped.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Spawns an `InitFails` supervisor and returns the spawn's error.
async fn failing_initialization(
    sys: &Arc<ActorSystem>,
    mode: InitMode,
) -> (
    tokio_actors::SpawnError,
    (String, String),
    (Arc<Probe>, Arc<Probe>),
    Arc<InitCounts>,
) {
    let counts = Arc::new(InitCounts::default());
    let names = (uname("c1"), uname("c2"));
    let probes = (Arc::new(Probe::default()), Arc::new(Probe::default()));
    let err = InitFails {
        mode,
        probes: probes.clone(),
        names: names.clone(),
        counts: counts.clone(),
    }
    .spawn()
    .named(uname("init-fails"))
    .on_system(sys)
    .with_supervision(plain())
    .await;
    let err = match err {
        Ok(_) => panic!("the initialization fails"),
        Err(err) => err,
    };
    (err, names, probes, counts)
}

#[test]
fn a_supervisor_whose_initialization_fails_receives_no_child_report_and_no_on_stopped() {
    common::run(0, async {
        // A refused `spawn_child` returned with `?` fails the initialization
        // with the registration error.
        let sys = ActorSystem::create(uname("init-name-taken")).unwrap();
        let taken = uname("taken");
        let holder = Probed {
            probe: Arc::new(Probe::default()),
            vetoes: 0,
        }
        .spawn()
        .named(taken.clone())
        .on_system(&sys)
        .await
        .expect("a live root holds the name");
        let (err, _, _, counts) =
            failing_initialization(&sys, InitMode::NameTaken(taken.clone())).await;
        match err {
            tokio_actors::SpawnError::Init(inner) => assert!(
                matches!(
                    *inner,
                    ActorError::Spawn(tokio_actors::SpawnError::NameTaken { .. })
                ),
                "{inner:?}"
            ),
            other => panic!("expected an initialization failure, got {other:?}"),
        }
        assert_eq!(counts.child_stopped.load(Ordering::SeqCst), 0);
        assert_eq!(counts.stopped.load(Ordering::SeqCst), 0);
        drop(holder);

        // A child that failed during the initialization is not reported, and
        // the supervisor never receives `on_stopped`.
        let sys = ActorSystem::create(uname("init-child-failed")).unwrap();
        let (err, _, probes, counts) =
            failing_initialization(&sys, InitMode::ChildFailedDuring).await;
        assert!(matches!(err, tokio_actors::SpawnError::Init(_)), "{err:?}");
        assert_eq!(counts.child_stopped.load(Ordering::SeqCst), 0);
        assert_eq!(counts.stopped.load(Ordering::SeqCst), 0);
        assert!(probes.0.ended.load(Ordering::SeqCst) && probes.1.ended.load(Ordering::SeqCst));

        // A child whose own initialization fails: `spawn_child` never awaits it.
        let sys = ActorSystem::create(uname("init-child-init")).unwrap();
        let (err, _, probes, counts) = failing_initialization(&sys, InitMode::ChildInitFails).await;
        assert!(matches!(err, tokio_actors::SpawnError::Init(_)), "{err:?}");
        assert_eq!(counts.child_stopped.load(Ordering::SeqCst), 0);
        assert_eq!(counts.stopped.load(Ordering::SeqCst), 0);
        assert!(probes.0.ended.load(Ordering::SeqCst));
    });
}

#[cfg(feature = "tracing")]
mod warnings {
    use std::fmt::Write as _;
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::{span, Event, Level, Metadata, Subscriber};

    /// The one global subscriber of this test binary: it collects every WARN
    /// event as one line of `name=value` pairs, and hands every DEBUG event
    /// to [`absorption_log`](super::absorption_log).
    #[derive(Clone, Default)]
    pub struct Warnings {
        pub lines: Arc<Mutex<Vec<String>>>,
    }

    /// Installs the one subscriber of this test binary on first use and
    /// returns its lines. Tests find their own lines by the unique names of
    /// their children, so concurrent tests do not disturb each other.
    pub fn install() -> Arc<Mutex<Vec<String>>> {
        static GLOBAL: std::sync::OnceLock<Warnings> = std::sync::OnceLock::new();
        GLOBAL
            .get_or_init(|| {
                let warnings = Warnings::default();
                tracing::subscriber::set_global_default(warnings.clone())
                    .expect("no other global subscriber is installed in this test binary");
                warnings
            })
            .lines
            .clone()
    }

    /// The captured lines that carry `child` as their `child` field. The
    /// match is on the whole field token, so a name never matches another
    /// name it is a prefix of.
    pub fn naming(lines: &[String], child: &str) -> Vec<String> {
        let token = format!("child={child} ");
        lines
            .iter()
            .filter(|line| line.contains(&token))
            .cloned()
            .collect()
    }

    struct Text(String);

    impl Visit for Text {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let _ = write!(self.0, "{}={:?} ", field.name(), value);
        }
    }

    impl Subscriber for Warnings {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() == Level::WARN || *metadata.level() == Level::DEBUG
        }

        fn new_span(&self, _attrs: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }

        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

        fn event(&self, event: &Event<'_>) {
            if *event.metadata().level() == Level::DEBUG {
                super::absorption_log::record(event);
                return;
            }
            let mut text = Text(String::new());
            event.record(&mut text);
            self.lines.lock().unwrap().push(text.0);
        }

        fn enter(&self, _span: &span::Id) {}

        fn exit(&self, _span: &span::Id) {}
    }
}

#[cfg(feature = "tracing")]
#[test]
fn a_child_ending_with_a_failure_during_the_teardown_logs_one_warning() {
    let lines = warnings::install();
    {
        common::run_current_thread_only(async {
            let sys = ActorSystem::create(uname("teardown-warning")).unwrap();
            let rig = supervisor_on(&sys, plain()).await;
            let kids = spawn_children(&sys, &rig, &["bad", "good"]).await;
            kids[0].1.pre_stop_panics.store(true, Ordering::SeqCst);
            stop_supervisor(&rig, BOUND).await;
            assert!(
                deliveries(&rig).iter().all(|line| line == "on_stopped"),
                "no callback for either child"
            );
            let lines = lines.lock().unwrap().clone();
            let naming_bad = warnings::naming(&lines, &kids[0].0);
            assert_eq!(naming_bad.len(), 1, "{lines:?}");
            assert!(naming_bad[0].contains("incarnation"), "{lines:?}");
            assert!(warnings::naming(&lines, &kids[1].0).is_empty(), "{lines:?}");
        });
    }
}

#[cfg(feature = "tracing")]
#[test]
fn a_supervisor_whose_initialization_fails_logs_a_child_that_failed_during_it_and_delivers_nothing()
{
    for mode in [InitMode::ChildFailedDuring, InitMode::ChildInitFails] {
        let lines = warnings::install();
        let named = matches!(mode, InitMode::ChildFailedDuring);
        {
            common::run_current_thread_only(async {
                let sys = ActorSystem::create(uname("init-warning")).unwrap();
                let (err, names, _probes, counts) = failing_initialization(&sys, mode).await;
                assert!(matches!(err, tokio_actors::SpawnError::Init(_)), "{err:?}");
                assert_eq!(counts.child_stopped.load(Ordering::SeqCst), 0);
                assert_eq!(counts.stopped.load(Ordering::SeqCst), 0);
                let lines = lines.lock().unwrap().clone();
                let naming_first = warnings::naming(&lines, &names.0);
                assert_eq!(naming_first.len(), 1, "{lines:?}");
                assert!(
                    naming_first[0].contains("incarnation") && naming_first[0].contains("failure")
                );
                if named {
                    assert!(warnings::naming(&lines, &names.1).is_empty(), "{lines:?}");
                }
            });
        }
    }
}

#[cfg(feature = "tracing")]
#[test]
fn an_attempt_whose_factory_panicked_during_the_teardown_logs_one_warning() {
    // The supervisor yields in `on_stopped`, so the restart attempt runs after
    // the final collection, finds no order and panics in its factory. The
    // teardown keeps waiting for the attempt's own report, and that report is
    // the one warning for the child. The sequence needs one thread for the
    // supervisor and the attempt.
    let lines = warnings::install();
    common::run_current_thread_only(async {
        let sys = ActorSystem::create(uname("factory-panics-warning")).unwrap();
        let rig = supervisor_named(&sys, &uname("sup"), plain(), true, true).await;
        let probe = Arc::new(Probe::default());
        let name = uname("w");
        rig.spawn_child(
            &name,
            &probe,
            Shutdown::Timeout(Duration::from_secs(1)),
            RestartType::Permanent,
            0,
        )
        .await;
        let child = child_in(&sys, &name).await;
        probe.factory_panics.store(true, Ordering::SeqCst);
        crash(&child).await;
        within(
            BOUND,
            "the supervisor to stop itself",
            rig.sup.wait_stopped(),
        )
        .await;
        sleep(QUIET).await;
        let lines = lines.lock().unwrap().clone();
        let naming_child = warnings::naming(&lines, &name);
        assert_eq!(
            naming_child.len(),
            1,
            "one warning for the attempt's end: {lines:?}"
        );
    });
}

/// Root `P`, its child supervisor `S` under `window`, and `S`'s children `W`
/// and `R`; `R`'s second initialization never completes and is in flight
/// when `P` stops. Returns how many times `W` ran `on_stopped`.
async fn pairing_rule(window: Duration) -> usize {
    let sys = ActorSystem::create(uname("pairing")).unwrap();
    let root = supervisor_on(&sys, plain()).await;
    let inner_name = uname("inner");
    root.ok(Cmd::SpawnSupervisorChild {
        name: inner_name.clone(),
        shutdown: Shutdown::Timeout(window),
        restart: RestartType::Permanent,
        config: plain(),
        built: Arc::new(AtomicUsize::new(0)),
        events: Arc::new(Mutex::new(Vec::new())),
        knobs: Arc::new(Knobs::default()),
    })
    .await;
    let inner_handle = sys
        .get::<Sup>(&inner_name)
        .expect("the child supervisor is registered");
    let inner = Rig {
        sup: inner_handle,
        events: Arc::new(Mutex::new(Vec::new())),
        knobs: Arc::new(Knobs::default()),
    };
    let (pw, pr) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
    let (w, r) = (uname("w"), uname("r"));
    // `W` starts first and `R` after it, so the teardown reaches `R` first;
    // `R` is stopped within 800 ms.
    inner
        .spawn_child(
            &w,
            &pw,
            Shutdown::Timeout(Duration::from_millis(300)),
            RestartType::Permanent,
            0,
        )
        .await;
    inner
        .spawn_child(
            &r,
            &pr,
            Shutdown::Timeout(Duration::from_millis(800)),
            RestartType::Permanent,
            0,
        )
        .await;
    let child_r = child_in(&sys, &r).await;
    // R's first incarnation has read its knobs: what is set below reaches
    // only the restarted one, which waits in `on_started` for a child that is
    // never spawned, so only its stop ends it.
    wait_until(BOUND, "R's first incarnation to finish starting", || {
        pr.started.load(Ordering::SeqCst) == 1
    })
    .await;
    *pr.wait_alive.lock().unwrap() = Some(uname("never-spawned"));
    crash(&child_r).await;
    wait_until(BOUND, "R's restart to be inside its init", || {
        pr.entered.load(Ordering::SeqCst) == 2
    })
    .await;
    stop_supervisor(&root, Duration::from_secs(10)).await;
    pw.stopped_runs.load(Ordering::SeqCst)
}

#[test]
fn teardown_honors_the_pairing_rule_for_a_child_mid_restart() {
    // S's window covers its children's budgets: W is stopped by its supervisor
    // and runs `on_stopped`.
    common::run(0, async {
        assert_eq!(pairing_rule(Duration::from_secs(5)).await, 1);
    });
    // S at 500 ms: S is escalated to Kill before its teardown reaches W, and W
    // is taken by Kill, the documented consequence of pairing the windows.
    common::run(0, async {
        assert_eq!(pairing_rule(Duration::from_millis(500)).await, 0);
    });
}

#[test]
fn one_for_all_mid_restart_when_shutdown_fires_never_respawns_after_shutdown_returns() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("shutdown-mid-restart")).unwrap();
        let rig = supervisor_on(&sys, one_for_all()).await;
        let (pa, pb) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
        let (a, b) = (uname("a"), uname("b"));
        for (name, probe) in [(&a, &pa), (&b, &pb)] {
            rig.spawn_child(
                name,
                probe,
                Shutdown::Timeout(Duration::from_millis(300)),
                RestartType::Permanent,
                0,
            )
            .await;
        }
        let child_a = child_in(&sys, &a).await;
        let _ = child_in(&sys, &b).await;
        // The group restart's chain restarts `a` first, slowly.
        pa.init_sleep_ms.store(500, Ordering::SeqCst);
        crash(&child_a).await;
        wait_until(BOUND, "the chain to start a", || {
            pa.built.load(Ordering::SeqCst) == 2
        })
        .await;
        sleep(Duration::from_millis(50)).await;
        let report = within(BOUND, "the system shutdown", sys.shutdown()).await;
        assert!(report.swept.is_empty(), "{:?}", report.swept);
        let built = (
            pa.built.load(Ordering::SeqCst),
            pb.built.load(Ordering::SeqCst),
        );
        sleep(Duration::from_millis(800)).await;
        assert_eq!(
            built,
            (
                pa.built.load(Ordering::SeqCst),
                pb.built.load(Ordering::SeqCst)
            ),
            "nothing is respawned after shutdown returned"
        );
        assert!(sys.get::<Probed>(&a).is_none() && sys.get::<Probed>(&b).is_none());
    });
}

#[test]
fn parent_stop_escalates_a_restart_adopted_during_the_stop_through_its_runner() {
    // As the test with a busy supervisor, but the child vetoes its stop: only
    // the order the adopted incarnation's runner honors ends it, at the end of
    // the child's own window.
    common::run(0, async {
        let sys = ActorSystem::create(uname("adopted-veto")).unwrap();
        let rig = supervisor_on(&sys, plain()).await;
        let probe = Arc::new(Probe::default());
        let name = uname("w");
        rig.spawn_child(
            &name,
            &probe,
            Shutdown::Timeout(Duration::from_millis(600)),
            RestartType::Permanent,
            always_vetoes(),
        )
        .await;
        let child = child_in(&sys, &name).await;
        probe.init_sleep_ms.store(50, Ordering::SeqCst);
        crash(&child).await;
        wait_until(BOUND, "the restart attempt to be built", || {
            probe.built.load(Ordering::SeqCst) == 2
        })
        .await;
        // The supervisor's handler says it runs and holds until the stop is
        // raised: the restart acks while the handler runs, and is adopted
        // with the stop already pending.
        let (entered, release) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let busy = {
            let sup = rig.sup.clone();
            let op = Op::Hold {
                entered: entered.clone(),
                release: release.clone(),
            };
            tokio::spawn(async move { sup.send(Cmd::Script(vec![op])).await })
        };
        wait_until(BOUND, "the supervisor's handler to run", || {
            entered.load(Ordering::SeqCst)
        })
        .await;
        // The incarnation publishes Running and acks in one step.
        wait_until(BOUND, "the restart to ack", || {
            probe.started.load(Ordering::SeqCst) == 2
                && sys.actor_status(child.id()) == Some(ActorStatus::Running)
        })
        .await;
        let stopping_at = Instant::now();
        within(
            BOUND,
            "the stop request",
            rig.sup.stop(StopReason::Graceful),
        )
        .await
        .expect("the stop request is accepted");
        release.store(true, Ordering::SeqCst);
        within(
            Duration::from_secs(5),
            "the supervisor to publish Stopped",
            rig.sup.wait_stopped(),
        )
        .await;
        let took = stopping_at.elapsed();
        let _ = busy.await;
        assert!(
            probe.ended.load(Ordering::SeqCst),
            "the supervisor publishes Stopped only after the adopted incarnation ended"
        );
        assert!(
            stopping_at.elapsed() >= Duration::from_millis(500)
                && took >= Duration::from_millis(100),
            "the child's window counts from the order: {:?}",
            stopping_at.elapsed()
        );
    });
}

// ---------------------------------------------------------------------------
// A death that owes its restart decision is not decided by a manual stop
// ---------------------------------------------------------------------------

/// RestForOne `[x, y, z]`: `y` crashes and `z`, which vetoes under a long
/// window, holds the group's stop phase open; `x`, which is not part of the
/// group, is then killed, so its death owes its restart decision until the
/// group completes. Returns once `children()` reads `x` as owing it.
async fn x_owes_its_decision_while_a_group_pends() -> (Rig, [String; 3], [Arc<Probe>; 3]) {
    // The budget is one restart: the group restart uses it, so a charge for
    // the death that owes its decision would end the supervisor.
    let rig =
        supervisor(SupervisionConfig::rest_for_one().max_restarts(1, Duration::from_secs(60)))
            .await;
    let names = [uname("x"), uname("y"), uname("z")];
    let (px, py, pz) = probes();
    for (name, probe, window, vetoes) in [
        (&names[0], &px, Duration::from_secs(2), 0),
        (&names[1], &py, Duration::from_secs(2), 0),
        (&names[2], &pz, Duration::from_millis(1500), always_vetoes()),
    ] {
        rig.spawn_child(
            name,
            probe,
            Shutdown::Timeout(window),
            RestartType::Permanent,
            vetoes,
        )
        .await;
    }
    crash(&child_handle(&names[1]).await).await;
    wait_until(BOUND, "the group to ask z to stop", || {
        pz.pre_stops.load(Ordering::SeqCst) == 1
    })
    .await;
    let _ = child_handle(&names[0])
        .await
        .stop_within(StopReason::Kill, Duration::from_millis(200))
        .await;
    let deadline = Instant::now() + BOUND;
    loop {
        let lines = rig.script(vec![Op::Read(names[0].clone())]).await;
        if lines[0] == "alive=false pending=true" {
            break;
        }
        assert!(Instant::now() < deadline, "x never read pending: {lines:?}");
        sleep(Duration::from_millis(5)).await;
    }
    (rig, names, [px, py, pz])
}

#[test]
fn terminate_child_of_a_death_awaiting_its_decision_in_a_group_dismisses_it_without_a_charge() {
    common::run(0, async {
        let (rig, [x, _y, _z], [px, py, pz]) = x_owes_its_decision_while_a_group_pends().await;
        let lines = rig
            .script(vec![
                Op::Terminate(x.clone()),
                Op::Read(x.clone()),
                Op::Stop(x.clone()),
            ])
            .await;
        // The terminate dismisses the owed decision: it answers `Ok`, the
        // child reads down with nothing owed, and the bounce that follows
        // finds a child that is down.
        assert_eq!(lines[0], "Ok(())", "{lines:?}");
        assert_eq!(lines[1], "alive=false pending=false", "{lines:?}");
        assert_eq!(lines[2], "Ok(())", "{lines:?}");
        assert_eq!(pz.built.load(Ordering::SeqCst), 1, "the group still pends");
        assert_eq!(px.built.load(Ordering::SeqCst), 1);
        // The group completes, and the death is then delivered exactly once,
        // as a removal with its reason, and the child is not restarted.
        wait_until(BOUND, "the death to be delivered after the group", || {
            !rig.events_for(&x).is_empty()
        })
        .await;
        sleep(QUIET).await;
        let events = rig.events_for(&x);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].action, SupervisionAction::Removed, "{events:?}");
        assert!(matches!(events[0].reason, StopReason::Kill), "{events:?}");
        assert_eq!(px.built.load(Ordering::SeqCst), 1, "x is not restarted");
        // The dismissed death was not charged: the budget of one restart
        // went to the group, and the supervisor is still running.
        wait_until(BOUND, "the group to restart y and z", || {
            py.built.load(Ordering::SeqCst) == 2 && pz.built.load(Ordering::SeqCst) == 2
        })
        .await;
        assert!(rig.sup.is_alive(), "the supervisor gave up");
        assert_ne!(rig.sup.status(), tokio_actors::ActorStatus::Stopped);
    });
}

#[test]
fn terminate_child_after_a_bounce_met_a_crash_dismisses_it_and_the_crash_is_reported_once() {
    common::run(0, async {
        // A budget of one restart: the dismissed crash must not use it.
        let rig =
            supervisor(SupervisionConfig::one_for_one().max_restarts(1, Duration::from_secs(60)))
                .await;
        let name = uname("owed");
        let probe = Arc::new(Probe::default());
        rig.spawn_child(
            &name,
            &probe,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        wait_until(BOUND, "the child to finish starting", || {
            probe.started.load(Ordering::SeqCst) == 1
        })
        .await;
        // stop_child meets the crash; terminate_child then finds a death
        // that owes its decision and dismisses it.
        let lines = rig
            .script(vec![
                Op::SlowFail(name.clone()),
                Op::UntilInTurn(probe.clone()),
                Op::Stop(name.clone()),
                Op::Terminate(name.clone()),
            ])
            .await;
        assert_eq!(lines[2], "Ok(())", "{lines:?}");
        assert_eq!(lines[3], "Ok(())", "{lines:?}");
        // The crash is delivered once, as a removal with its failure, and the
        // child is not restarted.
        wait_until(BOUND, "the crash to be delivered", || {
            !rig.events_for(&name).is_empty()
        })
        .await;
        sleep(QUIET).await;
        assert_eq!(probe.built.load(Ordering::SeqCst), 1);
        let events = rig.events_for(&name);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].action, SupervisionAction::Removed);
        assert!(matches!(events[0].reason, StopReason::Failure(_)));
        // The dismissed crash was not charged: the one restart the budget
        // allows is still there for a later crash of the revived child.
        let lines = rig
            .script(vec![
                Op::Restart(name.clone()),
                Op::UntilAlive(name.clone()),
                Op::CrashAndWait(name.clone()),
            ])
            .await;
        assert_eq!(lines[0], "Ok(())", "{lines:?}");
        wait_until(
            BOUND,
            "the crash of the revived child to be restarted",
            || probe.built.load(Ordering::SeqCst) == 3,
        )
        .await;
        assert!(rig.sup.is_alive(), "the supervisor gave up");
    });
}

#[test]
fn a_chain_front_whose_manual_stop_was_in_progress_does_not_hold_the_group_for_ever() {
    common::run(0, async {
        // OneForAll [a, b]: a terminate of b is in progress (b vetoes for
        // its window of 300 ms) when a crashes. The group does not stop b,
        // and its chain, after it restarted a, holds at b until b's stop
        // completes.
        let rig = supervisor(one_for_all()).await;
        let (a, b) = (uname("a"), uname("b"));
        let (pa, pb) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
        rig.spawn_child(
            &a,
            &pa,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        rig.spawn_child(
            &b,
            &pb,
            Shutdown::Timeout(Duration::from_millis(300)),
            RestartType::Permanent,
            always_vetoes(),
        )
        .await;
        wait_until(BOUND, "the children to finish starting", || {
            pa.started.load(Ordering::SeqCst) == 1 && pb.started.load(Ordering::SeqCst) == 1
        })
        .await;
        let lines = rig.script(vec![Op::TerminateAbandon(b.clone())]).await;
        assert_eq!(lines, ["abandoned"]);
        wait_until(BOUND, "b to be asked to stop", || {
            pb.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "the chain to restart a", || {
            pa.built.load(Ordering::SeqCst) == 2
        })
        .await;
        // b's stop completes and its end is reported once; the chain, released
        // from b, completes the group: nothing reads restart pending.
        wait_until(BOUND, "b's end to be reported", || {
            !rig.events_for(&b).is_empty()
        })
        .await;
        let deadline = Instant::now() + BOUND;
        loop {
            let lines = rig
                .script(vec![Op::Read(a.clone()), Op::Read(b.clone())])
                .await;
            if lines.iter().all(|line| line.ends_with("pending=false")) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the group never completed: {lines:?}"
            );
            sleep(Duration::from_millis(5)).await;
        }
        sleep(QUIET).await;
        wait_for_alive(&rig, &a).await;
        // A terminate committed before the group was evaluated rejoins the
        // group: b's end is reported as the restart it is part of, and the
        // chain starts b in its slot once the incarnation being stopped ended.
        wait_for_alive(&rig, &b).await;
        let events = rig.events_for(&b);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(
            events[0].action,
            SupervisionAction::RestartInitiated,
            "{events:?}"
        );
        assert_eq!(pb.built.load(Ordering::SeqCst), 2);
        assert!(
            pb.build_order(1) > pb.drop_order(0),
            "no new incarnation of b begins beside the one being stopped"
        );
        assert!(rig.sup.is_alive());
    });
}

// ---------------------------------------------------------------------------
// The manual calls during a pending group restart, and the outcome of
// `stop_child`
// ---------------------------------------------------------------------------

/// The name and the probe of a `Permanent` member that stops at once.
async fn member(rig: &Rig, base: &str, window: Duration, vetoes: usize) -> (String, Arc<Probe>) {
    let name = uname(base);
    let probe = Arc::new(Probe::default());
    rig.spawn_child(
        &name,
        &probe,
        Shutdown::Timeout(window),
        RestartType::Permanent,
        vetoes,
    )
    .await;
    (name, probe)
}

#[test]
fn terminate_child_upgrades_an_abandoned_bounce() {
    common::run(0, async {
        let rig = supervisor(plain()).await;
        let (name, probe) = member(&rig, "upgrade", Duration::from_millis(300), 1).await;
        wait_until(BOUND, "the child to start", || {
            probe.started.load(Ordering::SeqCst) == 1
        })
        .await;
        // The bounce is abandoned at its commit and the child vetoes it; the
        // terminate that follows turns the stop in progress into a terminate.
        let lines = rig
            .script(vec![
                Op::StopAbandon(name.clone()),
                Op::Terminate(name.clone()),
            ])
            .await;
        assert_eq!(lines[1], "Ok(())", "{lines:?}");
        sleep(QUIET).await;
        assert_eq!(
            probe.built.load(Ordering::SeqCst),
            1,
            "the child stays down"
        );
        removed_once(&rig, &name);
    });
}

#[test]
fn terminate_child_removes_a_chain_queued_member() {
    common::run(0, async {
        let rig = supervisor(one_for_all()).await;
        let (a, pa) = member(&rig, "a", Duration::from_secs(2), 0).await;
        let (b, pb) = member(&rig, "b", Duration::from_secs(2), 0).await;
        let (c, pc) = member(&rig, "c", Duration::from_secs(2), 0).await;
        wait_until(BOUND, "the children to start", || {
            [&pa, &pb, &pc]
                .iter()
                .all(|p| p.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        // The chain holds at a, so b and c are down in the restart set.
        pa.held.store(true, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "the chain to begin a's restart", || {
            pa.built.load(Ordering::SeqCst) == 2
        })
        .await;
        let lines = rig
            .script(vec![Op::Terminate(b.clone()), Op::Read(b.clone())])
            .await;
        assert_eq!(lines[0], "Ok(())", "{lines:?}");
        assert_eq!(lines[1], "alive=false pending=false", "{lines:?}");
        pa.held.store(false, Ordering::SeqCst);
        wait_until(BOUND, "the chain to restart c", || {
            pc.built.load(Ordering::SeqCst) == 2
        })
        .await;
        sleep(QUIET).await;
        assert_eq!(pb.built.load(Ordering::SeqCst), 1, "b stays down");
        assert!(!is_alive(&rig, &b).await);
        // b's end was reported once, when the group stopped it, as the
        // restart the group then made no more.
        let events = rig.events_for(&b);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(
            events[0].action,
            SupervisionAction::RestartInitiated,
            "{events:?}"
        );
        assert!(!rig.events_for(&c).is_empty());
    });
}

#[test]
fn terminate_child_on_a_retrying_member_cancels_the_retry() {
    common::run(0, async {
        let rig = supervisor(one_for_all()).await;
        let (a, pa) = member(&rig, "a", Duration::from_millis(400), 0).await;
        let (b, pb) = member(&rig, "b", Duration::from_secs(2), 0).await;
        wait_until(BOUND, "the children to start", || {
            pa.started.load(Ordering::SeqCst) == 1 && pb.started.load(Ordering::SeqCst) == 1
        })
        .await;
        // The chain restarts a, b's start fails once and the group is
        // planned again; a's second incarnation vetoes the retry's request (the probe counts every call),
        // so the retry's stop phase lasts for its 400 ms window.
        pb.fail_next.store(1, Ordering::SeqCst);
        pa.veto_script.lock().unwrap().push_back(2);
        crash(&child_handle(&b).await).await;
        wait_until(BOUND, "the retry to ask a to stop", || {
            pa.pre_stops.load(Ordering::SeqCst) == 2
        })
        .await;
        let lines = rig
            .script(vec![Op::Terminate(b.clone()), Op::Read(b.clone())])
            .await;
        assert_eq!(lines[0], "Ok(())", "{lines:?}");
        wait_until(BOUND, "the retry to restart a", || {
            pa.built.load(Ordering::SeqCst) == 3
        })
        .await;
        sleep(QUIET).await;
        let builds = pb.built.load(Ordering::SeqCst);
        assert!(builds <= 2, "b is not started again by the retry: {builds}");
        assert!(!is_alive(&rig, &b).await, "b stays down");
        assert!(is_alive(&rig, &a).await);
    });
}

/// Polls the supervisor until `children()` reads the child restart-pending.
async fn until_pending(rig: &Rig, name: &str) {
    let deadline = Instant::now() + BOUND;
    loop {
        let lines = rig.script(vec![Op::Read(name.to_string())]).await;
        if lines[0].ends_with("pending=true") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{name} never read restart-pending: {lines:?}"
        );
        sleep(Duration::from_millis(5)).await;
    }
}

/// Polls the supervisor until `children()` reads none of the children
/// restart-pending: no group restart pends over them.
async fn until_idle(rig: &Rig, names: &[&str]) {
    let deadline = Instant::now() + BOUND;
    loop {
        let lines = rig
            .script(names.iter().map(|n| Op::Read((*n).to_string())).collect())
            .await;
        if lines.iter().all(|line| line.ends_with("pending=false")) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the group never completed: {lines:?}"
        );
        sleep(Duration::from_millis(5)).await;
    }
}

fn restarting(line: &str) -> bool {
    line.contains("is restarting")
}

/// OneForAll `[a, x]`: a bounce of `a` is abandoned at its commit and held in
/// its `pre_stop`, then `x` crashes, so the group evaluates and joins `a`'s
/// stop.
async fn group_with_a_bounce_in_progress() -> (Rig, [String; 2], [Arc<Probe>; 2]) {
    let rig = supervisor(one_for_all()).await;
    let (a, pa) = member(&rig, "a", Duration::from_secs(5), 0).await;
    let (x, px) = member(&rig, "x", Duration::from_secs(2), 0).await;
    wait_until(BOUND, "the children to start", || {
        pa.started.load(Ordering::SeqCst) == 1 && px.started.load(Ordering::SeqCst) == 1
    })
    .await;
    pa.pre_stop_gate.store(true, Ordering::SeqCst);
    let lines = rig.script(vec![Op::StopAbandon(a.clone())]).await;
    assert_eq!(lines, ["abandoned"]);
    wait_until(BOUND, "a to be asked to stop", || {
        pa.pre_stops.load(Ordering::SeqCst) == 1
    })
    .await;
    crash(&child_handle(&x).await).await;
    until_pending(&rig, &x).await;
    (rig, [a, x], [pa, px])
}

#[test]
fn stop_child_of_a_pending_group_member_is_absorbed_while_its_manual_stop_is_in_progress() {
    common::run(0, async {
        let (rig, [a, x], [pa, px]) = group_with_a_bounce_in_progress().await;
        // The member is the group's, joined with its stop in progress: the
        // second bounce is absorbed and changes nothing; a terminate that
        // follows takes the override path.
        let lines = rig
            .script(vec![
                Op::Read(a.clone()),
                Op::StopOutcome(a.clone()),
                Op::TerminateThenOpen(a.clone(), pa.clone()),
            ])
            .await;
        assert_eq!(lines[0], "alive=true pending=false", "{lines:?}");
        assert_eq!(lines[1], "Ok(Absorbed)", "{lines:?}");
        assert_eq!(lines[2], "Ok(())", "the terminate waits for the stop");
        wait_until(BOUND, "the group to restart x", || {
            px.built.load(Ordering::SeqCst) == 2
        })
        .await;
        until_idle(&rig, &[&a, &x]).await;
        // Neither call asked a second time: one ladder, one `pre_stop`.
        assert_eq!(pa.pre_stops.load(Ordering::SeqCst), 1, "no second ladder");
        sleep(QUIET).await;
        assert_eq!(pa.built.load(Ordering::SeqCst), 1, "a stays down");
        removed_once(&rig, &a);
        a_terminate_during_a_chains_restart_attempt_of_a_member_answers_child_restarting().await;
    });
}

/// The second run of the test above: the group completes, and a terminate of
/// the member sent while the chain's restart attempt of it is in flight is
/// refused.
async fn a_terminate_during_a_chains_restart_attempt_of_a_member_answers_child_restarting() {
    let (rig, [a, x], [pa, px]) = group_with_a_bounce_in_progress().await;
    // The group completes: a is restarted by the chain, its restart
    // attempt held in its initialization.
    pa.held.store(true, Ordering::SeqCst);
    pa.pre_stop_gate.store(false, Ordering::SeqCst);
    wait_until(BOUND, "the chain to begin a's restart", || {
        pa.built.load(Ordering::SeqCst) == 2
    })
    .await;
    let lines = rig
        .script(vec![Op::Read(a.clone()), Op::Terminate(a.clone())])
        .await;
    assert_eq!(lines[0], "alive=false pending=true", "{lines:?}");
    assert!(restarting(&lines[1]), "{lines:?}");
    pa.held.store(false, Ordering::SeqCst);
    wait_until(BOUND, "x to be restarted", || {
        px.built.load(Ordering::SeqCst) == 2
    })
    .await;
    until_idle(&rig, &[&a, &x]).await;
    wait_for_alive(&rig, &a).await;
    assert_eq!(pa.built.load(Ordering::SeqCst), 2, "a runs once");
}

#[test]
fn stop_child_of_a_member_released_by_terminate_child_answers_child_restarting_until_the_group_completes(
) {
    common::run(0, async {
        let rig = supervisor(one_for_all()).await;
        let (a, pa) = member(&rig, "a", Duration::from_secs(2), 0).await;
        let (b, pb) = member(&rig, "b", Duration::from_millis(300), always_vetoes()).await;
        let (d, pd) = member(&rig, "d", Duration::from_secs(2), 0).await;
        wait_until(BOUND, "the children to start", || {
            [&pa, &pb, &pd]
                .iter()
                .all(|p| p.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "the group to ask b to stop", || {
            pb.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        // One handler: the override, then the bounce of the released member.
        let lines = rig
            .script(vec![Op::Terminate(b.clone()), Op::Stop(b.clone())])
            .await;
        assert_eq!(lines[0], "Ok(())", "{lines:?}");
        assert!(restarting(&lines[1]), "{lines:?}");
        until_idle(&rig, &[&a, &b, &d]).await;
        wait_until(BOUND, "the group to restart a and d", || {
            pa.built.load(Ordering::SeqCst) == 2 && pd.built.load(Ordering::SeqCst) == 2
        })
        .await;
        let lines = rig.script(vec![Op::StopOutcome(b.clone())]).await;
        assert_eq!(lines[0], "Ok(Stopped)", "{lines:?}");
        sleep(QUIET).await;
        assert_eq!(pb.built.load(Ordering::SeqCst), 1, "b stays down");
        removed_once(&rig, &b);
    });
}

#[test]
fn stop_child_of_a_queued_member_is_absorbed_and_one_the_chain_restarted_answers_child_restarting()
{
    common::run(0, async {
        // A budget of one: the group's restart is the one charge, so the
        // bounce at the end must be free.
        let rig =
            supervisor(SupervisionConfig::one_for_all().max_restarts(1, Duration::from_secs(60)))
                .await;
        let (a, pa) = member(&rig, "a", Duration::from_secs(2), 0).await;
        let (b, pb) = member(&rig, "b", Duration::from_secs(2), 0).await;
        let (c, pc) = member(&rig, "c", Duration::from_secs(5), 0).await;
        wait_until(BOUND, "the children to start", || {
            [&pa, &pb, &pc]
                .iter()
                .all(|p| p.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        // The group's stop phase waits for c; c's restart is then held.
        pc.pre_stop_gate.store(true, Ordering::SeqCst);
        pc.held.store(true, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "the group to ask c to stop", || {
            pc.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        let lines = rig.script(vec![Op::StopOutcome(b.clone())]).await;
        assert_eq!(lines[0], "Ok(Absorbed)", "{lines:?}");
        pc.pre_stop_gate.store(false, Ordering::SeqCst);
        wait_until(BOUND, "the chain to begin c's restart", || {
            pc.built.load(Ordering::SeqCst) == 2
        })
        .await;
        // a and b were restarted by the chain and c's attempt is in flight.
        let lines = rig
            .script(vec![
                Op::StopOutcome(a.clone()),
                Op::Restart(a.clone()),
                Op::StopOutcome(c.clone()),
            ])
            .await;
        for line in &lines {
            assert!(restarting(line), "{lines:?}");
        }
        pc.held.store(false, Ordering::SeqCst);
        until_idle(&rig, &[&a, &b, &c]).await;
        wait_for_alive(&rig, &c).await;
        for p in [&pa, &pb, &pc] {
            assert_eq!(p.built.load(Ordering::SeqCst), 2);
        }
        // The group completed: the bounce is the ordinary one, and free.
        let lines = rig.script(vec![Op::StopOutcome(a.clone())]).await;
        assert_eq!(lines[0], "Ok(Stopped)", "{lines:?}");
        wait_until(BOUND, "a to be bounced", || {
            pa.built.load(Ordering::SeqCst) == 3
        })
        .await;
        sleep(QUIET).await;
        assert_eq!(pa.built.load(Ordering::SeqCst), 3, "bounced once");
        assert!(rig.sup.is_alive(), "the bounce charged nothing");
    });
}

#[test]
fn restart_child_answers_child_restarting_for_a_key_the_chain_restarted_until_the_group_completes()
{
    common::run(0, async {
        let rig = supervisor(one_for_all()).await;
        let (a, pa) = member(&rig, "a", Duration::from_secs(2), 0).await;
        let (b, pb) = member(&rig, "b", Duration::from_secs(2), 0).await;
        let (c, pc) = member(&rig, "c", Duration::from_secs(2), 0).await;
        wait_until(BOUND, "the children to start", || {
            [&pa, &pb, &pc]
                .iter()
                .all(|p| p.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        // c's restart is held, so the chain has restarted a and b.
        pc.held.store(true, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "the chain to begin c's restart", || {
            pc.built.load(Ordering::SeqCst) == 2
        })
        .await;
        let lines = rig
            .script(vec![Op::UntilAlive(a.clone()), Op::Restart(a.clone())])
            .await;
        assert_eq!(lines[0], "alive", "{lines:?}");
        assert!(restarting(&lines[1]), "{lines:?}");
        pc.held.store(false, Ordering::SeqCst);
        until_idle(&rig, &[&a, &b, &c]).await;
        let lines = rig.script(vec![Op::Restart(a.clone())]).await;
        assert!(lines[0].contains("is running"), "{lines:?}");
        assert_eq!(pa.built.load(Ordering::SeqCst), 2);
        assert_eq!(pb.built.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn stop_child_during_a_retry_of_a_member_the_first_chain_restarted_is_absorbed() {
    common::run(0, async {
        let rig = supervisor(one_for_all()).await;
        let (a, pa) = member(&rig, "a", Duration::from_secs(5), 0).await;
        let (b, pb) = member(&rig, "b", Duration::from_secs(2), 0).await;
        wait_until(BOUND, "the children to start", || {
            pa.started.load(Ordering::SeqCst) == 1 && pb.started.load(Ordering::SeqCst) == 1
        })
        .await;
        // b's restart fails once: the group is planned again, and the retry
        // stops a, held in its `pre_stop`.
        pb.fail_next.store(1, Ordering::SeqCst);
        pa.pre_stop_gate.store(true, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "the retry to ask a to stop", || {
            pa.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        let lines = rig
            .script(vec![Op::Read(a.clone()), Op::StopOutcome(a.clone())])
            .await;
        assert_eq!(lines[0], "alive=true pending=false", "{lines:?}");
        assert_eq!(lines[1], "Ok(Absorbed)", "{lines:?}");
        pa.pre_stop_gate.store(false, Ordering::SeqCst);
        until_idle(&rig, &[&a, &b]).await;
        wait_for_alive(&rig, &a).await;
        wait_for_alive(&rig, &b).await;
        sleep(QUIET).await;
        assert_eq!(
            pa.built.load(Ordering::SeqCst),
            3,
            "restarted by the retry once"
        );
    });
}

#[test]
fn stop_child_during_a_rest_for_one_retry_of_a_member_the_first_chain_restarted_answers_child_restarting(
) {
    common::run(0, async {
        let rig =
            supervisor(SupervisionConfig::rest_for_one().max_restarts(3, Duration::from_secs(60)))
                .await;
        let (x, px) = member(&rig, "x", Duration::from_secs(2), 0).await;
        let (y, py) = member(&rig, "y", Duration::from_secs(2), 0).await;
        wait_until(BOUND, "the children to start", || {
            px.started.load(Ordering::SeqCst) == 1 && py.started.load(Ordering::SeqCst) == 1
        })
        .await;
        // y's restart fails at once; the retry's attempt of y is held.
        py.fail_first.store(1, Ordering::SeqCst);
        py.held.store(true, Ordering::SeqCst);
        crash(&child_handle(&x).await).await;
        wait_until(BOUND, "the retry to begin y's restart", || {
            py.built.load(Ordering::SeqCst) == 3
        })
        .await;
        let lines = rig
            .script(vec![
                Op::Read(y.clone()),
                Op::StopOutcome(x.clone()),
                Op::Restart(x.clone()),
            ])
            .await;
        assert_eq!(lines[0], "alive=false pending=true", "{lines:?}");
        assert!(restarting(&lines[1]), "{lines:?}");
        assert!(restarting(&lines[2]), "{lines:?}");
        assert_eq!(px.built.load(Ordering::SeqCst), 2, "x was restarted once");
        py.held.store(false, Ordering::SeqCst);
        until_idle(&rig, &[&x, &y]).await;
        wait_for_alive(&rig, &y).await;
        let lines = rig.script(vec![Op::StopOutcome(x.clone())]).await;
        assert_eq!(lines[0], "Ok(Stopped)", "{lines:?}");
        wait_until(BOUND, "x to be bounced", || {
            px.built.load(Ordering::SeqCst) == 3
        })
        .await;
    });
}

#[test]
fn stop_child_of_an_awaited_member_whose_terminate_was_abandoned_answers_child_restarting() {
    common::run(0, async {
        let rig = supervisor(one_for_all()).await;
        let (a, pa) = member(&rig, "a", Duration::from_secs(2), 0).await;
        let (b, pb) = member(&rig, "b", Duration::from_secs(5), 0).await;
        wait_until(BOUND, "the children to start", || {
            pa.started.load(Ordering::SeqCst) == 1 && pb.started.load(Ordering::SeqCst) == 1
        })
        .await;
        pb.pre_stop_gate.store(true, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "the group to ask b to stop", || {
            pb.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        let lines = rig.script(vec![Op::TerminateAbandon(b.clone())]).await;
        assert_eq!(lines, ["abandoned"]);
        let lines = rig.script(vec![Op::StopOutcome(b.clone())]).await;
        assert!(restarting(&lines[0]), "never absorbed: {lines:?}");
        pb.pre_stop_gate.store(false, Ordering::SeqCst);
        until_idle(&rig, &[&a, &b]).await;
        wait_for_alive(&rig, &a).await;
        sleep(QUIET).await;
        assert_eq!(pb.built.load(Ordering::SeqCst), 1, "b stays down");
        removed_once(&rig, &b);
    });
}

/// OneForAll `[a, b]`, a crashes, b's restart attempt is held in its
/// initialization and fails once released; a is restarted by the chain. The
/// supervisor, the names and the probes.
async fn group_with_a_chain_waiting_on_a_failing_start() -> (Rig, [String; 2], [Arc<Probe>; 2]) {
    let rig =
        supervisor(SupervisionConfig::one_for_all().max_restarts(3, Duration::from_secs(60))).await;
    let (a, pa) = member(&rig, "a", Duration::from_secs(2), 0).await;
    let (b, pb) = member(&rig, "b", Duration::from_secs(2), 0).await;
    wait_until(BOUND, "the children to start", || {
        pa.started.load(Ordering::SeqCst) == 1 && pb.started.load(Ordering::SeqCst) == 1
    })
    .await;
    pb.held.store(true, Ordering::SeqCst);
    pb.fail_next.store(1, Ordering::SeqCst);
    crash(&child_handle(&a).await).await;
    wait_until(BOUND, "the chain to begin b's restart", || {
        pb.built.load(Ordering::SeqCst) == 2 && pa.started.load(Ordering::SeqCst) == 2
    })
    .await;
    (rig, [a, b], [pa, pb])
}

#[test]
fn stop_child_of_a_restarted_member_whose_terminate_is_in_progress_answers_child_restarting() {
    common::run(0, async {
        // The chain restarted `a` and holds at `b`'s failing start. A
        // terminate of `a` is abandoned at its commit and held in `a`'s
        // `pre_stop`: `a` is a member the terminate released, still stopping.
        let (rig, [a, b], [pa, pb]) = group_with_a_chain_waiting_on_a_failing_start().await;
        pa.pre_stop_gate.store(true, Ordering::SeqCst);
        let lines = rig
            .script(vec![
                Op::TerminateAbandon(a.clone()),
                Op::StopOutcome(a.clone()),
            ])
            .await;
        assert_eq!(lines[0], "abandoned", "{lines:?}");
        assert!(restarting(&lines[1]), "{lines:?}");
        assert_eq!(pa.pre_stops.load(Ordering::SeqCst), 1, "one ladder");
        pa.pre_stop_gate.store(false, Ordering::SeqCst);
        pb.held.store(false, Ordering::SeqCst);
        until_idle(&rig, &[&a, &b]).await;
        wait_for_alive(&rig, &b).await;
        sleep(QUIET).await;
        assert_eq!(pa.built.load(Ordering::SeqCst), 2, "a stays down");
    });
}

#[test]
fn stop_child_of_a_transient_child_spawned_during_a_pending_group_stays_down_through_the_retry() {
    common::run(0, async {
        for restart in [RestartType::Transient, RestartType::Temporary] {
            let (rig, [a, b], [pa, pb]) = group_with_a_chain_waiting_on_a_failing_start().await;
            let t = uname("t");
            let pt = Arc::new(Probe::default());
            // The stop lands while b's failing attempt is still in flight.
            let lines = rig
                .script(vec![
                    Op::Spawn {
                        name: t.clone(),
                        probe: pt.clone(),
                        shutdown: Shutdown::Timeout(Duration::from_secs(2)),
                        restart,
                        vetoes: 0,
                    },
                    Op::StopOutcome(t.clone()),
                    Op::Restart(t.clone()),
                    Op::Read(t.clone()),
                ])
                .await;
            assert_eq!(lines[0], "Ok(())", "{lines:?}");
            assert_eq!(lines[1], "Ok(Stopped)", "{restart:?}: {lines:?}");
            if restart == RestartType::Transient {
                assert!(restarting(&lines[2]), "{lines:?}");
                assert_eq!(lines[3], "alive=false pending=false", "{lines:?}");
            } else {
                assert!(lines[2].contains("not found"), "{lines:?}");
            }
            pb.held.store(false, Ordering::SeqCst);
            until_idle(&rig, &[&a, &b]).await;
            wait_for_alive(&rig, &b).await;
            sleep(QUIET).await;
            // The retry restarted a and b, and never the child spawned since.
            assert_eq!(pa.built.load(Ordering::SeqCst), 3);
            assert_eq!(pt.built.load(Ordering::SeqCst), 1);
            removed_once(&rig, &t);
            let lines = rig.script(vec![Op::Restart(t.clone())]).await;
            if restart == RestartType::Transient {
                assert_eq!(lines[0], "Ok(())", "its spec is kept: {lines:?}");
                wait_until(BOUND, "t to start again", || {
                    pt.started.load(Ordering::SeqCst) == 2
                })
                .await;
            } else {
                assert!(lines[0].contains("not found"), "removed: {lines:?}");
            }
        }
    });
}

#[test]
fn terminate_during_the_chain_keeps_the_member_down_through_the_retry() {
    common::run(0, async {
        // The member the chain restarted.
        let (rig, [a, b], [pa, pb]) = group_with_a_chain_waiting_on_a_failing_start().await;
        let lines = rig
            .script(vec![Op::Terminate(a.clone()), Op::Read(a.clone())])
            .await;
        assert_eq!(lines[0], "Ok(())", "{lines:?}");
        pb.held.store(false, Ordering::SeqCst);
        wait_until(BOUND, "the retry to restart b", || {
            pb.built.load(Ordering::SeqCst) == 3
        })
        .await;
        until_idle(&rig, &[&a, &b]).await;
        wait_for_alive(&rig, &b).await;
        sleep(QUIET).await;
        assert_eq!(pa.built.load(Ordering::SeqCst), 2, "a stays down");
        assert_eq!(
            rig.script(vec![Op::Read(a.clone())]).await[0],
            "alive=false pending=false"
        );
        // a's crash was reported as the group's restart, and the terminate of
        // its second incarnation once, as a removal.
        let events = rig.events_for(&a);
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[1].action, SupervisionAction::Removed, "{events:?}");

        // A child spawned during the pending group and then terminated.
        let (rig, [_a, b], [_pa, pb]) = group_with_a_chain_waiting_on_a_failing_start().await;
        let t = uname("t");
        let pt = Arc::new(Probe::default());
        let lines = rig
            .script(vec![
                Op::Spawn {
                    name: t.clone(),
                    probe: pt.clone(),
                    shutdown: Shutdown::Timeout(Duration::from_secs(2)),
                    restart: RestartType::Permanent,
                    vetoes: 0,
                },
                Op::Terminate(t.clone()),
            ])
            .await;
        assert_eq!(lines, ["Ok(())", "Ok(())"]);
        pb.held.store(false, Ordering::SeqCst);
        wait_until(BOUND, "the retry to restart b", || {
            pb.built.load(Ordering::SeqCst) == 3
        })
        .await;
        until_idle(&rig, &[&b, &t]).await;
        sleep(QUIET).await;
        assert_eq!(pt.built.load(Ordering::SeqCst), 1, "t stays down");

        // RestForOne `[x, y]`: the retry's attempt of y is in flight when x,
        // which no RestForOne retry includes, is terminated.
        let rig =
            supervisor(SupervisionConfig::rest_for_one().max_restarts(3, Duration::from_secs(60)))
                .await;
        let (x, px) = member(&rig, "x", Duration::from_secs(2), 0).await;
        let (y, py) = member(&rig, "y", Duration::from_secs(2), 0).await;
        wait_until(BOUND, "the children to start", || {
            px.started.load(Ordering::SeqCst) == 1 && py.started.load(Ordering::SeqCst) == 1
        })
        .await;
        py.fail_first.store(1, Ordering::SeqCst);
        py.held.store(true, Ordering::SeqCst);
        crash(&child_handle(&x).await).await;
        wait_until(BOUND, "the retry to begin y's restart", || {
            py.built.load(Ordering::SeqCst) == 3
        })
        .await;
        let lines = rig
            .script(vec![Op::Terminate(x.clone()), Op::Restart(x.clone())])
            .await;
        assert_eq!(lines[0], "Ok(())", "{lines:?}");
        assert!(
            restarting(&lines[1]),
            "released until the group completes: {lines:?}"
        );
        py.held.store(false, Ordering::SeqCst);
        until_idle(&rig, &[&x, &y]).await;
        wait_for_alive(&rig, &y).await;
        sleep(QUIET).await;
        assert_eq!(px.built.load(Ordering::SeqCst), 2, "x stays down");
        let events = rig.events_for(&x);
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[1].action, SupervisionAction::Removed, "{events:?}");
    });
}

#[test]
fn terminate_delete_respawn_in_one_handler_succeeds_in_every_group_phase() {
    common::run(0, async {
        // The chain phase, a member the chain already restarted.
        let (rig, [a, b], [pa, pb]) = group_with_a_chain_waiting_on_a_failing_start().await;
        let respawned = Arc::new(Probe::default());
        let lines = rig
            .script(vec![
                Op::Terminate(a.clone()),
                Op::Delete(a.clone()),
                Op::Spawn {
                    name: a.clone(),
                    probe: respawned.clone(),
                    shutdown: Shutdown::Timeout(Duration::from_secs(2)),
                    restart: RestartType::Permanent,
                    vetoes: 0,
                },
            ])
            .await;
        assert_eq!(lines, ["Ok(())", "Ok(())", "Ok(())"]);
        pb.held.store(false, Ordering::SeqCst);
        until_idle(&rig, &[&a, &b]).await;
        sleep(QUIET).await;
        let _ = pa;

        // A retry's stop phase: b's failed start leaves it down in the
        // retry's restart set while the retry stops a.
        let rig =
            supervisor(SupervisionConfig::one_for_all().max_restarts(3, Duration::from_secs(60)))
                .await;
        let (a, pa) = member(&rig, "a", Duration::from_secs(5), 0).await;
        let (b, pb) = member(&rig, "b", Duration::from_secs(2), 0).await;
        wait_until(BOUND, "the children to start", || {
            pa.started.load(Ordering::SeqCst) == 1 && pb.started.load(Ordering::SeqCst) == 1
        })
        .await;
        pb.fail_next.store(1, Ordering::SeqCst);
        pa.pre_stop_gate.store(true, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "the retry to ask a to stop", || {
            pa.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        let new_b = Arc::new(Probe::default());
        let lines = rig
            .script(vec![
                Op::Terminate(b.clone()),
                Op::Delete(b.clone()),
                Op::Spawn {
                    name: b.clone(),
                    probe: new_b.clone(),
                    shutdown: Shutdown::Timeout(Duration::from_secs(2)),
                    restart: RestartType::Permanent,
                    vetoes: 0,
                },
                Op::StopOutcome(b.clone()),
            ])
            .await;
        assert_eq!(&lines[..3], ["Ok(())", "Ok(())", "Ok(())"], "{lines:?}");
        assert_eq!(
            lines[3], "Ok(Stopped)",
            "the new child is no member: {lines:?}"
        );
        pa.pre_stop_gate.store(false, Ordering::SeqCst);
        until_idle(&rig, &[&a, &b]).await;
        wait_for_alive(&rig, &a).await;
        wait_for_alive(&rig, &b).await;
        assert_eq!(new_b.built.load(Ordering::SeqCst), 2, "built twice");

        // A `Temporary` member: the terminate prunes the spec and records
        // nothing, and a later spawn under the same id is a non-member.
        let rig =
            supervisor(SupervisionConfig::one_for_all().max_restarts(3, Duration::from_secs(60)))
                .await;
        let (a, pa) = member(&rig, "a", Duration::from_secs(2), 0).await;
        let t = uname("t");
        let pt = Arc::new(Probe::default());
        rig.spawn_child(
            &t,
            &pt,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Temporary,
            0,
        )
        .await;
        wait_until(BOUND, "the children to start", || {
            pa.started.load(Ordering::SeqCst) == 1 && pt.started.load(Ordering::SeqCst) == 1
        })
        .await;
        pa.held.store(false, Ordering::SeqCst);
        pa.pre_stop_gate.store(false, Ordering::SeqCst);
        let respawned = Arc::new(Probe::default());
        // a's restart is held so the group pends while the handler runs.
        pa.held.store(true, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "the chain to begin a's restart", || {
            pa.built.load(Ordering::SeqCst) == 2
        })
        .await;
        let lines = rig
            .script(vec![
                Op::Spawn {
                    name: t.clone(),
                    probe: pt.clone(),
                    shutdown: Shutdown::Timeout(Duration::from_secs(2)),
                    restart: RestartType::Temporary,
                    vetoes: 0,
                },
                Op::Terminate(t.clone()),
                Op::Spawn {
                    name: t.clone(),
                    probe: respawned.clone(),
                    shutdown: Shutdown::Timeout(Duration::from_secs(2)),
                    restart: RestartType::Temporary,
                    vetoes: 0,
                },
                Op::StopOutcome(t.clone()),
            ])
            .await;
        assert_eq!(
            lines,
            ["Ok(())", "Ok(())", "Ok(())", "Ok(Stopped)"],
            "{lines:?}"
        );
        pa.held.store(false, Ordering::SeqCst);
        until_idle(&rig, &[&a]).await;
        wait_for_alive(&rig, &a).await;
        sleep(QUIET).await;
        // The group never restarted anything under t's id: every report of
        // it is a removal, and the respawn was built once.
        let events = rig.events_for(&t);
        assert!(
            !events.is_empty()
                && events
                    .iter()
                    .all(|event| event.action == SupervisionAction::Removed),
            "{events:?}"
        );
        assert_eq!(respawned.built.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn stop_child_of_a_transient_member_stopping_under_its_group_stops_it_and_keeps_it_down_through_the_retry(
) {
    common::run(0, async {
        let rig =
            supervisor(SupervisionConfig::one_for_all().max_restarts(3, Duration::from_secs(60)))
                .await;
        let (a, pa) = member(&rig, "a", Duration::from_secs(2), 0).await;
        let b = uname("b");
        let pb = Arc::new(Probe::default());
        rig.ok(Cmd::Spawn {
            name: b.clone(),
            probe: pb.clone(),
            shutdown: Shutdown::Timeout(Duration::from_secs(5)),
            restart: RestartType::Transient,
            vetoes: 0,
        })
        .await;
        let (c, pc) = member(&rig, "c", Duration::from_secs(2), 0).await;
        wait_until(BOUND, "the children to start", || {
            [&pa, &pb, &pc]
                .iter()
                .all(|p| p.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        // c's restart fails once; b's stop under the group is held.
        pc.fail_next.store(1, Ordering::SeqCst);
        pb.pre_stop_gate.store(true, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "the group to ask b to stop", || {
            pb.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        let lines = rig
            .script(vec![
                Op::Read(b.clone()),
                Op::StopOutcomeThenOpen(b.clone(), pb.clone()),
            ])
            .await;
        assert_eq!(lines[0], "alive=true pending=false", "{lines:?}");
        assert_eq!(lines[1], "Ok(Stopped)", "never absorbed: {lines:?}");
        until_idle(&rig, &[&a, &b, &c]).await;
        wait_for_alive(&rig, &c).await;
        sleep(QUIET).await;
        // The retry restarted a and c, never b; b's spec is kept.
        assert_eq!(pa.built.load(Ordering::SeqCst), 3);
        assert_eq!(pb.built.load(Ordering::SeqCst), 1);
        removed_once(&rig, &b);
        assert_eq!(
            rig.script(vec![Op::Read(b.clone())]).await[0],
            "alive=false pending=false"
        );
        let lines = rig.script(vec![Op::Restart(b.clone())]).await;
        assert_eq!(lines[0], "Ok(())", "{lines:?}");
        wait_until(BOUND, "b to start again", || {
            pb.started.load(Ordering::SeqCst) == 2
        })
        .await;
    });
}

#[test]
fn stop_child_of_a_temporary_member_stopping_under_its_group_stops_and_removes_it() {
    common::run(0, async {
        let rig = supervisor(one_for_all()).await;
        let (a, pa) = member(&rig, "a", Duration::from_secs(2), 0).await;
        let b = uname("b");
        let pb = Arc::new(Probe::default());
        rig.ok(Cmd::Spawn {
            name: b.clone(),
            probe: pb.clone(),
            shutdown: Shutdown::Timeout(Duration::from_secs(5)),
            restart: RestartType::Temporary,
            vetoes: 0,
        })
        .await;
        let (c, pc) = member(&rig, "c", Duration::from_secs(2), 0).await;
        wait_until(BOUND, "the children to start", || {
            [&pa, &pb, &pc]
                .iter()
                .all(|p| p.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        pb.pre_stop_gate.store(true, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "the group to ask b to stop", || {
            pb.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        let lines = rig
            .script(vec![
                Op::Read(b.clone()),
                Op::StopOutcomeThenOpen(b.clone(), pb.clone()),
            ])
            .await;
        assert_eq!(lines[0], "alive=true pending=false", "{lines:?}");
        assert_eq!(lines[1], "Ok(Stopped)", "never absorbed: {lines:?}");
        until_idle(&rig, &[&a, &c]).await;
        wait_for_alive(&rig, &c).await;
        sleep(QUIET).await;
        assert_eq!(pb.built.load(Ordering::SeqCst), 1);
        removed_once(&rig, &b);
        let lines = rig
            .script(vec![Op::Read(b.clone()), Op::Restart(b.clone())])
            .await;
        assert_eq!(lines[0], "no such child", "{lines:?}");
        assert!(lines[1].contains("not found"), "{lines:?}");
    });
}

#[test]
fn terminate_child_after_a_restart_completes_succeeds() {
    common::run(0, async {
        let rig = supervisor(plain()).await;
        let (w, pw) = member(&rig, "w", Duration::from_secs(2), 0).await;
        wait_until(BOUND, "w to start", || {
            pw.started.load(Ordering::SeqCst) == 1
        })
        .await;
        pw.held.store(true, Ordering::SeqCst);
        crash(&child_handle(&w).await).await;
        wait_until(BOUND, "the restart attempt to be built", || {
            pw.built.load(Ordering::SeqCst) == 2
        })
        .await;
        let lines = rig.script(vec![Op::Terminate(w.clone())]).await;
        assert!(
            restarting(&lines[0]),
            "while the attempt is in flight: {lines:?}"
        );
        pw.held.store(false, Ordering::SeqCst);
        wait_for_alive(&rig, &w).await;
        let lines = rig.script(vec![Op::Terminate(w.clone())]).await;
        assert_eq!(lines[0], "Ok(())", "once the restart completed: {lines:?}");
        sleep(QUIET).await;
        assert_eq!(pw.built.load(Ordering::SeqCst), 2, "w stays down");
    });
}

#[test]
fn terminate_child_on_a_death_awaiting_its_decision_dismisses_the_restart() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("dismiss")).unwrap();
        let rig = supervisor_on(
            &sys,
            SupervisionConfig::rest_for_one().max_restarts(5, Duration::from_secs(60)),
        )
        .await;
        let p = uname("p");
        // P is a supervisor child; its worker W sits in a turn that ends
        // only when W's kill grace of 1500 ms has passed after P's end.
        let p_built = Arc::new(AtomicUsize::new(0));
        rig.ok(Cmd::SpawnSupervisorChild {
            name: p.clone(),
            shutdown: Shutdown::Timeout(Duration::from_millis(100)),
            restart: RestartType::Permanent,
            config: plain(),
            built: p_built.clone(),
            events: Arc::new(Mutex::new(Vec::new())),
            knobs: Arc::new(Knobs::default()),
        })
        .await;
        let w = uname("w");
        let pw = Arc::new(Probe::default());
        let inner = sys
            .get::<Sup>(&p)
            .expect("the child supervisor is registered");
        let spawned = inner
            .send(Cmd::SpawnWithGrace {
                name: w.clone(),
                probe: pw.clone(),
                shutdown: Shutdown::Timeout(Duration::from_secs(5)),
                kill_grace: Duration::from_millis(1500),
            })
            .await
            .expect("the child supervisor answers");
        assert!(matches!(spawned, Reply::Done(Ok(()))), "{spawned:?}");
        let (y, py) = member(&rig, "y", Duration::from_secs(2), 0).await;
        let (z, pz) = member(&rig, "z", Duration::from_secs(5), 0).await;
        wait_until(BOUND, "the children to start", || {
            py.started.load(Ordering::SeqCst) == 1 && pz.started.load(Ordering::SeqCst) == 1
        })
        .await;
        let child_w = child_in(&sys, &w).await;
        child_w.notify(ChildMsg::Park).await.expect("park W's turn");
        sleep(Duration::from_millis(50)).await;
        // y fails: the group [y, z] pends in z's held stop. P, before y, is
        // outside it. P is then killed: its death waits behind the group.
        pz.pre_stop_gate.store(true, Ordering::SeqCst);
        crash(&child_in(&sys, &y).await).await;
        wait_until(BOUND, "the group to ask z to stop", || {
            pz.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        let outcome = inner
            .stop_within(StopReason::Kill, Duration::from_millis(200))
            .await;
        assert!(outcome.is_ok(), "{outcome:?}");
        until_pending(&rig, &p).await;
        // The terminate dismisses the owed decision and returns once P's
        // whole subtree, W included, has ended.
        let lines = rig
            .script(vec![
                Op::Stop(p.clone()),
                Op::Terminate(p.clone()),
                Op::Read(p.clone()),
            ])
            .await;
        assert!(
            restarting(&lines[0]),
            "a bounce keeps the owed decision: {lines:?}"
        );
        assert_eq!(lines[1], "Ok(())", "{lines:?}");
        assert!(
            pw.ended.load(Ordering::SeqCst),
            "W ended before the call returned"
        );
        assert_eq!(lines[2], "alive=false pending=false", "{lines:?}");
        assert!(rig.events_for(&p).is_empty(), "delivered after the group");
        pz.pre_stop_gate.store(false, Ordering::SeqCst);
        wait_until(BOUND, "P's death to be delivered", || {
            !rig.events_for(&p).is_empty()
        })
        .await;
        sleep(QUIET).await;
        removed_once(&rig, &p);
        assert_eq!(p_built.load(Ordering::SeqCst), 1, "P is not restarted");
        let _ = (y, z);
    });
}

#[test]
fn terminate_calls_during_one_group_share_one_more_ask() {
    common::run(0, async {
        // OneForAll `[x, b, d]`: d's restart is held, so the group stays open
        // for the whole test. b is under `Infinity`; its first incarnation
        // accepts the group's stop, and its chain-restarted incarnation
        // vetoes the first two requests it gets (the probe counts the first
        // incarnation's call too) and accepts the next.
        let rig = supervisor(one_for_all()).await;
        let (x, px) = member(&rig, "x", Duration::from_secs(2), 0).await;
        let b = uname("b");
        let pb = Arc::new(Probe::default());
        rig.spawn_child(&b, &pb, Shutdown::Infinity, RestartType::Permanent, 0)
            .await;
        let (d, pd) = member(&rig, "d", Duration::from_secs(2), 0).await;
        wait_until(BOUND, "the children to start", || {
            [&px, &pb, &pd]
                .iter()
                .all(|p| p.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        pb.veto_script.lock().unwrap().push_back(3);
        pd.held.store(true, Ordering::SeqCst);
        crash(&child_handle(&x).await).await;
        wait_until(BOUND, "the chain to restart b and begin d", || {
            pb.started.load(Ordering::SeqCst) == 2 && pd.built.load(Ordering::SeqCst) == 2
        })
        .await;
        let still_pending = |rig: &Rig| {
            let sup = rig.sup.clone();
            let d = d.clone();
            async move {
                let reply = sup.send(Cmd::Script(vec![Op::Read(d)])).await.unwrap();
                matches!(reply, Reply::Steps(lines) if lines[0].ends_with("pending=true"))
            }
        };
        for call in 1..=3usize {
            assert!(
                still_pending(&rig).await,
                "the group pends before call {call}"
            );
            let lines = rig.script(vec![Op::TerminateAbandon(b.clone())]).await;
            assert_eq!(lines, ["abandoned"]);
            match call {
                1 => {
                    // The call's own first request.
                    wait_until(BOUND, "b to answer the call's request", || {
                        pb.pre_stops_returned.load(Ordering::SeqCst) == 2
                    })
                    .await;
                }
                2 => {
                    // The one more ask, shared with the group.
                    wait_until(BOUND, "b to answer the shared ask", || {
                        pb.pre_stops_returned.load(Ordering::SeqCst) == 3
                    })
                    .await;
                }
                _ => sleep(QUIET).await,
            }
        }
        assert!(still_pending(&rig).await);
        assert_eq!(
            pb.pre_stops.load(Ordering::SeqCst),
            3,
            "the first stop's request and the one shared ask, and no third"
        );
        // The parent's stop asks b again, and b accepts.
        let _ = stop_supervisor(&rig, BOUND).await;
        assert_eq!(pb.pre_stops.load(Ordering::SeqCst), 4);
        assert!(pb.ended.load(Ordering::SeqCst));
    });
}

#[test]
fn stop_and_terminate_of_a_child_whose_task_ended_complete_at_once_with_its_real_reason() {
    common::run(0, async {
        // terminate_child, a child ended through its handle.
        let rig = supervisor(plain()).await;
        let (name, probe) = member(&rig, "ended", Duration::from_secs(2), 0).await;
        let lines = rig
            .script(vec![
                Op::KillThroughHandle(name.clone()),
                Op::UntilNotAlive(name.clone()),
                Op::Restart(name.clone()),
                Op::Delete(name.clone()),
                Op::Terminate(name.clone()),
            ])
            .await;
        assert_eq!(lines[1], "not alive", "{lines:?}");
        assert!(restarting(&lines[2]) && restarting(&lines[3]), "{lines:?}");
        assert_eq!(lines[4], "Ok(())", "{lines:?}");
        sleep(QUIET).await;
        assert_eq!(
            probe.built.load(Ordering::SeqCst),
            1,
            "the child stays down"
        );
        let events = rig.events_for(&name);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].action, SupervisionAction::Removed);
        assert!(matches!(events[0].reason, StopReason::Kill), "{events:?}");

        // stop_child, a child that failed: the failure is evaluated and
        // charged once as a crash (a budget of one), and the child restarts.
        let rig =
            supervisor(SupervisionConfig::one_for_one().max_restarts(1, Duration::from_secs(60)))
                .await;
        let (name, probe) = member(&rig, "failed", Duration::from_secs(2), 0).await;
        wait_until(BOUND, "the child to start", || {
            probe.started.load(Ordering::SeqCst) == 1
        })
        .await;
        let lines = rig
            .script(vec![
                Op::SlowFail(name.clone()),
                Op::UntilInTurn(probe.clone()),
                Op::UntilNotAlive(name.clone()),
                Op::Restart(name.clone()),
                Op::Delete(name.clone()),
                Op::Stop(name.clone()),
            ])
            .await;
        assert_eq!(lines[2], "not alive", "{lines:?}");
        assert!(restarting(&lines[3]) && restarting(&lines[4]), "{lines:?}");
        assert_eq!(lines[5], "Ok(())", "{lines:?}");
        wait_until(BOUND, "the crash to be restarted", || {
            probe.started.load(Ordering::SeqCst) == 2
        })
        .await;
        let events = rig.events_for(&name);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].action, SupervisionAction::RestartInitiated);
        assert!(
            matches!(events[0].reason, StopReason::Failure(_)),
            "{events:?}"
        );
        // The crash was charged: the next one exhausts the budget.
        crash(&child_handle(&name).await).await;
        within(BOUND, "the supervisor to stop", rig.sup.wait_stopped()).await;
    });
}

#[test]
fn terminate_of_a_member_the_chain_restarted_stays_down_through_the_retry() {
    common::run(0, async {
        let rig =
            supervisor(SupervisionConfig::one_for_all().max_restarts(3, Duration::from_secs(60)))
                .await;
        let (a, b) = (uname("a"), uname("b"));
        let (pa, pb, _pc) = probes();
        spawn_members(&rig, &[(&a, &pa), (&b, &pb)]).await;
        wait_until(BOUND, "the children to finish starting", || {
            pa.started.load(Ordering::SeqCst) == 1 && pb.started.load(Ordering::SeqCst) == 1
        })
        .await;
        // b's restart waits in its init until released, then fails once.
        pb.held.store(true, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "a's second incarnation started by the chain", || {
            pa.started.load(Ordering::SeqCst) == 2
        })
        .await;
        wait_for_alive(&rig, &a).await;
        wait_until(BOUND, "b's second incarnation built", || {
            pb.built.load(Ordering::SeqCst) == 2
        })
        .await;
        // The caller releases a while the group restart is pending.
        rig.ok(Cmd::Terminate { name: a.clone() }).await;
        sleep(QUIET).await;
        pb.fail_next.store(1, Ordering::SeqCst);
        pb.held.store(false, Ordering::SeqCst);
        wait_until(BOUND, "b started again after the retry", || {
            pb.started.load(Ordering::SeqCst) == 2
        })
        .await;
        sleep(QUIET).await;
        // The released a is not started again by the retry, and its removal
        // was reported once after the group's restart of it.
        assert_eq!(
            pa.built.load(Ordering::SeqCst),
            2,
            "the released a stays down"
        );
        let events = rig.events_for(&a);
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[1].action, SupervisionAction::Removed, "{events:?}");
    });
}

#[test]
fn stop_child_handled_as_a_terminate_waits_for_the_childs_subtree() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("as-terminate")).unwrap();
        let rig = supervisor_on(&sys, one_for_all()).await;
        let (a, pa) = member(&rig, "a", Duration::from_secs(2), 0).await;
        // P is a `Transient` supervisor child with a short window; its worker
        // W sits in a turn that ends only when W's kill grace of 1500 ms has
        // passed after P's end.
        let p = uname("p");
        let p_built = Arc::new(AtomicUsize::new(0));
        rig.ok(Cmd::SpawnSupervisorChild {
            name: p.clone(),
            shutdown: Shutdown::Timeout(Duration::from_millis(100)),
            restart: RestartType::Transient,
            config: plain(),
            built: p_built.clone(),
            events: Arc::new(Mutex::new(Vec::new())),
            knobs: Arc::new(Knobs::default()),
        })
        .await;
        let w = uname("w");
        let pw = Arc::new(Probe::default());
        let inner = sys
            .get::<Sup>(&p)
            .expect("the child supervisor is registered");
        let spawned = inner
            .send(Cmd::SpawnWithGrace {
                name: w.clone(),
                probe: pw.clone(),
                shutdown: Shutdown::Timeout(Duration::from_secs(5)),
                kill_grace: Duration::from_millis(1500),
            })
            .await
            .expect("the child supervisor answers");
        assert!(matches!(spawned, Reply::Done(Ok(()))), "{spawned:?}");
        wait_until(BOUND, "a to start", || {
            pa.started.load(Ordering::SeqCst) == 1
        })
        .await;
        child_in(&sys, &w)
            .await
            .notify(ChildMsg::Park)
            .await
            .expect("park W's turn");
        sleep(Duration::from_millis(50)).await;
        // a crashes: the group stops P, a `Transient` member.
        crash(&child_in(&sys, &a).await).await;
        until_pending(&rig, &a).await;
        // A bounce of P is its terminate: it stops P, keeps it down through
        // the group restart, and returns once W has ended too.
        let lines = rig.script(vec![Op::StopOutcome(p.clone())]).await;
        assert_eq!(lines[0], "Ok(Stopped)", "{lines:?}");
        assert!(
            pw.ended.load(Ordering::SeqCst),
            "the call returned before the subtree ended"
        );
        until_idle(&rig, &[&a, &p]).await;
        assert_eq!(p_built.load(Ordering::SeqCst), 1, "P stays down");
    });
}

// ---------------------------------------------------------------------------
// Kill finality: a `Kill` is final for its target at its next turn boundary
// on every path, and every kill entry point ends in an abort once the
// target's kill grace has passed
// ---------------------------------------------------------------------------

/// A supervisor that runs one `Temporary` child and records how it ends.
struct KillTop<C: Actor> {
    factory: Arc<dyn Fn() -> C + Send + Sync>,
    child: String,
    shutdown: Shutdown,
    kill_grace: Duration,
    /// Whether the child is itself a supervisor.
    supervisor_child: bool,
    events: Events,
}

impl<C: Actor> Actor for KillTop<C> {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let factory = self.factory.clone();
        let mut builder = ctx
            .spawn_child(move || factory())
            .named(self.child.clone())
            .restart_type(RestartType::Temporary)
            .shutdown(self.shutdown)
            .kill_grace(self.kill_grace);
        if self.supervisor_child {
            builder = builder.with_config(ActorConfig::default().supervisor());
        }
        builder.await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}

/// Spawns a [`KillTop`] around `factory` on `sys`; returns it and the
/// events it records.
async fn kill_top<C: Actor>(
    sys: &Arc<ActorSystem>,
    child: &str,
    shutdown: Shutdown,
    kill_grace: Duration,
    supervisor_child: bool,
    factory: impl Fn() -> C + Send + Sync + 'static,
) -> (ActorHandle<KillTop<C>>, Events) {
    let events: Events = Arc::new(Mutex::new(Vec::new()));
    let top = KillTop {
        factory: Arc::new(factory),
        child: child.to_string(),
        shutdown,
        kill_grace,
        supervisor_child,
        events: events.clone(),
    }
    .spawn()
    .named(uname("kill-top"))
    .on_system(sys)
    .supervisor()
    .await
    .expect("the top supervisor spawns");
    (top, events)
}

/// The handle of a registered actor of type `A`.
async fn registered_as<A: Actor>(sys: &Arc<ActorSystem>, name: &str) -> ActorHandle<A> {
    let mut found = None;
    wait_until(BOUND, "the actor to be registered", || {
        found = sys.get::<A>(name);
        found.is_some()
    })
    .await;
    found.expect("registered")
}

/// The reason the single recorded end of the top supervisor's child carried.
async fn child_end(events: &Events) -> StopReason {
    wait_until(BOUND, "the child's end to be reported", || {
        !events.lock().unwrap().is_empty()
    })
    .await;
    let recorded = events.lock().unwrap();
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    recorded[0].reason.clone()
}

/// A supervisor whose `on_stopped` holds until the test releases it, with one
/// child that has a 2 s window.
struct SlowStop {
    probe: Arc<Probe>,
    veto_name: String,
    /// How many requests to stop the child vetoes (`always_vetoes()` for a
    /// child that refuses every one, 0 for one that accepts the first).
    vetoes: usize,
    in_on_stopped: Arc<AtomicBool>,
    /// `on_stopped` returns once this is set (or after ten seconds).
    release: Arc<AtomicBool>,
}

impl Actor for SlowStop {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let probe = self.probe.clone();
        let vetoes = self.vetoes;
        ctx.spawn_child(move || probe.instance(vetoes))
            .named(self.veto_name.clone())
            .shutdown(Shutdown::Timeout(Duration::from_secs(2)))
            .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.in_on_stopped.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.release.load(Ordering::SeqCst) && Instant::now() < deadline {
            sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    }
}

/// A `Kill` raised while a supervisor's `on_stopped` runs, which returns only
/// after the `Kill` was accepted, takes the Kill teardown whether its child
/// vetoes every request (`vetoes` `always_vetoes()`) or accepts the first:
/// the child is killed and asked nothing.
async fn kill_during_on_stopped(vetoes: usize, paused: bool) {
    {
        let sys = ActorSystem::create(uname("kill-on-stopped")).unwrap();
        let probe = Arc::new(Probe::default());
        let veto_name = uname("veto");
        let in_on_stopped = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let slow_name = uname("slow");
        let (_top, events) = kill_top(
            &sys,
            &slow_name,
            Shutdown::Timeout(Duration::from_secs(5)),
            // Long enough that the lane read, not the abort, ends the
            // supervisor's `on_stopped`.
            Duration::from_secs(2),
            true,
            {
                let (probe, veto_name, flag, release) = (
                    probe.clone(),
                    veto_name.clone(),
                    in_on_stopped.clone(),
                    release.clone(),
                );
                move || SlowStop {
                    probe: probe.clone(),
                    veto_name: veto_name.clone(),
                    vetoes,
                    in_on_stopped: flag.clone(),
                    release: release.clone(),
                }
            },
        )
        .await;
        let slow = registered_as::<SlowStop>(&sys, &slow_name).await;
        let _veto = child_in(&sys, &veto_name).await;

        let started = Instant::now();
        slow.stop(StopReason::Graceful).await.unwrap();
        wait_until(BOUND, "on_stopped to start", || {
            in_on_stopped.load(Ordering::SeqCst)
        })
        .await;
        slow.stop(StopReason::Kill)
            .await
            .expect("a Kill is accepted while on_stopped runs");
        release.store(true, Ordering::SeqCst);

        // The supervisor reaches `Stopped` inside a 1 s bound, before the
        // child's 2 s window could end: the child is not asked to stop.
        within(
            upper(paused, Duration::from_secs(1)),
            "the supervisor to publish Stopped",
            slow.wait_stopped(),
        )
        .await;
        assert!(started.elapsed() < upper(paused, Duration::from_secs(1)));
        assert!(matches!(child_end(&events).await, StopReason::Kill));
        assert_eq!(
            probe.stopped_runs.load(Ordering::SeqCst),
            0,
            "the child ran no on_stopped"
        );
        assert_eq!(
            probe.pre_stops.load(Ordering::SeqCst),
            0,
            "the Kill teardown asks nothing of the child: no pre_stop ran"
        );
        wait_until(BOUND, "the child's task to end", || {
            probe.ended.load(Ordering::SeqCst)
        })
        .await;
    }
}

#[test]
fn kill_during_on_stopped_takes_the_kill_teardown() {
    common::run(0, kill_during_on_stopped(always_vetoes(), false));
}

#[test]
fn kill_during_on_stopped_takes_the_kill_teardown_paused() {
    common::run_paused(kill_during_on_stopped(always_vetoes(), true));
}

#[test]
fn kill_during_on_stopped_reaches_a_cooperative_child_as_kill() {
    // A child that would accept its parent's stop: it still never runs
    // `pre_stop` or `on_stopped`, because the Kill teardown asks nothing of
    // it.
    common::run(0, kill_during_on_stopped(0, false));
}

#[test]
fn kill_during_on_stopped_reaches_a_cooperative_child_as_kill_paused() {
    common::run_paused(kill_during_on_stopped(0, true));
}

/// Records which start hooks ran. `pre_start` takes `pre_start_ms`.
struct Initer {
    pre_start_began: Arc<AtomicBool>,
    /// `pre_start` holds until this is set (for at most ten seconds).
    pre_start_release: Arc<AtomicBool>,
    on_started_ran: Arc<AtomicBool>,
    on_stopped_ran: Arc<AtomicBool>,
}

impl Actor for Initer {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.pre_start_began.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.pre_start_release.load(Ordering::SeqCst) && Instant::now() < deadline {
            sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    }

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.on_started_ran.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        _reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.on_stopped_ran.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Clone, Default)]
struct IniterFlags {
    pre_start_began: Arc<AtomicBool>,
    pre_start_release: Arc<AtomicBool>,
    on_started_ran: Arc<AtomicBool>,
    on_stopped_ran: Arc<AtomicBool>,
}

impl IniterFlags {
    fn actor(&self) -> Initer {
        Initer {
            pre_start_began: self.pre_start_began.clone(),
            pre_start_release: self.pre_start_release.clone(),
            on_started_ran: self.on_started_ran.clone(),
            on_stopped_ran: self.on_stopped_ran.clone(),
        }
    }
}

#[test]
fn kill_before_or_during_pre_start_skips_every_later_init_hook() {
    // A kill raised while `pre_start` holds; `pre_start` returns once the
    // kill was accepted, inside the 500 ms grace, so the lane read, and not
    // the abort, ends the init.
    common::run(0, async {
        let sys = ActorSystem::create(uname("kill-in-pre-start")).unwrap();
        let flags = IniterFlags::default();
        let name = uname("initer");
        let (_top, events) = kill_top(
            &sys,
            &name,
            Shutdown::Timeout(Duration::from_secs(5)),
            Duration::from_millis(500),
            false,
            {
                let flags = flags.clone();
                move || flags.actor()
            },
        )
        .await;
        wait_until(BOUND, "pre_start to begin", || {
            flags.pre_start_began.load(Ordering::SeqCst)
        })
        .await;
        sys.kill(&name).await.expect("the kill is accepted");
        flags.pre_start_release.store(true, Ordering::SeqCst);
        assert!(matches!(child_end(&events).await, StopReason::Kill));
        assert!(
            !flags.on_started_ran.load(Ordering::SeqCst),
            "on_started never begins after the Kill"
        );
        assert!(
            !flags.on_stopped_ran.load(Ordering::SeqCst),
            "no on_stopped runs"
        );
    });
}

#[test]
fn kill_before_the_first_poll_skips_pre_start() {
    // With a single thread nothing runs between the spawn and the kill, so
    // the Kill is stored before the task is first polled.
    common::run_current_thread_only(async {
        let sys = ActorSystem::create(uname("kill-before-poll")).unwrap();
        let flags = IniterFlags::default();
        let name = uname("initer");
        let handle = flags
            .actor()
            .spawn()
            .named(&name)
            .on_system(&sys)
            .detached()
            .await
            .expect("the spawn is accepted");
        sys.kill(&name).await.expect("the kill is accepted");
        within(BOUND, "the actor to stop", handle.wait_stopped()).await;
        assert!(
            !flags.pre_start_began.load(Ordering::SeqCst),
            "pre_start never begins"
        );
        assert!(!flags.on_started_ran.load(Ordering::SeqCst));
        assert!(!flags.on_stopped_ran.load(Ordering::SeqCst));
    });
}

/// Parks in `pre_start` for good.
struct PreStartParked;

impl Actor for PreStartParked {
    type Message = ();
    type Response = ();

    async fn pre_start(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        std::future::pending::<()>().await;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// Polls a future that never awaits on a pending value, once: a lane request
/// answers in its first poll.
fn poll_once<T>(fut: impl std::future::Future<Output = T>) -> Option<T> {
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(value) => Some(value),
        std::task::Poll::Pending => None,
    }
}

#[test]
fn kill_in_the_pre_attach_window_still_ends_in_an_abort() {
    // A real race between a spawn and a kill on another thread: a name is
    // registered before the task is spawned, so a kill can find the entry
    // before the entry holds the task's abort handle. The killer spins on
    // `kill(name)` from the moment the spawn starts, so its first answer lands
    // right after the registration.
    common::run_multi_thread_only(4, async {
        let sys = ActorSystem::create(uname("pre-attach")).unwrap();
        let mut not_aborted = 0usize;
        for batch in 0..20 {
            let mut racers = Vec::new();
            for i in 0..25 {
                let name = format!(
                    "pre-attach-{batch}-{i}-{}",
                    UNIQ.fetch_add(1, Ordering::Relaxed)
                );
                let go = Arc::new(AtomicBool::new(false));
                let killer = {
                    let (sys, name, go) = (sys.clone(), name.clone(), go.clone());
                    std::thread::spawn(move || {
                        while !go.load(Ordering::SeqCst) {
                            std::hint::spin_loop();
                        }
                        loop {
                            match poll_once(sys.kill(&name)) {
                                Some(Ok(())) => return true,
                                Some(Err(tokio_actors::SendError::NotFound)) => {
                                    std::thread::yield_now();
                                }
                                _ => return false,
                            }
                        }
                    })
                };
                go.store(true, Ordering::SeqCst);
                let handle = PreStartParked
                    .spawn()
                    .named(&name)
                    .on_system(&sys)
                    .detached()
                    .await
                    .expect("the spawn is accepted");
                racers.push((killer, handle));
            }
            // The property is liveness: a kill whose backstop was never armed
            // leaves its actor parked in `pre_start` for good, so one generous
            // deadline per batch tells an armed backstop from a missing one
            // whatever the load. How long the grace lasts before the abort is
            // asserted where it can be measured without a race beside it.
            let deadline = Instant::now() + BOUND;
            for (killer, handle) in racers {
                let killed = killer.join().expect("the killer ends");
                let stopped = killed
                    && tokio::time::timeout_at(deadline, handle.wait_stopped())
                        .await
                        .is_ok();
                if !stopped {
                    not_aborted += 1;
                }
            }
        }
        assert_eq!(not_aborted, 0, "every killed actor reaches Stopped");
    });
}

#[test]
fn stop_and_kill_on_an_aborted_actor_are_refused() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("closed-after-abort")).unwrap();
        let name = uname("parked");
        let probe = Arc::new(Probe::default());
        let handle = probe
            .instance(0)
            .spawn()
            .named(&name)
            .on_system(&sys)
            .await
            .unwrap();
        handle.notify(ChildMsg::Park).await.unwrap();
        wait_until(BOUND, "the turn to park", || {
            probe.parked.load(Ordering::SeqCst)
        })
        .await;
        assert!(within(BOUND, "kill_by_id", sys.kill_by_id(handle.id()))
            .await
            .is_some());
        within(BOUND, "the actor to stop", handle.wait_stopped()).await;

        // The handle's lane has no receiver left: every request is refused.
        for reason in [StopReason::Kill, StopReason::Graceful] {
            let answer = handle.stop(reason.clone()).await;
            assert!(
                matches!(answer, Err(tokio_actors::SendError::Closed)),
                "stop({reason:?}) after an abort: {answer:?}"
            );
        }
        let answer = handle
            .stop_within(StopReason::Kill, Duration::from_millis(50))
            .await;
        assert!(
            matches!(answer, Err(tokio_actors::SendError::Closed)),
            "{answer:?}"
        );
        // The task's registration went with it.
        assert!(matches!(
            sys.stop(&name).await,
            Err(tokio_actors::SendError::NotFound)
        ));
        assert!(matches!(
            sys.kill(&name).await,
            Err(tokio_actors::SendError::NotFound)
        ));
    });
}

/// The largest duration `Instant::checked_add` accepts from now, less 500 us.
fn largest_addable() -> Duration {
    let base = Instant::now();
    let mut lo = Duration::ZERO;
    let mut hi = Duration::from_secs(u64::MAX / 2);
    while hi - lo > Duration::from_nanos(1) {
        let mid = lo + (hi - lo) / 2;
        if base.checked_add(mid).is_some() {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo.saturating_sub(Duration::from_micros(500))
}

#[test]
fn stop_within_kill_at_the_edge_of_range_does_not_panic() {
    common::run(0, async {
        let thirty_years = Duration::from_secs(86_400 * 365 * 30);
        for within_window in [largest_addable(), thirty_years, Duration::MAX] {
            let sys = ActorSystem::create(uname("edge-within")).unwrap();
            let probe = Arc::new(Probe::default());
            let handle = probe
                .instance(0)
                .spawn()
                .named(uname("cooperative"))
                .on_system(&sys)
                .await
                .unwrap();
            let outcome = within(
                BOUND,
                "stop_within(Kill) at the edge of range",
                handle.stop_within(StopReason::Kill, within_window),
            )
            .await
            .expect("accepted");
            assert_eq!(outcome, tokio_actors::StopOutcome::Killed);
        }
    });
}

/// A supervisor whose `on_started` starts a vetoing child, holds until
/// `release` is set (for at most ten seconds) and fails.
struct FailingInit {
    probe: Arc<Probe>,
    veto_name: String,
    release: Arc<AtomicBool>,
}

impl Actor for FailingInit {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let probe = self.probe.clone();
        ctx.spawn_child(move || probe.instance(always_vetoes()))
            .named(self.veto_name.clone())
            .shutdown(Shutdown::Timeout(Duration::from_secs(2)))
            .await?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.release.load(Ordering::SeqCst) && Instant::now() < deadline {
            sleep(Duration::from_millis(5)).await;
        }
        Err(ActorError::user("the initialization failed"))
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[test]
fn kill_during_a_failed_init_takes_the_kill_teardown() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("kill-failed-init")).unwrap();
        let probe = Arc::new(Probe::default());
        let veto_name = uname("veto");
        let name = uname("failing");
        let release = Arc::new(AtomicBool::new(false));
        let spawned = {
            let (sys, probe, veto_name, name, release) = (
                sys.clone(),
                probe.clone(),
                veto_name.clone(),
                name.clone(),
                release.clone(),
            );
            tokio::spawn(async move {
                FailingInit {
                    probe,
                    veto_name,
                    release,
                }
                .spawn()
                .named(name)
                .on_system(&sys)
                // Long enough that the init finishes before any abort.
                .kill_grace(Duration::from_secs(2))
                .supervisor()
                .await
            })
        };
        // The child is started inside `on_started`, which then holds: the
        // kill lands during the init, which fails only once it was accepted.
        let _veto = child_in(&sys, &veto_name).await;
        sys.kill(&name).await.expect("the kill is accepted");
        release.store(true, Ordering::SeqCst);

        // The init's error is not the final reason: the Kill accepted before
        // the seal is, and the awaited spawn reads it, as the plane does.
        let result = within(BOUND, "the awaited spawn", spawned).await.unwrap();
        assert!(
            matches!(result, Err(tokio_actors::SpawnError::Killed)),
            "the spawn reads Killed, got {:?}",
            result.map(|_| ())
        );
        wait_until(BOUND, "the child to end", || {
            probe.ended.load(Ordering::SeqCst)
        })
        .await;
        assert_eq!(
            probe.stopped_runs.load(Ordering::SeqCst),
            0,
            "the child ran no on_stopped: the Kill teardown took it"
        );
        assert_eq!(
            probe.pre_stops.load(Ordering::SeqCst),
            0,
            "the Kill teardown asks nothing of the child: no pre_stop ran"
        );
    });
}

/// A supervisor with a vetoing child (a 2 s window) whose stop is under way:
/// the child has run its first `pre_stop` (and vetoed it).
async fn supervisor_mid_teardown() -> (Arc<ActorSystem>, Rig, String, Arc<Probe>) {
    let sys = ActorSystem::create(uname("mid-teardown")).unwrap();
    let sup_name = uname("sup");
    let rig = supervisor_named(&sys, &sup_name, plain(), false, false).await;
    let probe = Arc::new(Probe::default());
    let name = uname("veto");
    rig.spawn_child(
        &name,
        &probe,
        Shutdown::Timeout(Duration::from_secs(2)),
        RestartType::Permanent,
        always_vetoes(),
    )
    .await;
    let _child = child_in(&sys, &name).await;
    rig.sup.stop(StopReason::Graceful).await.unwrap();
    wait_until(BOUND, "the child's first pre_stop", || {
        probe.pre_stops.load(Ordering::SeqCst) >= 1
    })
    .await;
    (sys, rig, sup_name, probe)
}

/// After the Kill the supervisor publishes `Stopped` within the default kill
/// grace, the wait after an abort and a margin, and the child's own window
/// (2 s) is cut: it ran no `on_stopped`, and its `pre_stop` never ran a
/// second time (the Kill teardown asks nothing of it).
async fn kill_ends_a_teardown_in_progress(
    rig: &Rig,
    probe: &Probe,
    pre_stops_before: usize,
    killed_at: Instant,
    paused: bool,
) {
    within(
        default_abort_bound(paused),
        "the supervisor to publish Stopped",
        rig.sup.wait_stopped(),
    )
    .await;
    assert!(killed_at.elapsed() < upper(paused, Duration::from_secs(2)));
    assert_eq!(
        probe.stopped_runs.load(Ordering::SeqCst),
        0,
        "the child's own window was cut by the Kill"
    );
    assert_eq!(
        probe.pre_stops.load(Ordering::SeqCst),
        pre_stops_before,
        "the Kill teardown asks nothing more of the child"
    );
}

#[test]
fn kill_is_accepted_while_children_are_being_stopped() {
    common::run(
        0,
        kill_is_accepted_while_children_are_being_stopped_body(false),
    );
}

#[test]
fn kill_is_accepted_while_children_are_being_stopped_paused() {
    common::run_paused(kill_is_accepted_while_children_are_being_stopped_body(true));
}

async fn kill_is_accepted_while_children_are_being_stopped_body(paused: bool) {
    let (_sys, rig, _sup_name, probe) = supervisor_mid_teardown().await;
    assert_ne!(rig.sup.status(), tokio_actors::ActorStatus::Stopped);
    let pre_stops_before = probe.pre_stops.load(Ordering::SeqCst);
    let killed_at = Instant::now();
    rig.sup
        .stop(StopReason::Kill)
        .await
        .expect("a Kill is accepted while the children are being stopped");
    kill_ends_a_teardown_in_progress(&rig, &probe, pre_stops_before, killed_at, paused).await;
}

#[test]
fn stop_within_kill_is_accepted_while_children_are_being_stopped() {
    common::run(
        0,
        stop_within_kill_is_accepted_while_children_are_being_stopped_body(false),
    );
}

#[test]
fn stop_within_kill_is_accepted_while_children_are_being_stopped_paused() {
    common::run_paused(stop_within_kill_is_accepted_while_children_are_being_stopped_body(true));
}

async fn stop_within_kill_is_accepted_while_children_are_being_stopped_body(paused: bool) {
    let (_sys, rig, _sup_name, probe) = supervisor_mid_teardown().await;
    let pre_stops_before = probe.pre_stops.load(Ordering::SeqCst);
    let killed_at = Instant::now();
    let outcome = within(
        BOUND,
        "stop_within(Kill)",
        rig.sup
            .stop_within(StopReason::Kill, Duration::from_millis(200)),
    )
    .await
    .expect("a Kill is accepted while the children are being stopped");
    assert_eq!(outcome, tokio_actors::StopOutcome::Killed);
    kill_ends_a_teardown_in_progress(&rig, &probe, pre_stops_before, killed_at, paused).await;
}

#[test]
fn graceful_stop_is_refused_while_children_are_being_stopped() {
    common::run(0, async {
        let (sys, rig, sup_name, _probe) = supervisor_mid_teardown().await;
        let direct = rig.sup.stop(StopReason::Graceful).await;
        assert!(
            matches!(direct, Err(tokio_actors::SendError::Closed)),
            "{direct:?}"
        );
        let by_name = sys.stop(&sup_name).await;
        assert!(
            matches!(by_name, Err(tokio_actors::SendError::Closed)),
            "{by_name:?}"
        );
        // Let it finish.
        rig.sup.stop(StopReason::Kill).await.unwrap();
        within(BOUND, "the supervisor to stop", rig.sup.wait_stopped()).await;
    });
}

/// Runs `body` with a root actor parked in `handle`, on a system of its own,
/// and with the bound a kill entry point must meet.
async fn parked_root_on(sys: &Arc<ActorSystem>) -> (String, ActorHandle<Probed>) {
    let name = uname("parked");
    let probe = Arc::new(Probe::default());
    let handle = probe
        .instance(0)
        .spawn()
        .named(&name)
        .on_system(sys)
        .await
        .unwrap();
    handle.notify(ChildMsg::Park).await.unwrap();
    wait_until(BOUND, "the turn to park", || {
        probe.parked.load(Ordering::SeqCst)
    })
    .await;
    (name, handle)
}

/// The margin a kill path has beyond the default kill grace and the wait
/// after an abort.
fn default_abort_bound(paused: bool) -> Duration {
    upper(
        paused,
        ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE + Duration::from_millis(500),
    )
}

#[test]
fn kill_by_name_aborts_a_turn_parked_at_an_await() {
    common::run(0, kill_by_name_aborts_a_turn_parked_at_an_await_body(false));
}

#[test]
fn kill_by_name_aborts_a_turn_parked_at_an_await_paused() {
    common::run_paused(kill_by_name_aborts_a_turn_parked_at_an_await_body(true));
}

async fn kill_by_name_aborts_a_turn_parked_at_an_await_body(paused: bool) {
    let sys = ActorSystem::create(uname("kill-by-name")).unwrap();
    // The kill returns at once: with a 30 s grace, a call that waited for
    // the abort would not return within the bound, and the target is
    // still running when it does.
    let far_name = uname("far-grace");
    let far_probe = Arc::new(Probe::default());
    let far = far_probe
        .instance(0)
        .spawn()
        .named(&far_name)
        .on_system(&sys)
        .kill_grace(Duration::from_secs(30))
        .await
        .unwrap();
    far.notify(ChildMsg::Park).await.unwrap();
    wait_until(BOUND, "the turn to park", || {
        far_probe.parked.load(Ordering::SeqCst)
    })
    .await;
    within(BOUND, "kill to return at once", sys.kill(&far_name))
        .await
        .expect("the kill is accepted");
    assert_ne!(
        far.status(),
        tokio_actors::ActorStatus::Stopped,
        "kill returns before the abort"
    );

    let (name, handle) = parked_root_on(&sys).await;
    sys.kill(&name).await.expect("the kill is accepted");
    within(
        default_abort_bound(paused),
        "the parked actor to be aborted",
        handle.wait_stopped(),
    )
    .await;
}

#[test]
fn stop_kill_aborts_a_turn_parked_at_an_await() {
    common::run(0, stop_kill_aborts_a_turn_parked_at_an_await_body(false));
}

#[test]
fn stop_kill_aborts_a_turn_parked_at_an_await_paused() {
    common::run_paused(stop_kill_aborts_a_turn_parked_at_an_await_body(true));
}

async fn stop_kill_aborts_a_turn_parked_at_an_await_body(paused: bool) {
    let sys = ActorSystem::create(uname("stop-kill")).unwrap();
    let (_name, handle) = parked_root_on(&sys).await;
    handle.stop(StopReason::Kill).await.expect("accepted");
    within(
        default_abort_bound(paused),
        "the parked actor to be aborted",
        handle.wait_stopped(),
    )
    .await;
}

#[test]
fn kill_by_id_backstop_survives_a_dropped_call() {
    common::run(0, kill_by_id_backstop_survives_a_dropped_call_body(false));
}

#[test]
fn kill_by_id_backstop_survives_a_dropped_call_paused() {
    common::run_paused(kill_by_id_backstop_survives_a_dropped_call_body(true));
}

async fn kill_by_id_backstop_survives_a_dropped_call_body(paused: bool) {
    let sys = ActorSystem::create(uname("kill-by-id-dropped")).unwrap();
    let (_name, handle) = parked_root_on(&sys).await;
    // The call is dropped at once, after it raised the Kill.
    let _ = timeout(Duration::ZERO, sys.kill_by_id(handle.id())).await;
    within(
        default_abort_bound(paused),
        "the parked actor to be aborted",
        handle.wait_stopped(),
    )
    .await;
}

#[test]
fn stop_within_kill_backstop_survives_a_dropped_call() {
    common::run(
        0,
        stop_within_kill_backstop_survives_a_dropped_call_body(false),
    );
}

#[test]
fn stop_within_kill_backstop_survives_a_dropped_call_paused() {
    common::run_paused(stop_within_kill_backstop_survives_a_dropped_call_body(true));
}

async fn stop_within_kill_backstop_survives_a_dropped_call_body(paused: bool) {
    let sys = ActorSystem::create(uname("within-dropped")).unwrap();
    let (_name, handle) = parked_root_on(&sys).await;
    let window = Duration::from_millis(150);
    let _ = timeout(Duration::ZERO, handle.stop_within(StopReason::Kill, window)).await;
    within(
        upper(
            paused,
            window + ASSUMED_ABORT_SETTLE + Duration::from_millis(500),
        ),
        "the parked actor to be aborted",
        handle.wait_stopped(),
    )
    .await;
}

#[test]
fn shutdown_kill_step_backstop_survives_a_dropped_shutdown() {
    common::run(
        0,
        shutdown_kill_step_backstop_survives_a_dropped_shutdown_body(false),
    );
}

#[test]
fn shutdown_kill_step_backstop_survives_a_dropped_shutdown_paused() {
    common::run_paused(shutdown_kill_step_backstop_survives_a_dropped_shutdown_body(true));
}

async fn shutdown_kill_step_backstop_survives_a_dropped_shutdown_body(paused: bool) {
    let sys = ActorSystem::create_with(
        uname("shutdown-dropped"),
        tokio_actors::SystemConfig {
            shutdown_policy: tokio_actors::ShutdownPolicy {
                timeout: Duration::from_secs(30),
                per_actor_timeout: Duration::from_millis(50),
            },
            ..tokio_actors::SystemConfig::default()
        },
    )
    .unwrap();
    let (_name, handle) = parked_root_on(&sys).await;
    // The shutdown raises its Kill step at 50 ms and is dropped at 80 ms.
    let _ = timeout(Duration::from_millis(80), sys.shutdown()).await;
    within(
        Duration::from_millis(50) + default_abort_bound(paused),
        "the parked actor to be aborted",
        handle.wait_stopped(),
    )
    .await;
}

/// Obeys only its parent: `pre_stop` accepts a `ParentRequest` and vetoes
/// anything else. A turn sets `in_turn` and lasts until `end_turn` is
/// notified.
struct ParentOnly {
    stopped: Arc<Mutex<Vec<StopReason>>>,
    in_turn: Arc<AtomicBool>,
    end_turn: Arc<tokio::sync::Notify>,
}

impl Actor for ParentOnly {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.in_turn.store(true, Ordering::SeqCst);
        self.end_turn.notified().await;
        Ok(())
    }

    async fn pre_stop(&mut self, reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        matches!(reason, StopReason::ParentRequest)
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.stopped.lock().unwrap().push(reason.clone());
        Ok(())
    }
}

async fn parent_request_then_graceful(
    shutdown: Shutdown,
) -> (Arc<Mutex<Vec<StopReason>>>, Duration) {
    let sys = ActorSystem::create(uname("parent-request-kept")).unwrap();
    let stopped: Arc<Mutex<Vec<StopReason>>> = Arc::new(Mutex::new(Vec::new()));
    let in_turn = Arc::new(AtomicBool::new(false));
    let end_turn = Arc::new(tokio::sync::Notify::new());
    let name = uname("obeys-parent");
    let (top, _events) = kill_top(&sys, &name, shutdown, ASSUMED_KILL_GRACE, false, {
        let (stopped, in_turn, end_turn) = (stopped.clone(), in_turn.clone(), end_turn.clone());
        move || ParentOnly {
            stopped: stopped.clone(),
            in_turn: in_turn.clone(),
            end_turn: end_turn.clone(),
        }
    })
    .await;
    let child = registered_as::<ParentOnly>(&sys, &name).await;
    // A turn in flight, held until both requests are on the child's lane, so
    // neither is read before the turn ends.
    child.notify(()).await.unwrap();
    wait_until(BOUND, "the child's turn to start", || {
        in_turn.load(Ordering::SeqCst)
    })
    .await;
    let started = Instant::now();
    // The parent's request first: the top closes its mailbox in the poll of
    // its teardown that raises the request on the child's lane. Then a third
    // party's graceful one.
    top.stop(StopReason::Graceful).await.unwrap();
    wait_until(BOUND, "the top to stop its child", || !top.is_alive()).await;
    child.stop(StopReason::Graceful).await.expect("accepted");
    end_turn.notify_one();
    within(BOUND, "the parent to publish Stopped", top.wait_stopped()).await;
    let took = started.elapsed();
    (stopped, took)
}

#[test]
fn parent_request_is_kept_over_a_later_graceful_for_a_child_that_obeys_only_its_parent() {
    common::run(
        0,
        parent_request_is_kept_over_a_later_graceful_for_a_child_that_obeys_only_its_parent_body(
            false,
        ),
    );
}

#[test]
fn parent_request_is_kept_over_a_later_graceful_for_a_child_that_obeys_only_its_parent_paused() {
    common::run_paused(
        parent_request_is_kept_over_a_later_graceful_for_a_child_that_obeys_only_its_parent_body(
            true,
        ),
    );
}

async fn parent_request_is_kept_over_a_later_graceful_for_a_child_that_obeys_only_its_parent_body(
    paused: bool,
) {
    // Under a 400 ms window the child runs on_stopped with the parent's
    // request: the later graceful one did not replace it.
    let (stopped, took) =
        parent_request_then_graceful(Shutdown::Timeout(Duration::from_millis(400))).await;
    assert!(
        matches!(
            stopped.lock().unwrap().as_slice(),
            [StopReason::ParentRequest]
        ),
        "the parent's request is the reason on_stopped received: {:?}",
        stopped.lock().unwrap()
    );
    assert!(
        took < upper(paused, Duration::from_millis(1500)),
        "{took:?}"
    );

    // Under `Infinity` the parent's stop completes.
    let (stopped, _took) = parent_request_then_graceful(Shutdown::Infinity).await;
    assert!(matches!(
        stopped.lock().unwrap().as_slice(),
        [StopReason::ParentRequest]
    ));
}

/// Records the reason it stopped with. A turn takes 300 ms.
struct SlowTurn {
    stopped: Arc<Mutex<Vec<StopReason>>>,
}

impl Actor for SlowTurn {
    type Message = ();
    type Response = ();

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        sleep(Duration::from_millis(300)).await;
        Ok(())
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        self.stopped.lock().unwrap().push(reason.clone());
        Ok(())
    }
}

#[test]
fn failure_is_kept_over_a_later_cancelled() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("failure-kept")).unwrap();
        let stopped: Arc<Mutex<Vec<StopReason>>> = Arc::new(Mutex::new(Vec::new()));
        let handle = SlowTurn {
            stopped: stopped.clone(),
        }
        .spawn()
        .named(uname("slow-turn"))
        .on_system(&sys)
        .await
        .unwrap();
        handle.notify(()).await.unwrap();
        sleep(Duration::from_millis(30)).await;
        handle
            .stop(StopReason::Failure(ActorError::user("fail")))
            .await
            .unwrap();
        handle.stop(StopReason::Cancelled).await.unwrap();
        within(BOUND, "the actor to stop", handle.wait_stopped()).await;
        let reasons = stopped.lock().unwrap();
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(
            matches!(reasons[0], StopReason::Failure(_)),
            "the pending failure is kept over the later cancellation: {reasons:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// Group rules: the whole retry, an attempt in flight, a Temporary member and a
// child spawned while a group restart pends
// ---------------------------------------------------------------------------

/// The factory calls of the probes in the order they happened, as `name#n`.
fn build_sequence(probes: &[(&str, &Arc<Probe>)]) -> Vec<String> {
    let mut calls: Vec<(usize, String)> = Vec::new();
    for (label, probe) in probes {
        for index in 0..probe.built.load(Ordering::SeqCst) {
            calls.push((probe.build_order(index), format!("{label}#{}", index + 1)));
        }
    }
    calls.sort();
    calls.into_iter().map(|(_, label)| label).collect()
}

/// Waits until the supervisor's loop has handled the end of `name`.
///
/// The loop serves its supervision events before its mailbox, so a handler
/// that runs after the end of a child was reported finds that end handled.
/// Reading `pending` alone is not enough: a child whose task ended reads as
/// restart-pending before its end is handled. So the end must read pending in
/// two handlers one after the other, each of which the loop ran only after it
/// had served every event ready at the time.
async fn wait_routed(rig: &Rig, name: &str) {
    let deadline = Instant::now() + BOUND;
    let mut seen = 0;
    loop {
        let lines = rig.script(vec![Op::Read(name.to_string())]).await;
        if lines[0].ends_with("pending=true") {
            seen += 1;
            if seen == 2 {
                return;
            }
        } else {
            seen = 0;
        }
        assert!(
            Instant::now() < deadline,
            "the end of {name} was never handled: {lines:?}"
        );
        sleep(Duration::from_millis(5)).await;
    }
}

/// Spawns `names` as permanent children with a two second window.
async fn spawn_members(rig: &Rig, members: &[(&String, &Arc<Probe>)]) {
    for (name, probe) in members {
        rig.spawn_child(
            name,
            probe,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
    }
}

#[test]
fn one_for_all_retry_restarts_the_whole_group() {
    common::run(0, async {
        // A budget of two covers the crash's charge and the retry's one
        // charge, and no third.
        let rig =
            supervisor(SupervisionConfig::one_for_all().max_restarts(2, Duration::from_secs(60)))
                .await;
        let (a, b, c) = (uname("a"), uname("b"), uname("c"));
        let (pa, pb, pc) = probes();
        spawn_members(&rig, &[(&a, &pa), (&b, &pb), (&c, &pc)]).await;
        wait_until(BOUND, "the children to finish starting", || {
            [&pa, &pb, &pc]
                .iter()
                .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        // The first restart of b fails its initialization.
        pb.fail_next.store(1, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "the retry to start the whole group again", || {
            pa.started.load(Ordering::SeqCst) == 3
                && pb.started.load(Ordering::SeqCst) == 2
                && pc.started.load(Ordering::SeqCst) == 2
        })
        .await;
        for name in [&a, &b, &c] {
            wait_for_alive(&rig, name).await;
        }
        sleep(QUIET).await;
        // The retry stopped the a the chain had started and started a, b and
        // c again from the first, in start order.
        assert_eq!(
            build_sequence(&[("a", &pa), ("b", &pb), ("c", &pc)]),
            ["a#1", "b#1", "c#1", "a#2", "b#2", "a#3", "b#3", "c#2"]
        );
        assert_eq!(pa.stopped_runs.load(Ordering::SeqCst), 2);
        assert!(
            pa.drop_order(1) < pa.build_order(2),
            "a#2 is gone before a#3 begins"
        );
        // The failed attempt of b was reported once as part of the restart.
        let b_events = rig.events_for(&b);
        assert_eq!(b_events.len(), 2, "{b_events:?}");
        assert!(b_events
            .iter()
            .all(|event| event.action == SupervisionAction::RestartInitiated));
        // The retry took one charge: the supervisor still runs, and one more
        // crash exhausts the budget of two.
        assert!(rig.sup.is_alive());
        crash(&child_handle(&c).await).await;
        within(
            BOUND,
            "the supervisor to stop on its exhausted budget",
            rig.sup.wait_stopped(),
        )
        .await;
        assert!(rig
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.action == SupervisionAction::BudgetExhausted));
    });
}

#[test]
fn rest_for_one_retry_restarts_from_the_failed_member() {
    common::run(0, async {
        let rig =
            supervisor(SupervisionConfig::rest_for_one().max_restarts(2, Duration::from_secs(60)))
                .await;
        let (x, y, z) = (uname("x"), uname("y"), uname("z"));
        let (px, py, pz) = probes();
        spawn_members(&rig, &[(&x, &px), (&y, &py), (&z, &pz)]).await;
        wait_until(BOUND, "the children to finish starting", || {
            [&px, &py, &pz]
                .iter()
                .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        py.fail_next.store(1, Ordering::SeqCst);
        crash(&child_handle(&x).await).await;
        wait_until(BOUND, "the retry to start y and z again", || {
            py.started.load(Ordering::SeqCst) == 2 && pz.started.load(Ordering::SeqCst) == 2
        })
        .await;
        for name in [&x, &y, &z] {
            wait_for_alive(&rig, name).await;
        }
        sleep(QUIET).await;
        // x#2 survives the retry; y and z are started again after it.
        assert_eq!(
            build_sequence(&[("x", &px), ("y", &py), ("z", &pz)]),
            ["x#1", "y#1", "z#1", "x#2", "y#2", "y#3", "z#2"]
        );
        assert_eq!(px.built.load(Ordering::SeqCst), 2);
        assert_eq!(
            px.drop_order(1),
            usize::MAX,
            "x#2 was not stopped by the retry"
        );
        assert_eq!(px.pre_stops.load(Ordering::SeqCst), 0);
        assert!(rig.sup.is_alive());
        crash(&child_handle(&z).await).await;
        within(
            BOUND,
            "the supervisor to stop on its exhausted budget",
            rig.sup.wait_stopped(),
        )
        .await;
    });
}

/// Three children `[a, x, f]` under `strategy`: `a` crashes, the group
/// starts `a` and `x` again and `f`'s restart initializes slowly and then
/// fails; `x`'s new instance crashes while `f` initializes, so its death is
/// reported while the group pends.
async fn retry_with_a_member_death_queued(
    strategy: SupervisionConfig,
    budget_is_exact: bool,
) -> String {
    let rig = supervisor(strategy).await;
    let (a, x, f) = (uname("a"), uname("x"), uname("f"));
    let (pa, px, pf) = probes();
    spawn_members(&rig, &[(&a, &pa), (&x, &px), (&f, &pf)]).await;
    wait_until(BOUND, "the children to finish starting", || {
        [&pa, &px, &pf]
            .iter()
            .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
    })
    .await;
    // f's restart is held in its initialization and then fails.
    pf.held.store(true, Ordering::SeqCst);
    pf.fail_next.store(1, Ordering::SeqCst);
    crash(&child_handle(&a).await).await;
    wait_until(BOUND, "f's restart to initialize", || {
        pf.built.load(Ordering::SeqCst) == 2 && px.started.load(Ordering::SeqCst) == 2
    })
    .await;
    // x's new instance crashes while the chain waits for f, and its end is
    // handled before f's restart is released.
    crash(&child_handle(&x).await).await;
    wait_routed(&rig, &x).await;
    pf.held.store(false, Ordering::SeqCst);
    // Every member runs again, and nothing is left waiting.
    wait_until(BOUND, "every member to be started again", || {
        pf.started.load(Ordering::SeqCst) >= 2 && px.started.load(Ordering::SeqCst) >= 3
    })
    .await;
    for name in [&a, &x, &f] {
        wait_for_alive(&rig, name).await;
    }
    sleep(QUIET).await;
    assert!(rig.sup.is_alive());
    if budget_is_exact {
        // The death of x was absorbed by the retry: its first instance was
        // stopped with the group and the crash of the second was reported
        // once, each as part of the restart and charged nothing, so the
        // budget of two that the crash of a and the retry spent holds.
        let events = rig.events_for(&x);
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(events
            .iter()
            .all(|event| event.action == SupervisionAction::RestartInitiated));
        assert!(matches!(events[1].reason, StopReason::Failure(_)));
        assert_eq!(px.built.load(Ordering::SeqCst), 3);
    }
    let kids = rig
        .script(vec![
            Op::Read(a.clone()),
            Op::Read(x.clone()),
            Op::Read(f.clone()),
        ])
        .await;
    assert!(
        kids.iter().all(|line| line == "alive=true pending=false"),
        "no member waits for a decision: {kids:?}"
    );
    x
}

/// Collects the places absorption took reports from, by child name. The
/// binary's one global subscriber hands this module every DEBUG event, and
/// only the absorption event carries a place.
#[cfg(feature = "tracing")]
mod absorption_log {
    use std::collections::HashMap;
    use std::fmt::Write as _;
    use std::sync::{Mutex, OnceLock};

    use tracing::field::{Field, Visit};
    use tracing::Event;

    static PLACES: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();

    fn places() -> &'static Mutex<HashMap<String, Vec<String>>> {
        PLACES.get_or_init(Mutex::default)
    }

    #[derive(Default)]
    struct Fields {
        child: String,
        place: String,
    }

    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "child" {
                let _ = write!(self.child, "{value:?}");
            }
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "waited_in" {
                self.place.push_str(value);
            }
        }
    }

    /// Records one DEBUG event: an absorption event carries the child and the
    /// place its report was taken from.
    pub fn record(event: &Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if !fields.place.is_empty() {
            places()
                .lock()
                .unwrap()
                .entry(fields.child)
                .or_default()
                .push(fields.place);
        }
    }

    /// Installs the binary's one global subscriber (see
    /// [`warnings::install`](super::warnings::install)), which hands every
    /// DEBUG event to [`record`].
    pub fn install() {
        let _ = super::warnings::install();
    }

    /// The places the reports of `child` were absorbed from, oldest first.
    pub fn of(child: &str) -> Vec<String> {
        places()
            .lock()
            .unwrap()
            .get(child)
            .cloned()
            .unwrap_or_default()
    }
}

#[test]
fn a_member_death_queued_behind_a_group_is_absorbed_when_the_group_retries() {
    #[cfg(feature = "tracing")]
    absorption_log::install();
    common::run(0, async {
        // OneForAll `[a, x, f]`: x's new instance crashes while the chain
        // waits for f, so x's death waits behind the group, in the deferred
        // reports. f's failure retries the group, whose evaluation absorbs
        // that report: it is delivered once as part of the restart, with the
        // reason the crash had, and charged nothing.
        let x = retry_with_a_member_death_queued(
            SupervisionConfig::one_for_all().max_restarts(2, Duration::from_secs(60)),
            true,
        )
        .await;
        #[cfg(feature = "tracing")]
        assert_eq!(
            absorption_log::of(&x),
            ["deferred behind the group"],
            "the report was taken from the deferred reports"
        );
        #[cfg(not(feature = "tracing"))]
        let _ = x;
    });
}

#[test]
fn retry_with_a_member_death_queued_completes_the_group() {
    common::run(0, async {
        // OneForAll: the retry covers x, so it absorbs the queued death, and
        // the budget of two (the crash of a, the retry) suffices.
        retry_with_a_member_death_queued(
            SupervisionConfig::one_for_all().max_restarts(2, Duration::from_secs(60)),
            true,
        )
        .await;
        // RestForOne: x is before the failed member and outside the retry, so
        // its death waits for the group and is decided afterwards (a third
        // charge); every member runs again.
        retry_with_a_member_death_queued(
            SupervisionConfig::rest_for_one().max_restarts(3, Duration::from_secs(60)),
            false,
        )
        .await;
    });
}

#[test]
fn retry_absorbs_the_death_of_a_child_spawned_during_a_rest_for_one_group() {
    // A child spawned during the pending group is after the failed member, so
    // the retry covers it and absorbs its death.
    common::run(
        0,
        retry_absorbs_the_death_of_a_child_spawned_during_the_group(),
    );
}

/// RestForOne `[x, y, z]` with a budget of two: `y` crashes and its restart is
/// held in its initialization; a child `n` is spawned meanwhile and crashes.
/// Its death waits behind the group. `y`'s restart then fails, and the retry
/// covers `n`: its death is reported once as part of the restart and charged
/// nothing, and `n` is started again.
async fn retry_absorbs_the_death_of_a_child_spawned_during_the_group() {
    let rig =
        supervisor(SupervisionConfig::rest_for_one().max_restarts(2, Duration::from_secs(60)))
            .await;
    let (x, y, z, n) = (uname("x"), uname("y"), uname("z"), uname("n"));
    let (px, py, pz) = probes();
    let pn = Arc::new(Probe::default());
    spawn_members(&rig, &[(&x, &px), (&y, &py), (&z, &pz)]).await;
    wait_until(BOUND, "the children to finish starting", || {
        [&px, &py, &pz]
            .iter()
            .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
    })
    .await;
    py.held.store(true, Ordering::SeqCst);
    py.fail_next.store(1, Ordering::SeqCst);
    crash(&child_handle(&y).await).await;
    wait_until(BOUND, "the chain to start y", || {
        py.built.load(Ordering::SeqCst) == 2
    })
    .await;
    spawn_new(&rig, &n, &pn).await;
    wait_until(BOUND, "n to start", || {
        pn.started.load(Ordering::SeqCst) == 1
    })
    .await;
    crash(&child_handle(&n).await).await;
    wait_routed(&rig, &n).await;
    py.held.store(false, Ordering::SeqCst);
    wait_until(BOUND, "the retry to start n again", || {
        py.started.load(Ordering::SeqCst) == 2 && pn.started.load(Ordering::SeqCst) == 2
    })
    .await;
    for name in [&x, &y, &z, &n] {
        wait_for_alive(&rig, name).await;
    }
    sleep(QUIET).await;
    let events = rig.events_for(&n);
    assert_eq!(events.len(), 1, "n's death is reported once: {events:?}");
    assert_eq!(events[0].action, SupervisionAction::RestartInitiated);
    assert!(matches!(events[0].reason, StopReason::Failure(_)));
    assert_eq!(pn.built.load(Ordering::SeqCst), 2);
    assert!(
        rig.sup.is_alive(),
        "the budget of two held: n's death was charged nothing"
    );
}

#[test]
fn supervisor_answers_a_send_between_two_chained_retries() {
    common::run(0, async {
        // Three charges: the crash and two retries.
        let rig =
            supervisor(SupervisionConfig::one_for_all().max_restarts(3, Duration::from_secs(60)))
                .await;
        let (a, b) = (uname("a"), uname("b"));
        let (pa, pb, _) = probes();
        spawn_members(&rig, &[(&a, &pa), (&b, &pb)]).await;
        wait_until(BOUND, "the children to finish starting", || {
            [&pa, &pb]
                .iter()
                .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        // b's first two restarts fail their initialization; the third waits
        // in `on_started` until the test releases it.
        pb.fail_first.store(2, Ordering::SeqCst);
        pb.held.store(true, Ordering::SeqCst);
        let held = ReleasesTheHeldInit(pb.clone());
        crash(&child_handle(&a).await).await;
        // After b's second failure the third initialization begins.
        wait_until(BOUND, "b's third restart to be built", || {
            pb.built.load(Ordering::SeqCst) == 4 && pb.drop_order(2) != usize::MAX
        })
        .await;
        let answered = within(
            Duration::from_secs(5),
            "the supervisor to answer a send between the retries",
            rig.sup.send(Cmd::IsAlive { name: a.clone() }),
        )
        .await;
        assert!(answered.is_ok());
        assert_eq!(
            pb.started.load(Ordering::SeqCst),
            1,
            "the answer came before b's third initialization completed"
        );
        drop(held);
        wait_until(BOUND, "the group to complete", || {
            pb.started.load(Ordering::SeqCst) == 2
        })
        .await;
        wait_for_alive(&rig, &a).await;
        wait_for_alive(&rig, &b).await;
        assert!(rig.sup.is_alive());
    });
}

#[test]
fn a_retry_with_no_occupant_starts_the_chain_at_once() {
    common::run(0, async {
        let rig =
            supervisor(SupervisionConfig::one_for_all().max_restarts(2, Duration::from_secs(60)))
                .await;
        let a = uname("a");
        let (pa, _, _) = probes();
        spawn_members(&rig, &[(&a, &pa)]).await;
        wait_until(BOUND, "a to finish starting", || {
            pa.started.load(Ordering::SeqCst) == 1
        })
        .await;
        // a's first restart fails its initialization: the retry finds no
        // member with an occupant, and starts the chain in the same step.
        pa.fail_next.store(1, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "a to be started again by the retry", || {
            pa.started.load(Ordering::SeqCst) == 2
        })
        .await;
        wait_for_alive(&rig, &a).await;
        assert_eq!(pa.built.load(Ordering::SeqCst), 3);
        assert!(rig.sup.is_alive());
    });
}

#[test]
fn a_report_pending_at_a_retry_exhaustion_is_delivered_once_with_supervisor_stopping() {
    common::run_current_thread_only(async {
        // One restart: b's crash takes it, and the retry's charge exhausts
        // the budget.
        let rig =
            supervisor(SupervisionConfig::one_for_all().max_restarts(1, Duration::from_secs(60)))
                .await;
        let (a, b) = (uname("a"), uname("b"));
        let (pa, pb, _) = probes();
        spawn_members(&rig, &[(&a, &pa), (&b, &pb)]).await;
        wait_until(BOUND, "the children to finish starting", || {
            [&pa, &pb]
                .iter()
                .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        // b's restart waits in its initialization until the test releases it,
        // and then fails.
        pb.held.store(true, Ordering::SeqCst);
        pb.fail_next.store(1, Ordering::SeqCst);
        crash(&child_handle(&b).await).await;
        wait_until(BOUND, "the chain to start b", || {
            pb.built.load(Ordering::SeqCst) == 2
        })
        .await;
        // a's new instance crashes while the chain waits for b: its report
        // waits behind the group.
        wait_until(BOUND, "a's new instance to run", || {
            pa.started.load(Ordering::SeqCst) == 2
        })
        .await;
        crash(&child_handle(&a).await).await;
        sleep(QUIET).await;
        pb.held.store(false, Ordering::SeqCst);
        within(
            BOUND,
            "the supervisor to stop on its exhausted budget",
            rig.sup.wait_stopped(),
        )
        .await;
        let lines = deliveries(&rig);
        let for_child = |name: &String| -> Vec<&String> {
            lines
                .iter()
                .filter(|line| line.starts_with(&format!("delivery:{name}:")))
                .collect()
        };
        // b's failed attempt: once, with the exhausted budget.
        let b_lines = for_child(&b);
        assert!(
            b_lines
                .last()
                .is_some_and(|line| line.contains("BudgetExhausted")),
            "{lines:?}"
        );
        // a's pending report: once, with the supervisor stopping, its status
        // reading Stopping, before on_stopped.
        let a_lines = for_child(&a);
        assert_eq!(
            a_lines.len(),
            2,
            "the first crash and the pending one: {lines:?}"
        );
        assert!(
            a_lines[1].contains("SupervisorStopping") && a_lines[1].ends_with("Stopping"),
            "{lines:?}"
        );
        assert_eq!(lines.last().map(String::as_str), Some("on_stopped"));
        // Nothing was restarted after the exhaustion.
        assert_eq!(pa.built.load(Ordering::SeqCst), 2);
        assert_eq!(pb.built.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn group_restart_cancels_a_member_attempt_that_has_not_spawned() {
    // One thread: nothing else runs between the statements of the handler, and
    // the supervisor reaches the group evaluation before the attempt it
    // started is polled for the first time.
    common::run_current_thread_only(async {
        let rig = supervisor(one_for_all()).await;
        let (a, c) = (uname("a"), uname("c"));
        let (pa, pc, _) = probes();
        spawn_members(&rig, &[(&a, &pa), (&c, &pc)]).await;
        wait_until(BOUND, "the children to finish starting", || {
            [&pa, &pc]
                .iter()
                .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        // One handler: a ends, then c is bounced, which starts c's restart
        // attempt as the handler's last act. The supervisor's loop meets a's
        // death with the attempt not yet polled.
        let lines = rig
            .script(vec![Op::CrashAndWait(a.clone()), Op::Stop(c.clone())])
            .await;
        assert_eq!(lines[1], "Ok(())", "{lines:?}");
        wait_until(BOUND, "the group to restart both children", || {
            pa.started.load(Ordering::SeqCst) == 2 && pc.started.load(Ordering::SeqCst) == 2
        })
        .await;
        wait_for_alive(&rig, &c).await;
        sleep(QUIET).await;
        // The attempt was cancelled before its factory ran: c was built by the
        // chain only, and nothing was delivered for the attempt.
        assert_eq!(
            pc.built.load(Ordering::SeqCst),
            2,
            "the cancelled attempt never calls the factory"
        );
        assert_eq!(pc.max_live.load(Ordering::SeqCst), 1);
        let events = rig.events_for(&c);
        assert_eq!(events.len(), 1, "only the bounce is reported: {events:?}");
        assert_eq!(events[0].action, SupervisionAction::RestartInitiated);
        assert!(rig.sup.is_alive());
    });
}

#[test]
fn group_stop_ordered_on_an_initializing_member_keeps_its_window_while_the_supervisor_is_busy() {
    common::run(0, async {
        let rig = supervisor(one_for_all()).await;
        let (a, w) = (uname("a"), uname("w"));
        let (pa, pw, _) = probes();
        rig.spawn_child(
            &a,
            &pa,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        // w vetoes every request, so only the end of its window stops it.
        let window = Duration::from_millis(400);
        rig.spawn_child(
            &w,
            &pw,
            Shutdown::Timeout(window),
            RestartType::Permanent,
            always_vetoes(),
        )
        .await;
        wait_until(BOUND, "the children to finish starting", || {
            [&pa, &pw]
                .iter()
                .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        // w is bounced, and its restart is held in its initialization.
        pw.held.store(true, Ordering::SeqCst);
        rig.ok(Cmd::Stop { name: w.clone() }).await;
        wait_until(BOUND, "w's restart to be built", || {
            pw.built.load(Ordering::SeqCst) == 2
        })
        .await;
        // a crashes: the group orders w's attempt, and the supervisor's
        // delivery of a's end keeps it busy for one second. The attempt is
        // released once the group is evaluated.
        *rig.knobs.busy_for.lock().unwrap() = Some((a.clone(), 1000));
        let crashed_at = Instant::now();
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "the group to be evaluated", || {
            rig.events_for(&a)
                .iter()
                .any(|event| matches!(event.reason, StopReason::Failure(_)))
        })
        .await;
        pw.held.store(false, Ordering::SeqCst);
        let mut dropped_at = None;
        wait_until(BOUND, "w's attempt to end", || {
            if dropped_at.is_none() && pw.drop_order(1) != usize::MAX {
                dropped_at = Some(Instant::now());
            }
            dropped_at.is_some()
        })
        .await;
        let ended_after = dropped_at.unwrap().duration_since(crashed_at);
        assert!(
            ended_after >= window,
            "the window runs from the order: {ended_after:?}"
        );
        assert!(
            ended_after < Duration::from_secs(5),
            "the attempt ended inside the bound: {ended_after:?}"
        );
        wait_until(BOUND, "the busy delivery to end", || {
            rig.knobs
                .order
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.starts_with("end:"))
        })
        .await;
        let end_stamp: usize = rig
            .knobs
            .order
            .lock()
            .unwrap()
            .iter()
            .find_map(|line| line.strip_prefix(&format!("end:{a}:")))
            .and_then(|stamp| stamp.parse().ok())
            .expect("the stamp of the end of a's delivery");
        let pre_stops = pw.pre_stop_order.lock().unwrap().clone();
        assert!(
            pre_stops.len() >= 2 && pre_stops[1] < end_stamp,
            "the attempt's incarnation was asked to stop before the supervisor's callback \
             returned: {pre_stops:?} against {end_stamp}"
        );
        assert!(
            pw.drop_order(1) < end_stamp,
            "the window ended before the callback returned"
        );
        wait_for_alive(&rig, &a).await;
        wait_for_alive(&rig, &w).await;
        assert_eq!(pw.built.load(Ordering::SeqCst), 3);
    });
}

/// OneForAll `[c, a, b]` or `[a, b, c]`: c's restart attempt, in flight when
/// `a` crashes, waits in its initialization for `a` to be alive.
async fn ordered_init_waits_on_a_sibling(
    order: [&str; 3],
    window: Duration,
    c_start_timeout: Option<Duration>,
    paused: bool,
) {
    let rig = supervisor(one_for_all()).await;
    let names: std::collections::HashMap<&str, String> =
        ["a", "b", "c"].iter().map(|k| (*k, uname(k))).collect();
    let probes: std::collections::HashMap<&str, Arc<Probe>> = ["a", "b", "c"]
        .iter()
        .map(|k| (*k, Arc::new(Probe::default())))
        .collect();
    for key in order {
        if key == "c" {
            let reply = rig
                .command(Cmd::SpawnWith {
                    name: Some(names[key].clone()),
                    probe: probes[key].clone(),
                    shutdown: Shutdown::Timeout(window),
                    restart: RestartType::Permanent,
                    vetoes: 0,
                    start_timeout: c_start_timeout,
                })
                .await;
            assert!(matches!(reply, Reply::Id(_)), "{reply:?}");
        } else {
            rig.spawn_child(
                &names[key],
                &probes[key],
                Shutdown::Timeout(Duration::from_secs(2)),
                RestartType::Permanent,
                0,
            )
            .await;
        }
    }
    wait_until(BOUND, "the children to finish starting", || {
        probes
            .values()
            .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
    })
    .await;
    // c is bounced, and its restart waits for a to be alive, which a stays
    // not while c's attempt holds the group. The restart is held in its
    // initialization until a has crashed and the group is evaluated, so it
    // meets a dead sibling.
    *probes["c"].wait_alive.lock().unwrap() = Some(names["a"].clone());
    probes["c"].held.store(true, Ordering::SeqCst);
    rig.ok(Cmd::Stop {
        name: names["c"].clone(),
    })
    .await;
    wait_until(BOUND, "c's restart to be built", || {
        probes["c"].built.load(Ordering::SeqCst) == 2
    })
    .await;
    let crashed_at = Instant::now();
    crash(&child_handle(&names["a"]).await).await;
    wait_until(BOUND, "the group to be evaluated", || {
        rig.events_for(&names["a"])
            .iter()
            .any(|event| matches!(event.reason, StopReason::Failure(_)))
    })
    .await;
    probes["c"].held.store(false, Ordering::SeqCst);
    wait_until(BOUND, &format!("the group to complete ({order:?})"), || {
        probes["a"].started.load(Ordering::SeqCst) == 2
            && probes["b"].started.load(Ordering::SeqCst) == 2
            && probes["c"].started.load(Ordering::SeqCst) == 2
    })
    .await;
    let took = crashed_at.elapsed();
    let ends_by = c_start_timeout.map_or(window, |timeout| timeout.min(window));
    assert!(
        took < upper(
            paused,
            ends_by + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE + Duration::from_secs(1)
        ),
        "the group completed inside the bound of c's window: {took:?}"
    );
    // The attempt waiting on a is not the c that runs afterwards.
    assert_eq!(probes["c"].built.load(Ordering::SeqCst), 3);
    for key in ["a", "b", "c"] {
        wait_for_alive(&rig, &names[key]).await;
    }
    if c_start_timeout.is_some() {
        let ends = rig.events_for(&names["c"]);
        assert!(
            ends.iter().any(|event| matches!(
                &event.reason,
                StopReason::Failure(ActorError::Spawn(tokio_actors::SpawnError::StartTimeout))
            ) && event.action == SupervisionAction::RestartInitiated),
            "the attempt reports the failed start: {ends:?}"
        );
    }
    assert!(rig.sup.is_alive());
}

#[test]
fn ordered_init_waiting_on_a_sibling_is_bounded_by_its_window() {
    common::run(
        0,
        ordered_init_waiting_on_a_sibling_is_bounded_by_its_window_body(false),
    );
}

#[test]
fn ordered_init_waiting_on_a_sibling_is_bounded_by_its_window_paused() {
    common::run_paused(ordered_init_waiting_on_a_sibling_is_bounded_by_its_window_body(true));
}

async fn ordered_init_waiting_on_a_sibling_is_bounded_by_its_window_body(paused: bool) {
    let window = Duration::from_millis(300);
    // c is started after the sibling it waits for.
    ordered_init_waits_on_a_sibling(["a", "b", "c"], window, None, paused).await;
    // c is started before the sibling it waits for: the core case, the
    // chain starts c first and a only after.
    ordered_init_waits_on_a_sibling(["c", "a", "b"], window, None, paused).await;
    // The same with a start timeout that ends the attempt before its
    // window does.
    ordered_init_waits_on_a_sibling(
        ["c", "a", "b"],
        Duration::from_secs(3),
        Some(Duration::from_millis(500)),
        paused,
    )
    .await;
}

/// What the supervisor reads of a child that a group removed: no such child.
async fn is_removed(rig: &Rig, name: &str) -> bool {
    rig.script(vec![Op::Read(name.to_string())]).await[0] == "no such child"
}

/// The awaited occupant's own end: OneForAll `[a, t, b]` with a
/// `Temporary` t; a crashes, the group stops t and b, restarts a and b,
/// and removes t, reported once as removed.
async fn temporary_member_awaited_end() {
    let rig = supervisor(one_for_all()).await;
    let (a, t, b) = (uname("a"), uname("t"), uname("b"));
    let (pa, pt, pb) = probes();
    rig.spawn_child(
        &a,
        &pa,
        Shutdown::Timeout(Duration::from_secs(2)),
        RestartType::Permanent,
        0,
    )
    .await;
    rig.spawn_child(
        &t,
        &pt,
        Shutdown::Timeout(Duration::from_secs(2)),
        RestartType::Temporary,
        0,
    )
    .await;
    rig.spawn_child(
        &b,
        &pb,
        Shutdown::Timeout(Duration::from_secs(2)),
        RestartType::Permanent,
        0,
    )
    .await;
    wait_until(BOUND, "the children to finish starting", || {
        [&pa, &pt, &pb]
            .iter()
            .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
    })
    .await;
    crash(&child_handle(&a).await).await;
    wait_until(BOUND, "a and b to be started again", || {
        pa.started.load(Ordering::SeqCst) == 2 && pb.started.load(Ordering::SeqCst) == 2
    })
    .await;
    wait_for_alive(&rig, &b).await;
    sleep(QUIET).await;
    removed_once(&rig, &t);
    assert_eq!(pt.built.load(Ordering::SeqCst), 1, "t is never restarted");
    assert!(is_removed(&rig, &t).await, "t's spec is removed");
    assert!(rig.sup.is_alive());
}

/// The owed evaluation of a member whose manual bounce met a failure:
/// t is `Temporary` and fails during the bounce of its stop, while a's
/// group joins that stop.
async fn temporary_member_owed_evaluation_fold() {
    let rig = supervisor(one_for_all()).await;
    let (a, t) = (uname("a"), uname("t"));
    let (pa, pt, _) = probes();
    rig.spawn_child(
        &a,
        &pa,
        Shutdown::Timeout(Duration::from_secs(2)),
        RestartType::Permanent,
        0,
    )
    .await;
    rig.spawn_child(
        &t,
        &pt,
        Shutdown::Timeout(Duration::from_secs(2)),
        RestartType::Temporary,
        0,
    )
    .await;
    wait_until(BOUND, "the children to finish starting", || {
        [&pa, &pt]
            .iter()
            .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
    })
    .await;
    let child_t = child_handle(&t).await;
    pt.turn_held.store(true, Ordering::SeqCst);
    child_t.notify(ChildMsg::LongFail).await.unwrap();
    wait_until(BOUND, "t's long turn to begin", || {
        pt.in_turn.load(Ordering::SeqCst)
    })
    .await;
    // t's stop is committed and abandoned: it waits for the turn.
    rig.ok(Cmd::StopAbandon {
        name: t.clone(),
        wait: Duration::from_millis(5),
    })
    .await;
    // a crashes: the group joins t's stop. Then t's turn ends with a failure.
    crash(&child_handle(&a).await).await;
    wait_until(BOUND, "the group to be evaluated", || {
        rig.events_for(&a)
            .iter()
            .any(|event| matches!(event.reason, StopReason::Failure(_)))
    })
    .await;
    pt.turn_held.store(false, Ordering::SeqCst);
    wait_until(BOUND, "a to be started again", || {
        pa.started.load(Ordering::SeqCst) == 2
    })
    .await;
    sleep(QUIET).await;
    removed_once(&rig, &t);
    assert_eq!(pt.built.load(Ordering::SeqCst), 1, "t is never restarted");
    assert!(is_removed(&rig, &t).await, "t's spec is removed");
    assert!(rig.sup.is_alive());
}

/// An absorbed member's queued report: RestForOne `[a, t, c]` with a
/// `Temporary` t. c's group pends while c restarts; a and t crash
/// meanwhile and wait behind it. a's evaluation covers t, whose owed
/// evaluation is absorbed: reported once as removed, spec removed.
async fn temporary_member_absorbed_queued_report() {
    let sys = ActorSystem::create(uname("temporary-queued")).unwrap();
    let rig = supervisor_on(
        &sys,
        SupervisionConfig::rest_for_one().max_restarts(2, Duration::from_secs(60)),
    )
    .await;
    let (a, t, c) = (uname("a"), uname("t"), uname("c"));
    let (pa, pt, pc) = probes();
    for (name, probe, restart) in [
        (&a, &pa, RestartType::Permanent),
        (&t, &pt, RestartType::Temporary),
        (&c, &pc, RestartType::Permanent),
    ] {
        rig.spawn_child(
            name,
            probe,
            Shutdown::Timeout(Duration::from_millis(200)),
            restart,
            0,
        )
        .await;
    }
    let (ha, ht, hc) = (
        child_in(&sys, &a).await,
        child_in(&sys, &t).await,
        child_in(&sys, &c).await,
    );
    wait_until(BOUND, "the children to finish starting", || {
        [&pa, &pt, &pc]
            .iter()
            .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
    })
    .await;
    // c's restart is held in its initialization while a and t crash.
    pc.held.store(true, Ordering::SeqCst);
    crash(&hc).await;
    wait_until(BOUND, "c's restart to be built", || {
        pc.built.load(Ordering::SeqCst) == 2
    })
    .await;
    crash(&ha).await;
    crash(&ht).await;
    wait_routed(&rig, &a).await;
    wait_routed(&rig, &t).await;
    pc.held.store(false, Ordering::SeqCst);
    wait_until(BOUND, "a to be started again", || {
        pa.started.load(Ordering::SeqCst) == 2
    })
    .await;
    sleep(QUIET * 2).await;
    removed_once(&rig, &t);
    assert_eq!(pt.built.load(Ordering::SeqCst), 1, "t is never restarted");
    assert!(is_removed(&rig, &t).await, "t's spec is removed");
    assert!(rig.sup.is_alive());
}

/// A joined manual stop: t is `Temporary`, and its `terminate_child` or
/// `stop_child` is abandoned and still in progress when b's group is
/// evaluated. The group joins that stop; t is not in the set the group
/// restarts, so its end is the removal of a `Temporary` member, not a restart.
/// t's stop is held until the group's evaluation is observed with t still
/// stopping.
async fn temporary_member_joined_manual_stop(terminate: bool) {
    let sys = ActorSystem::create(uname("temporary-joined")).unwrap();
    let rig = supervisor_on(&sys, one_for_all()).await;
    let (pt, pb) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
    let (t, b) = (uname("t"), uname("b"));
    rig.spawn_child(
        &t,
        &pt,
        Shutdown::Timeout(Duration::from_secs(2)),
        RestartType::Temporary,
        0,
    )
    .await;
    rig.spawn_child(
        &b,
        &pb,
        Shutdown::Timeout(Duration::from_millis(200)),
        RestartType::Permanent,
        0,
    )
    .await;
    wait_until(BOUND, "the children to finish starting", || {
        [&pt, &pb]
            .iter()
            .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
    })
    .await;
    let child_b = child_in(&sys, &b).await;
    // t's `pre_stop` is held, so its stop stays in progress.
    pt.stop_held.store(true, Ordering::SeqCst);
    if terminate {
        rig.ok(Cmd::TerminateAbandon {
            name: t.clone(),
            wait: Duration::ZERO,
        })
        .await;
    } else {
        rig.ok(Cmd::StopAbandon {
            name: t.clone(),
            wait: Duration::ZERO,
        })
        .await;
    }
    wait_until(BOUND, "t's stop to begin", || {
        pt.pre_stops.load(Ordering::SeqCst) == 1
    })
    .await;
    crash(&child_b).await;
    wait_until(BOUND, "the group to be evaluated", || {
        rig.events_for(&b)
            .iter()
            .any(|event| matches!(event.reason, StopReason::Failure(_)))
    })
    .await;
    // The group joined t's stop: t is still stopping, so it reads alive.
    let read = rig.script(vec![Op::Read(t.clone())]).await;
    assert_eq!(read[0], "alive=true pending=false", "t is still stopping");
    assert!(rig.events_for(&t).is_empty(), "t has not ended");
    pt.stop_held.store(false, Ordering::SeqCst);
    wait_until(BOUND, "t's report", || !rig.events_for(&t).is_empty()).await;
    wait_until(BOUND, "b to be started again", || {
        pb.built.load(Ordering::SeqCst) == 2
    })
    .await;
    sleep(QUIET).await;
    removed_once(&rig, &t);
    assert_eq!(pt.built.load(Ordering::SeqCst), 1, "t is never restarted");
    assert!(is_removed(&rig, &t).await, "t's spec is removed");
    assert!(rig.sup.is_alive());
}

/// An absorbed member's report still in the death plane: its snapshot
/// says removed, and the report delivers it once when it drains.
async fn temporary_member_absorbed_snapshot() {
    let sys = ActorSystem::create(uname("temporary-snapshot")).unwrap();
    let rig = slow_delivery_supervisor(
        &sys,
        SupervisionConfig::one_for_all().max_restarts(1, Duration::from_secs(60)),
        100,
    )
    .await;
    let (pa, pt) = (Arc::new(Probe::default()), Arc::new(Probe::default()));
    let (a, t) = (uname("a"), uname("t"));
    rig.spawn_child(
        &a,
        &pa,
        Shutdown::Timeout(Duration::from_millis(200)),
        RestartType::Permanent,
        0,
    )
    .await;
    rig.spawn_child(
        &t,
        &pt,
        Shutdown::Timeout(Duration::from_millis(200)),
        RestartType::Temporary,
        0,
    )
    .await;
    let _ = child_in(&sys, &a).await;
    let _ = child_in(&sys, &t).await;
    let lines = rig
        .script(vec![
            Op::CrashAndWait(a.clone()),
            Op::SlowFail(t.clone()),
            Op::UntilInTurn(pt.clone()),
            Op::Stop(t.clone()),
        ])
        .await;
    assert!(lines[3].contains("Ok"), "{lines:?}");
    wait_until(BOUND, "t's report", || !rig.events_for(&t).is_empty()).await;
    wait_until(BOUND, "a to be started again", || {
        pa.started.load(Ordering::SeqCst) == 2
    })
    .await;
    sleep(QUIET * 2).await;
    removed_once(&rig, &t);
    assert_eq!(pt.built.load(Ordering::SeqCst), 1, "t is never restarted");
    assert!(is_removed(&rig, &t).await, "t's spec is removed");
    assert!(rig.sup.is_alive());
}

#[test]
fn temporary_member_stopped_with_its_group_is_removed() {
    common::run(0, async {
        temporary_member_awaited_end().await;
        temporary_member_owed_evaluation_fold().await;
        temporary_member_absorbed_queued_report().await;
        temporary_member_absorbed_snapshot().await;
        temporary_member_joined_manual_stop(true).await;
        temporary_member_joined_manual_stop(false).await;
    });
}

/// Spawns a permanent child under the supervisor and returns once the call
/// answered.
async fn spawn_new(rig: &Rig, name: &str, probe: &Arc<Probe>) {
    rig.spawn_child(
        name,
        probe,
        Shutdown::Timeout(Duration::from_secs(2)),
        RestartType::Permanent,
        0,
    )
    .await;
}

#[test]
fn new_child_spawned_during_a_pending_group_is_not_a_member() {
    common::run(0, async {
        let rig = supervisor(one_for_all()).await;
        let (a, b, n) = (uname("a"), uname("b"), uname("n"));
        let (pa, pb, pn) = probes();
        spawn_members(&rig, &[(&a, &pa), (&b, &pb)]).await;
        wait_until(BOUND, "the children to finish starting", || {
            [&pa, &pb]
                .iter()
                .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        // b crashes; the group stops a, whose stop is held.
        pa.stop_held.store(true, Ordering::SeqCst);
        crash(&child_handle(&b).await).await;
        wait_until(BOUND, "a's stop to begin", || {
            pa.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        // A child spawned during the pending group starts at once.
        spawn_new(&rig, &n, &pn).await;
        wait_until(BOUND, "n to start", || {
            pn.started.load(Ordering::SeqCst) == 1
        })
        .await;
        pa.stop_held.store(false, Ordering::SeqCst);
        wait_until(BOUND, "the group to complete", || {
            pa.started.load(Ordering::SeqCst) == 2 && pb.started.load(Ordering::SeqCst) == 2
        })
        .await;
        wait_for_alive(&rig, &a).await;
        wait_for_alive(&rig, &b).await;
        sleep(QUIET).await;
        // n is no member: the group neither stopped it nor restarted it.
        assert_eq!(pn.built.load(Ordering::SeqCst), 1);
        assert_eq!(pn.pre_stops.load(Ordering::SeqCst), 0);
        assert!(is_alive(&rig, &n).await);
        assert!(rig.events_for(&n).is_empty());
        assert!(rig.sup.is_alive());
    });
}

#[test]
fn new_child_spawned_during_a_pending_group_joins_a_whole_group_retry() {
    common::run(0, async {
        let rig =
            supervisor(SupervisionConfig::one_for_all().max_restarts(2, Duration::from_secs(60)))
                .await;
        let (a, b, n) = (uname("a"), uname("b"), uname("n"));
        let (pa, pb, pn) = probes();
        spawn_members(&rig, &[(&a, &pa), (&b, &pb)]).await;
        wait_until(BOUND, "the children to finish starting", || {
            [&pa, &pb]
                .iter()
                .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        // b crashes; the chain restarts a, whose restart is held in its
        // initialization and then fails.
        pa.held.store(true, Ordering::SeqCst);
        pa.fail_next.store(1, Ordering::SeqCst);
        crash(&child_handle(&b).await).await;
        wait_until(BOUND, "the chain to start a", || {
            pa.built.load(Ordering::SeqCst) == 2
        })
        .await;
        // A child spawned meanwhile starts at once, and is no member of the
        // group that pends.
        spawn_new(&rig, &n, &pn).await;
        wait_until(BOUND, "n to start", || {
            pn.started.load(Ordering::SeqCst) == 1
        })
        .await;
        pa.held.store(false, Ordering::SeqCst);
        // a's failure retries the whole group, n included: it is stopped and
        // started again after a and b.
        wait_until(BOUND, "the retry to start the group again", || {
            pa.started.load(Ordering::SeqCst) == 2
                && pb.started.load(Ordering::SeqCst) == 2
                && pn.started.load(Ordering::SeqCst) == 2
        })
        .await;
        for name in [&a, &b, &n] {
            wait_for_alive(&rig, name).await;
        }
        sleep(QUIET).await;
        assert_eq!(pn.pre_stops.load(Ordering::SeqCst), 1);
        assert_eq!(
            build_sequence(&[("a", &pa), ("b", &pb), ("n", &pn)]),
            ["a#1", "b#1", "a#2", "n#1", "a#3", "b#2", "n#2"]
        );
        assert!(rig.sup.is_alive());
    });
}

#[test]
fn new_child_spawned_during_a_pending_rest_for_one_group_joins_its_retry() {
    common::run(0, async {
        let rig =
            supervisor(SupervisionConfig::rest_for_one().max_restarts(2, Duration::from_secs(60)))
                .await;
        let (x, y, z, n) = (uname("x"), uname("y"), uname("z"), uname("n"));
        let (px, py, pz) = probes();
        let pn = Arc::new(Probe::default());
        spawn_members(&rig, &[(&x, &px), (&y, &py), (&z, &pz)]).await;
        wait_until(BOUND, "the children to finish starting", || {
            [&px, &py, &pz]
                .iter()
                .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        // y crashes; its restart is held in its initialization and then fails.
        py.held.store(true, Ordering::SeqCst);
        py.fail_next.store(1, Ordering::SeqCst);
        crash(&child_handle(&y).await).await;
        wait_until(BOUND, "the chain to start y", || {
            py.built.load(Ordering::SeqCst) == 2
        })
        .await;
        spawn_new(&rig, &n, &pn).await;
        wait_until(BOUND, "n to start", || {
            pn.started.load(Ordering::SeqCst) == 1
        })
        .await;
        py.held.store(false, Ordering::SeqCst);
        // The retry stops and starts y, z and n again, in start order; x is
        // untouched.
        wait_until(BOUND, "the retry to start y, z and n again", || {
            py.started.load(Ordering::SeqCst) == 2
                && pz.started.load(Ordering::SeqCst) == 2
                && pn.started.load(Ordering::SeqCst) == 2
        })
        .await;
        for name in [&x, &y, &z, &n] {
            wait_for_alive(&rig, name).await;
        }
        sleep(QUIET).await;
        assert_eq!(
            build_sequence(&[("y", &py), ("z", &pz), ("n", &pn)]),
            ["y#1", "z#1", "y#2", "n#1", "y#3", "z#2", "n#2"]
        );
        assert_eq!(px.built.load(Ordering::SeqCst), 1);
        assert_eq!(px.pre_stops.load(Ordering::SeqCst), 0);
        assert!(rig.sup.is_alive());
    });
}

/// OneForAll `[a, b]` with a budget of one: b's restart attempt is held in
/// its initialization, and a crashes meanwhile: the group commits the attempt
/// and orders it to stop. `manual` makes the attempt the one of
/// `restart_child` after a `terminate_child`; otherwise it is the attempt of a
/// `stop_child` bounce. With `init_fails` its initialization fails while the
/// group's order is on its way to it; otherwise it initializes.
async fn attempt_met_by_a_group(manual: bool, init_fails: bool) {
    let rig =
        supervisor(SupervisionConfig::one_for_all().max_restarts(1, Duration::from_secs(60))).await;
    let (a, b) = (uname("a"), uname("b"));
    let (pa, pb, _) = probes();
    spawn_members(&rig, &[(&a, &pa), (&b, &pb)]).await;
    wait_until(BOUND, "the children to finish starting", || {
        [&pa, &pb]
            .iter()
            .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
    })
    .await;
    pb.held.store(true, Ordering::SeqCst);
    if init_fails {
        pb.fail_next.store(1, Ordering::SeqCst);
    }
    if manual {
        rig.ok(Cmd::Terminate { name: b.clone() }).await;
        rig.ok(Cmd::RestartChild { name: b.clone() }).await;
    } else {
        rig.ok(Cmd::Stop { name: b.clone() }).await;
    }
    wait_until(BOUND, "the attempt to be built", || {
        pb.built.load(Ordering::SeqCst) == 2
    })
    .await;
    crash(&child_handle(&a).await).await;
    wait_until(BOUND, "the group to be evaluated", || {
        rig.events_for(&a)
            .iter()
            .any(|event| matches!(event.reason, StopReason::Failure(_)))
    })
    .await;
    pb.held.store(false, Ordering::SeqCst);
    // A stopped attempt finished its initialization; a failed one did not.
    let started = if init_fails { 2 } else { 3 };
    wait_until(BOUND, "b to be started in its slot", || {
        pb.built.load(Ordering::SeqCst) == 3 && pb.started.load(Ordering::SeqCst) == started
    })
    .await;
    wait_for_alive(&rig, &a).await;
    wait_for_alive(&rig, &b).await;
    sleep(QUIET).await;
    // The terminate or the bounce reported b; the attempt is reported once, as
    // the restart it is part of, with the reason its incarnation ended with.
    let events = rig.events_for(&b);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(
        events[0].action,
        if manual {
            SupervisionAction::Removed
        } else {
            SupervisionAction::RestartInitiated
        }
    );
    assert_eq!(events[1].action, SupervisionAction::RestartInitiated);
    if init_fails {
        assert!(
            events[1].reason.to_string().contains("refused to start"),
            "{events:?}"
        );
    }
    // The attempt's end charged nothing: the budget of one that the crash of
    // a took still holds.
    assert!(rig.sup.is_alive());
    assert!(rig
        .events
        .lock()
        .unwrap()
        .iter()
        .all(|event| event.action != SupervisionAction::BudgetExhausted));
}

#[test]
fn bounce_restart_attempt_stopped_by_a_group_is_reported_once() {
    common::run(0, attempt_met_by_a_group(false, false));
}

#[test]
fn manual_restart_attempt_that_initialized_is_stopped_by_a_group_and_reported_once() {
    common::run(0, attempt_met_by_a_group(true, false));
}

#[test]
fn manual_restart_attempt_stopped_by_a_group_is_reported_once() {
    // The attempt of `restart_child` has the manual origin: a group that
    // committed it restarts the member in its slot all the same.
    common::run(0, attempt_met_by_a_group(true, true));
}

#[test]
fn parent_teardown_asks_an_ordered_restart_attempt_once_more_after_its_init() {
    common::run(0, async {
        let rig = supervisor(one_for_all()).await;
        let (a, b) = (uname("a"), uname("b"));
        let (pa, pb, _) = probes();
        // b's first instance accepts any stop; its second, the manual
        // attempt's, vetoes the first request it gets after the one the
        // first instance counted, and accepts the next.
        pb.veto_script.lock().unwrap().extend([0, 2]);
        rig.spawn_child(
            &a,
            &pa,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        rig.spawn_child(&b, &pb, Shutdown::Infinity, RestartType::Permanent, 0)
            .await;
        wait_until(BOUND, "the children to finish starting", || {
            [&pa, &pb]
                .iter()
                .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        // b is terminated (it accepts) and started again by hand: its attempt
        // is held in its initialization.
        rig.ok(Cmd::Terminate { name: b.clone() }).await;
        assert_eq!(pb.pre_stops.load(Ordering::SeqCst), 1);
        pb.held.store(true, Ordering::SeqCst);
        rig.ok(Cmd::RestartChild { name: b.clone() }).await;
        wait_until(BOUND, "the manual attempt to be built", || {
            pb.built.load(Ordering::SeqCst) == 2
        })
        .await;
        // a crashes: the group commits b's attempt and orders it. Once the
        // group is evaluated the attempt is released: it acknowledges, runs
        // its loop and vetoes the order.
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "the group to be evaluated", || {
            rig.events_for(&a)
                .iter()
                .any(|event| matches!(event.reason, StopReason::Failure(_)))
        })
        .await;
        pb.held.store(false, Ordering::SeqCst);
        wait_until(BOUND, "the attempt to veto the group's order", || {
            pb.pre_stops.load(Ordering::SeqCst) == 2
        })
        .await;
        // The supervisor stops: its teardown asks the attempt once more.
        let stopped = Instant::now();
        rig.sup
            .stop(StopReason::Graceful)
            .await
            .expect("the stop is accepted");
        within(
            Duration::from_secs(5),
            "the supervisor to stop",
            rig.sup.wait_stopped(),
        )
        .await;
        assert_eq!(
            pb.pre_stops.load(Ordering::SeqCst),
            3,
            "the attempt's instance was asked twice: by the group and by the teardown"
        );
        assert!(stopped.elapsed() < Duration::from_secs(5));
    });
}

#[test]
fn group_restart_cancels_a_member_attempt_that_waits_for_its_predecessor_subtree() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("cancel-wait")).unwrap();
        let rig = supervisor_on(&sys, one_for_all()).await;
        let (a, p, w) = (uname("a"), uname("p"), uname("w"));
        let (pa, pw, _) = probes();
        rig.spawn_child(
            &a,
            &pa,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        // P is a supervisor child with a short window; its worker W sits in a
        // turn that never reaches a boundary, which P's stop does not wait for
        // and which ends only when W's kill grace of two seconds has passed.
        let p_built = Arc::new(AtomicUsize::new(0));
        rig.ok(Cmd::SpawnSupervisorChild {
            name: p.clone(),
            shutdown: Shutdown::Timeout(Duration::from_millis(100)),
            restart: RestartType::Permanent,
            config: plain(),
            built: p_built.clone(),
            events: Arc::new(Mutex::new(Vec::new())),
            knobs: Arc::new(Knobs::default()),
        })
        .await;
        let inner = sys
            .get::<Sup>(&p)
            .expect("the child supervisor is registered");
        let spawned = inner
            .send(Cmd::SpawnWithGrace {
                name: w.clone(),
                probe: pw.clone(),
                shutdown: Shutdown::Timeout(Duration::from_secs(5)),
                kill_grace: Duration::from_secs(2),
            })
            .await
            .expect("the child supervisor answers");
        assert!(matches!(spawned, Reply::Done(Ok(()))), "{spawned:?}");
        wait_until(BOUND, "a to finish starting", || {
            pa.started.load(Ordering::SeqCst) == 1
        })
        .await;
        let child_w = child_in(&sys, &w).await;
        child_w
            .notify(ChildMsg::Park)
            .await
            .expect("start the parked turn");
        wait_until(BOUND, "W's turn to park", || {
            pw.parked.load(Ordering::SeqCst)
        })
        .await;
        // P is bounced: its window passes and it is aborted with W's turn
        // still running, so P's restart attempt waits for W's subtree.
        rig.ok(Cmd::Stop { name: p.clone() }).await;
        assert_eq!(p_built.load(Ordering::SeqCst), 1, "the attempt waits");
        assert!(!pw.ended.load(Ordering::SeqCst), "W's turn still runs");
        // a crashes while P's attempt waits: the group cancels the attempt at
        // once, by the cancel flag or by the stop queue's order in its turn
        // (this test does not tell the two apart; the cancel flag alone is
        // pinned by the tests named in the module table), so the group does not
        // wait for W and the chain restarts a while W's turn still runs.
        crash(&child_in(&sys, &a).await).await;
        wait_until(BOUND, "the chain to restart a", || {
            pa.built.load(Ordering::SeqCst) == 2
        })
        .await;
        assert!(
            !pw.ended.load(Ordering::SeqCst),
            "a was restarted before W's turn ended"
        );
        assert_eq!(p_built.load(Ordering::SeqCst), 1, "the factory did not run");
        // Once W ends, the chain starts a and then P in start order.
        wait_until(BOUND, "the chain to start P", || {
            p_built.load(Ordering::SeqCst) == 2
        })
        .await;
        assert_eq!(pa.built.load(Ordering::SeqCst), 2, "a was started before P");
        assert!(pw.ended.load(Ordering::SeqCst));
        wait_for_alive(&rig, &a).await;
        wait_for_alive(&rig, &p).await;
        sleep(QUIET).await;
        // The cancelled attempt delivered nothing: only the bounce is
        // reported for P.
        let events = rig.events_for(&p);
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(rig.sup.is_alive());
    });
}

/// OneForAll: a's restart, the chain's front, is held in its initialization
/// and then fails, so the group is laid out again by a retry. A child
/// spawned while the group pends joins that retry; here it is bounced first,
/// so the retry commits its restart attempt in flight.
mod a_retry_commits_an_attempt_in_flight {
    use super::*;

    /// The attempt finished building its instance and is held in that
    /// instance's initialization: the retry orders it, the instance stops
    /// once its initialization is released, and the attempt is reported once,
    /// as the restart it is part of. The chain then starts a and n again.
    #[test]
    fn a_spawned_attempt_is_stopped() {
        common::run(0, async {
            let rig = supervisor(one_for_all()).await;
            let (a, n) = (uname("a"), uname("n"));
            let (pa, pn, _) = probes();
            spawn_members(&rig, &[(&a, &pa)]).await;
            wait_until(BOUND, "a to finish starting", || {
                pa.started.load(Ordering::SeqCst) == 1
            })
            .await;
            pa.held.store(true, Ordering::SeqCst);
            pa.fail_next.store(1, Ordering::SeqCst);
            crash(&child_handle(&a).await).await;
            wait_until(BOUND, "the chain to start a", || {
                pa.built.load(Ordering::SeqCst) == 2
            })
            .await;
            spawn_new(&rig, &n, &pn).await;
            wait_until(BOUND, "n to start", || {
                pn.started.load(Ordering::SeqCst) == 1
            })
            .await;
            // n is bounced: its attempt builds its instance, held in its
            // initialization.
            pn.held.store(true, Ordering::SeqCst);
            rig.ok(Cmd::Stop { name: n.clone() }).await;
            wait_until(BOUND, "n's attempt to build its instance", || {
                pn.built.load(Ordering::SeqCst) == 2
            })
            .await;
            // a's start fails: the retry commits n's attempt and orders it.
            pa.held.store(false, Ordering::SeqCst);
            wait_until(BOUND, "a's failed start to be reported", || {
                rig.events_for(&a)
                    .iter()
                    .any(|event| event.reason.to_string().contains("refused to start"))
            })
            .await;
            pn.held.store(false, Ordering::SeqCst);
            wait_until(BOUND, "the retry's chain to start a and n", || {
                pa.started.load(Ordering::SeqCst) == 2 && pn.built.load(Ordering::SeqCst) == 3
            })
            .await;
            wait_for_alive(&rig, &a).await;
            wait_for_alive(&rig, &n).await;
            sleep(QUIET).await;
            assert_eq!(
                build_sequence(&[("a", &pa), ("n", &pn)]),
                ["a#1", "a#2", "n#1", "n#2", "a#3", "n#3"]
            );
            assert_eq!(
                pn.started.load(Ordering::SeqCst),
                3,
                "the ordered instance finished its initialization, then stopped"
            );
            // The bounce reported n; the attempt is reported once, as the
            // restart it is part of.
            let events = rig.events_for(&n);
            assert_eq!(events.len(), 2, "{events:?}");
            assert!(events
                .iter()
                .all(|event| event.action == SupervisionAction::RestartInitiated));
            assert!(rig.sup.is_alive());
        });
    }

    /// The attempt still waits for the subtree of n's previous incarnation (a
    /// supervisor child whose worker outlives it): the retry cancels it at
    /// once, so its factory never runs, nothing is reported for it, and the
    /// retry's chain starts a before the worker ends; n is started again once
    /// the worker has ended.
    #[test]
    fn a_waiting_attempt_is_cancelled() {
        common::run(0, async {
            let sys = ActorSystem::create(uname("retry-cancel-wait")).unwrap();
            let rig = supervisor_on(&sys, one_for_all()).await;
            let (a, p, w) = (uname("a"), uname("p"), uname("w"));
            let (pa, pw, _) = probes();
            rig.spawn_child(
                &a,
                &pa,
                Shutdown::Timeout(Duration::from_secs(2)),
                RestartType::Permanent,
                0,
            )
            .await;
            wait_until(BOUND, "a to finish starting", || {
                pa.started.load(Ordering::SeqCst) == 1
            })
            .await;
            pa.held.store(true, Ordering::SeqCst);
            pa.fail_next.store(1, Ordering::SeqCst);
            crash(&child_in(&sys, &a).await).await;
            wait_until(BOUND, "the chain to start a", || {
                pa.built.load(Ordering::SeqCst) == 2
            })
            .await;
            // P is spawned while the group pends; its worker W sits in a turn
            // that never reaches a boundary until the test releases it, which
            // P's stop does not wait for. W's kill grace of a minute keeps the
            // abort out of the way.
            let p_built = Arc::new(AtomicUsize::new(0));
            rig.ok(Cmd::SpawnSupervisorChild {
                name: p.clone(),
                shutdown: Shutdown::Timeout(Duration::from_millis(100)),
                restart: RestartType::Permanent,
                config: plain(),
                built: p_built.clone(),
                events: Arc::new(Mutex::new(Vec::new())),
                knobs: Arc::new(Knobs::default()),
            })
            .await;
            let inner = sys
                .get::<Sup>(&p)
                .expect("the child supervisor is registered");
            let spawned = inner
                .send(Cmd::SpawnWithGrace {
                    name: w.clone(),
                    probe: pw.clone(),
                    shutdown: Shutdown::Timeout(Duration::from_secs(5)),
                    kill_grace: Duration::from_secs(60),
                })
                .await
                .expect("the child supervisor answers");
            assert!(matches!(spawned, Reply::Done(Ok(()))), "{spawned:?}");
            let child_w = child_in(&sys, &w).await;
            pw.turn_held.store(true, Ordering::SeqCst);
            child_w
                .notify(ChildMsg::LongFail)
                .await
                .expect("start the held turn");
            wait_until(BOUND, "W's turn to begin", || {
                pw.in_turn.load(Ordering::SeqCst)
            })
            .await;
            // P is bounced: its window passes and it is aborted with W's turn
            // still running, so P's restart attempt waits for W's subtree.
            rig.ok(Cmd::Stop { name: p.clone() }).await;
            assert_eq!(p_built.load(Ordering::SeqCst), 1, "the attempt waits");
            // a's start fails: the retry commits P's waiting attempt and
            // cancels it, so the retry's chain starts a while W still runs.
            pa.held.store(false, Ordering::SeqCst);
            wait_until(BOUND, "the retry's chain to start a", || {
                pa.started.load(Ordering::SeqCst) == 2
            })
            .await;
            assert!(
                !pw.ended.load(Ordering::SeqCst),
                "a was started again while W's turn is held"
            );
            assert_eq!(p_built.load(Ordering::SeqCst), 1, "the factory did not run");
            // Once W's turn is released and W ends, the chain starts P.
            pw.turn_held.store(false, Ordering::SeqCst);
            wait_until(BOUND, "the chain to start P", || {
                p_built.load(Ordering::SeqCst) == 2
            })
            .await;
            assert!(pw.ended.load(Ordering::SeqCst));
            wait_for_alive(&rig, &a).await;
            wait_for_alive(&rig, &p).await;
            sleep(QUIET).await;
            // The cancelled attempt delivered nothing: only the bounce is
            // reported for P.
            let events = rig.events_for(&p);
            assert_eq!(events.len(), 1, "{events:?}");
            assert!(rig.sup.is_alive());
        });
    }
}

#[test]
fn a_member_released_during_a_group_stays_down_through_its_retry() {
    common::run(0, async {
        // OneForAll `[a, b, c]` with a budget of three: a crashes, the group
        // stops b and c, and c's stop is held. c is released by
        // `terminate_child` while the group waits for it. Then b's restart
        // fails, and the retry lays the group out again: c stays down.
        let rig =
            supervisor(SupervisionConfig::one_for_all().max_restarts(3, Duration::from_secs(60)))
                .await;
        let (a, b, c) = (uname("a"), uname("b"), uname("c"));
        let (pa, pb, pc) = probes();
        spawn_members(&rig, &[(&a, &pa), (&b, &pb), (&c, &pc)]).await;
        wait_until(BOUND, "the children to finish starting", || {
            [&pa, &pb, &pc]
                .iter()
                .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        pc.stop_held.store(true, Ordering::SeqCst);
        pb.fail_next.store(1, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "c's stop to begin", || {
            pc.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        rig.ok(Cmd::TerminateAbandon {
            name: c.clone(),
            wait: Duration::ZERO,
        })
        .await;
        pc.stop_held.store(false, Ordering::SeqCst);
        wait_until(BOUND, "the group to complete after the retry", || {
            pa.started.load(Ordering::SeqCst) == 3 && pb.started.load(Ordering::SeqCst) == 2
        })
        .await;
        wait_for_alive(&rig, &a).await;
        wait_for_alive(&rig, &b).await;
        sleep(QUIET).await;
        // c was released: reported removed once, never built again, its spec
        // kept.
        assert_eq!(pc.built.load(Ordering::SeqCst), 1, "c is never built again");
        removed_once(&rig, &c);
        let read = rig.script(vec![Op::Read(c.clone())]).await;
        assert_eq!(read[0], "alive=false pending=false", "{read:?}");
        assert!(rig.sup.is_alive());
    });
}

// ---------------------------------------------------------------------------
// The stop queue: a group stops its members one at a time, newest first, and
// a pool stops its children all at once
// ---------------------------------------------------------------------------

/// Spawns `names` as permanent children with a two second window, and waits
/// for each to finish its first start.
async fn start_members(rig: &Rig, members: &[(&String, &Arc<Probe>)]) {
    spawn_members(rig, members).await;
    wait_until(BOUND, "the children to finish starting", || {
        members
            .iter()
            .all(|(_, probe)| probe.started.load(Ordering::SeqCst) == 1)
    })
    .await;
}

/// Waits until every probe was built and started this many times.
async fn until_started(probes: &[&Arc<Probe>], times: usize) {
    wait_until(BOUND, "the group to start its members again", || {
        probes.iter().all(|probe| {
            probe.built.load(Ordering::SeqCst) == times
                && probe.started.load(Ordering::SeqCst) == times
        })
    })
    .await;
}

/// The place in the global order of the `index`th call of the probe's
/// `pre_stop`.
fn pre_stop_at(probe: &Arc<Probe>, index: usize) -> usize {
    probe.pre_stop_order.lock().unwrap()[index]
}

/// Every event the supervisor delivered for `name` reads as the group's own
/// restart of it.
fn restart_initiated_once(rig: &Rig, name: &str) {
    let events = rig.events_for(name);
    assert_eq!(events.len(), 1, "exactly one report: {events:?}");
    assert_eq!(
        events[0].action,
        SupervisionAction::RestartInitiated,
        "{events:?}"
    );
}

#[test]
fn group_members_stop_one_at_a_time_in_reverse_start_order() {
    common::run(0, async {
        let rig = supervisor(one_for_all()).await;
        // `t` starts first and crashes, so the group stops `a`, `b` and `c`.
        let (t, pt) = (uname("t"), Arc::new(Probe::default()));
        let (a, b, c) = (uname("a"), uname("b"), uname("c"));
        let (pa, pb, pc) = probes();
        start_members(&rig, &[(&t, &pt), (&a, &pa), (&b, &pb), (&c, &pc)]).await;
        for probe in [&pa, &pb, &pc] {
            probe.pre_stop_sleep_ms.store(100, Ordering::SeqCst);
        }
        crash(&child_handle(&t).await).await;
        until_started(&[&pt, &pa, &pb, &pc], 2).await;
        // Each member is gone before the next one is asked to stop.
        assert!(
            pc.drop_order(0) < pre_stop_at(&pb, 0),
            "c was dropped before b was asked to stop"
        );
        assert!(
            pb.drop_order(0) < pre_stop_at(&pa, 0),
            "b was dropped before a was asked to stop"
        );
        assert!(rig.sup.is_alive());
    });
}

#[test]
fn a_queued_member_that_crashes_during_a_sequential_group_stop_folds_into_the_group() {
    common::run(0, async {
        // One restart per minute: a second charge would end the supervisor.
        let rig =
            supervisor(SupervisionConfig::one_for_all().max_restarts(1, Duration::from_secs(60)))
                .await;
        let (a, b, c, d) = (uname("a"), uname("b"), uname("c"), uname("d"));
        let (pa, pb, pc) = probes();
        let pd = Arc::new(Probe::default());
        start_members(&rig, &[(&a, &pa), (&b, &pb), (&c, &pc), (&d, &pd)]).await;
        // The queue is [d, c, b]; d's stop waits for the test.
        pd.pre_stop_gate.store(true, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        wait_until(
            BOUND,
            "d, the head of the queue, to be asked to stop",
            || pd.pre_stops.load(Ordering::SeqCst) == 1,
        )
        .await;
        assert_eq!(pc.pre_stops.load(Ordering::SeqCst), 0, "c waits behind d");
        assert_eq!(pb.pre_stops.load(Ordering::SeqCst), 0, "b waits behind c");
        // c, queued behind d, crashes before the queue reaches it.
        crash(&child_handle(&c).await).await;
        pd.pre_stop_gate.store(false, Ordering::SeqCst);
        until_started(&[&pa, &pb, &pc, &pd], 2).await;
        sleep(QUIET).await;
        assert!(
            rig.sup.is_alive(),
            "the crash of a queued member is part of the group: one charge"
        );
        assert_eq!(
            pc.pre_stops.load(Ordering::SeqCst),
            0,
            "c had ended: the queue never asked it to stop"
        );
        restart_initiated_once(&rig, &c);
        assert!(matches!(
            rig.events_for(&c)[0].reason,
            StopReason::Failure(_)
        ));
        for probe in [&pa, &pb, &pc, &pd] {
            assert_eq!(probe.built.load(Ordering::SeqCst), 2, "one group restart");
        }
        // b is a committed member: the queue stopped it before the chain
        // began, so it was gone before the chain built a again.
        assert_eq!(pb.pre_stops.load(Ordering::SeqCst), 1);
        assert!(
            pb.drop_order(0) < pa.build_order(1),
            "b was stopped by the group, not by the chain"
        );
        assert!(rig
            .events
            .lock()
            .unwrap()
            .iter()
            .all(|event| event.action != SupervisionAction::BudgetExhausted));
    });
}

#[test]
fn members_already_stopping_keep_stopping_during_a_sequential_group_stop() {
    common::run(
        0,
        members_already_stopping_keep_stopping_during_a_sequential_group_stop_body(false),
    );
}

#[test]
fn members_already_stopping_keep_stopping_during_a_sequential_group_stop_paused() {
    common::run_paused(
        members_already_stopping_keep_stopping_during_a_sequential_group_stop_body(true),
    );
}

async fn members_already_stopping_keep_stopping_during_a_sequential_group_stop_body(paused: bool) {
    let rig = supervisor(one_for_all()).await;
    let (a, b, c) = (uname("a"), uname("b"), uname("c"));
    let (pa, pb, pc) = probes();
    start_members(&rig, &[(&a, &pa), (&b, &pb), (&c, &pc)]).await;
    // b is stopping through a bounce whose caller gave up: its `pre_stop`
    // waits for the test.
    pb.pre_stop_gate.store(true, Ordering::SeqCst);
    rig.ok(Cmd::StopAbandon {
        name: b.clone(),
        wait: Duration::ZERO,
    })
    .await;
    wait_until(BOUND, "b to be asked to stop", || {
        pb.pre_stops.load(Ordering::SeqCst) == 1
    })
    .await;
    // a crashes: the queue is [c, b], and b is not its head.
    crash(&child_handle(&a).await).await;
    wait_until(BOUND, "c to end", || pc.drop_order(0) != usize::MAX).await;
    assert_eq!(
        pb.pre_stops_returned.load(Ordering::SeqCst),
        0,
        "b is still stopping when c ended"
    );
    pb.pre_stop_gate.store(false, Ordering::SeqCst);
    let opened = Instant::now();
    until_started(&[&pa, &pb, &pc], 2).await;
    // Nothing but the restarts follows b's end.
    assert!(
        opened.elapsed() < upper(paused, Duration::from_millis(500)),
        "the restart took {:?}",
        opened.elapsed()
    );
    assert!(
        pc.drop_order(0) < pb.drop_order(0),
        "c ended before b's `pre_stop` ended"
    );
    assert_eq!(
        pb.pre_stops.load(Ordering::SeqCst),
        1,
        "b's stop was joined, never given a second ladder"
    );
    restart_initiated_once(&rig, &b);
    for event in rig.events.lock().unwrap().iter() {
        assert!(
            !format!("{:?}", event.reason).contains("NameTaken"),
            "no restart hit a taken name: {event:?}"
        );
    }
    // The chain starts the members in start order.
    let order = build_sequence(&[("a", &pa), ("b", &pb), ("c", &pc)]);
    assert_eq!(
        order,
        ["a#1", "b#1", "c#1", "a#2", "b#2", "c#2"],
        "start order"
    );
    assert!(rig.sup.is_alive());
}

#[test]
fn an_infinity_member_blocks_the_members_queued_behind_it() {
    common::run(
        0,
        an_infinity_member_blocks_the_members_queued_behind_it_body(false),
    );
}

#[test]
fn an_infinity_member_blocks_the_members_queued_behind_it_paused() {
    common::run_paused(an_infinity_member_blocks_the_members_queued_behind_it_body(
        true,
    ));
}

async fn an_infinity_member_blocks_the_members_queued_behind_it_body(paused: bool) {
    let rig = supervisor(one_for_all()).await;
    let (a, b) = (uname("a"), uname("b"));
    let c = uname("c");
    let (pa, pb, pc) = probes();
    spawn_members(&rig, &[(&a, &pa), (&b, &pb)]).await;
    rig.spawn_child(&c, &pc, Shutdown::Infinity, RestartType::Permanent, 0)
        .await;
    wait_until(BOUND, "the children to finish starting", || {
        [&pa, &pb, &pc]
            .iter()
            .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
    })
    .await;
    // c's `pre_stop` waits for the test and then accepts: a slow stop, not
    // a veto.
    pc.pre_stop_gate.store(true, Ordering::SeqCst);
    crash(&child_handle(&a).await).await;
    wait_until(BOUND, "c to be asked to stop", || {
        pc.pre_stops.load(Ordering::SeqCst) == 1
    })
    .await;
    sleep(QUIET).await;
    // The cost of waiting without a bound: b, queued behind c, is not
    // asked to stop, and the supervisor still serves its messages.
    assert_eq!(pb.pre_stops.load(Ordering::SeqCst), 0);
    assert!(is_alive(&rig, &b).await, "b still runs");
    pc.pre_stop_gate.store(false, Ordering::SeqCst);
    let released = Instant::now();
    until_started(&[&pa, &pb, &pc], 2).await;
    // b's `pre_stop` accepts at once, so the restart takes the margin.
    assert!(
        released.elapsed() < upper(paused, Duration::from_millis(500)),
        "the group restarted {:?} after c was released",
        released.elapsed()
    );
    assert_eq!(pb.pre_stops.load(Ordering::SeqCst), 1);
    assert!(pc.drop_order(0) < pre_stop_at(&pb, 0), "c ended first");
    assert!(rig.sup.is_alive());
}

#[test]
fn terminate_child_of_a_member_the_stop_queue_has_not_reached_returns_once_it_ended() {
    common::run(0, async {
        let rig = supervisor(one_for_all()).await;
        let (a, b, c) = (uname("a"), uname("b"), uname("c"));
        let (pa, pb, pc) = probes();
        start_members(&rig, &[(&a, &pa), (&b, &pb), (&c, &pc)]).await;
        // c's stop waits for the test, so the queue stays at c.
        pc.pre_stop_gate.store(true, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "c to be asked to stop", || {
            pc.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        // The call stops b itself and returns once it ended.
        let lines = rig.script(vec![Op::Terminate(b.clone())]).await;
        assert_eq!(lines, ["Ok(())"], "{lines:?}");
        assert!(
            pb.drop_order(0) != usize::MAX,
            "b ended before the call returned"
        );
        assert_eq!(
            pc.drop_order(0),
            usize::MAX,
            "c was still being stopped when the call returned"
        );
        pc.pre_stop_gate.store(false, Ordering::SeqCst);
        wait_until(BOUND, "the group to restart a and c", || {
            pa.started.load(Ordering::SeqCst) == 2 && pc.started.load(Ordering::SeqCst) == 2
        })
        .await;
        sleep(QUIET).await;
        assert_eq!(pb.built.load(Ordering::SeqCst), 1, "b stays down");
        assert!(!is_alive(&rig, &b).await);
        removed_once(&rig, &b);
        assert_eq!(pa.built.load(Ordering::SeqCst), 2);
        assert_eq!(pc.built.load(Ordering::SeqCst), 2);
        assert!(rig.sup.is_alive());
    });
}

/// What the tests of the one more ask share: OneForAll `[a, b, c]` where `b`
/// runs under `Infinity` and `c`'s `pre_stop` waits for the test, so the
/// group stops `c` before it reaches `b`. `b` vetoes its first `vetoes`
/// requests and accepts the next.
struct AskGroup {
    rig: Rig,
    a: String,
    b: String,
    pa: Arc<Probe>,
    pb: Arc<Probe>,
    pc: Arc<Probe>,
}

async fn ask_group(vetoes: usize) -> AskGroup {
    let rig = supervisor(one_for_all()).await;
    let (a, b, c) = (uname("a"), uname("b"), uname("c"));
    let (pa, pb, pc) = probes();
    rig.spawn_child(
        &a,
        &pa,
        Shutdown::Timeout(Duration::from_secs(2)),
        RestartType::Permanent,
        0,
    )
    .await;
    rig.spawn_child(&b, &pb, Shutdown::Infinity, RestartType::Permanent, vetoes)
        .await;
    rig.spawn_child(
        &c,
        &pc,
        Shutdown::Timeout(Duration::from_secs(2)),
        RestartType::Permanent,
        0,
    )
    .await;
    wait_until(BOUND, "the children to finish starting", || {
        [&pa, &pb, &pc]
            .iter()
            .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
    })
    .await;
    pc.pre_stop_gate.store(true, Ordering::SeqCst);
    AskGroup {
        rig,
        a,
        b,
        pa,
        pb,
        pc,
    }
}

impl AskGroup {
    /// `a` crashes, and `c` is being stopped when this returns.
    async fn crash_a_and_wait_for_c(&self) {
        crash(&child_handle(&self.a).await).await;
        wait_until(
            BOUND,
            "c, the head of the queue, to be asked to stop",
            || self.pc.pre_stops.load(Ordering::SeqCst) == 1,
        )
        .await;
    }

    /// `c`'s stop is allowed to end.
    fn open_c(&self) {
        self.pc.pre_stop_gate.store(false, Ordering::SeqCst);
    }

    /// `b`'s `pre_stop` has answered this many requests.
    async fn until_b_answered(&self, answers: usize) {
        wait_until(BOUND, "b to answer a request", || {
            self.pb.pre_stops_returned.load(Ordering::SeqCst) == answers
        })
        .await;
    }

    /// The group restarted `a` and `c`; `b` stays down, reported once as
    /// removed.
    async fn finish_with_b_down(&self) {
        wait_until(BOUND, "the group to restart a and c", || {
            self.pa.started.load(Ordering::SeqCst) == 2
                && self.pc.started.load(Ordering::SeqCst) == 2
        })
        .await;
        sleep(QUIET).await;
        assert_eq!(self.pb.built.load(Ordering::SeqCst), 1, "b stays down");
        removed_once(&self.rig, &self.b);
        assert_eq!(self.pa.built.load(Ordering::SeqCst), 2);
        assert_eq!(self.pc.built.load(Ordering::SeqCst), 2);
        assert!(self.rig.sup.is_alive());
    }
}

/// The bound on a terminate override that must give its own ask: it returns
/// at once when it does, and a call without the ask waits on the queue that
/// only the loop it holds can advance.
const OVERRIDE_BOUND: Duration = Duration::from_secs(2);

#[test]
fn terminate_override_gives_an_infinity_member_the_one_more_ask_the_group_has_not_given() {
    common::run(0, async {
        let g = ask_group(1).await;
        // b vetoes the first stop of a bounce whose caller gave up.
        g.rig
            .ok(Cmd::StopAbandon {
                name: g.b.clone(),
                wait: Duration::ZERO,
            })
            .await;
        g.until_b_answered(1).await;
        g.crash_a_and_wait_for_c().await;
        // While c is being stopped, the call asks b once more itself, b
        // accepts, and the call returns.
        let lines = g
            .rig
            .script(vec![Op::TerminateWithin(g.b.clone(), OVERRIDE_BOUND)])
            .await;
        assert_eq!(lines, ["Ok(())"], "{lines:?}");
        assert_eq!(
            g.pc.drop_order(0),
            usize::MAX,
            "b ended while c was still being stopped"
        );
        g.open_c();
        g.finish_with_b_down().await;
        assert_eq!(
            g.pb.pre_stops.load(Ordering::SeqCst),
            2,
            "the bounce's request and the override's ask: the group asks nothing more"
        );
    });
}

#[test]
fn parent_teardown_asks_a_vetoing_infinity_child_once_more_after_its_group_did() {
    common::run(0, async {
        // b vetoes the first two requests and accepts the third.
        let g = ask_group(2).await;
        g.rig
            .ok(Cmd::StopAbandon {
                name: g.b.clone(),
                wait: Duration::ZERO,
            })
            .await;
        g.until_b_answered(1).await;
        g.crash_a_and_wait_for_c().await;
        // The override's call gives the one more ask: b vetoes again, and the
        // call is abandoned.
        let lines = g.rig.script(vec![Op::TerminateAbandon(g.b.clone())]).await;
        assert_eq!(lines, ["abandoned"]);
        g.until_b_answered(2).await;
        g.open_c();
        // The supervisor's own stop asks b once more.
        let took = stop_supervisor(&g.rig, Duration::from_secs(5)).await;
        assert!(took < Duration::from_secs(5), "{took:?}");
        assert_eq!(
            g.pb.pre_stops.load(Ordering::SeqCst),
            3,
            "the bounce, the override's ask and the teardown's ask"
        );
    });
}

#[test]
fn parent_teardown_asks_a_vetoing_infinity_member_the_group_ordered_once_more() {
    common::run(0, async {
        let g = ask_group(1).await;
        g.crash_a_and_wait_for_c().await;
        g.open_c();
        // The group stops c, then orders b, which vetoes.
        g.until_b_answered(1).await;
        let took = stop_supervisor(&g.rig, Duration::from_secs(5)).await;
        assert!(took < Duration::from_secs(5), "{took:?}");
        assert_eq!(
            g.pb.pre_stops.load(Ordering::SeqCst),
            2,
            "the group's order and the teardown's ask"
        );
    });
}

#[test]
fn terminate_override_gives_the_one_more_ask_to_a_member_the_group_ordered() {
    common::run(0, async {
        let g = ask_group(1).await;
        g.crash_a_and_wait_for_c().await;
        g.open_c();
        g.until_b_answered(1).await;
        // The queue reached b and its order was vetoed: the call asks once
        // more, b accepts.
        let lines = g
            .rig
            .script(vec![Op::TerminateWithin(g.b.clone(), OVERRIDE_BOUND)])
            .await;
        assert_eq!(lines, ["Ok(())"], "{lines:?}");
        g.finish_with_b_down().await;
        assert_eq!(g.pb.pre_stops.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn a_second_terminate_override_gives_the_one_more_ask_to_a_member_holding_the_first_ones_order() {
    common::run(0, async {
        let g = ask_group(1).await;
        g.crash_a_and_wait_for_c().await;
        // The first call writes b's order and is abandoned; b vetoes it.
        let lines = g
            .rig
            .script(vec![
                Op::Read(g.b.clone()),
                Op::TerminateAbandon(g.b.clone()),
            ])
            .await;
        assert!(lines[0].starts_with("alive=true"), "{lines:?}");
        g.until_b_answered(1).await;
        // The queue has not reached b. The second call joins that order and
        // gives the one more ask, since nobody asked: b accepts, and the call
        // returns while c is still being stopped.
        let lines = g
            .rig
            .script(vec![Op::TerminateWithin(g.b.clone(), OVERRIDE_BOUND)])
            .await;
        assert_eq!(lines, ["Ok(())"], "{lines:?}");
        assert_eq!(
            g.pc.drop_order(0),
            usize::MAX,
            "b ended while c was still being stopped"
        );
        g.open_c();
        g.finish_with_b_down().await;
        assert!(
            g.pb.drop_order(0) < g.pc.drop_order(0),
            "b ended before c did"
        );
        assert_eq!(
            g.pb.pre_stops.load(Ordering::SeqCst),
            2,
            "the order and the ask: the queue reaching b asks nothing more"
        );
    });
}

#[test]
fn a_member_stopping_at_the_evaluation_is_asked_once_more_when_the_queue_reaches_it() {
    common::run(0, async {
        let g = ask_group(1).await;
        g.rig
            .ok(Cmd::StopAbandon {
                name: g.b.clone(),
                wait: Duration::ZERO,
            })
            .await;
        g.until_b_answered(1).await;
        g.crash_a_and_wait_for_c().await;
        // The evaluation asked nothing of b: it is behind c.
        assert_eq!(g.pb.pre_stops.load(Ordering::SeqCst), 1);
        g.open_c();
        until_started(&[&g.pa, &g.pb, &g.pc], 2).await;
        assert_eq!(
            g.pb.pre_stops.load(Ordering::SeqCst),
            2,
            "b was asked once more, by the queue"
        );
        assert!(
            g.pc.drop_order(0) < pre_stop_at(&g.pb, 1),
            "b's second request came after c ended"
        );
        let order = build_sequence(&[("a", &g.pa), ("b", &g.pb), ("c", &g.pc)]);
        assert_eq!(order, ["a#1", "b#1", "c#1", "a#2", "b#2", "c#2"]);
        assert!(g.rig.sup.is_alive());
    });
}

#[test]
fn group_restart_stops_a_spawned_attempt_in_its_turn_behind_the_member_ahead_of_it() {
    common::run(0, async {
        let rig = supervisor(one_for_all()).await;
        let (a, b, c) = (uname("a"), uname("b"), uname("c"));
        let (pa, pb, pc) = probes();
        start_members(&rig, &[(&a, &pa), (&b, &pb), (&c, &pc)]).await;
        // b is bounced and its new instance stays in its initialization: b's
        // restart attempt has spawned its actor and is in flight.
        pb.held.store(true, Ordering::SeqCst);
        rig.ok(Cmd::Stop { name: b.clone() }).await;
        wait_until(BOUND, "b's new instance to be built", || {
            pb.built.load(Ordering::SeqCst) == 2
        })
        .await;
        // a crashes: the queue is [c, b], and c's stop waits for the test.
        pc.pre_stop_gate.store(true, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "c to be asked to stop: the group's order", || {
            pc.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        // The attempt finishes its initialization inside c's stop, and is not
        // asked to stop before c ended.
        pb.held.store(false, Ordering::SeqCst);
        wait_until(BOUND, "b's attempt to finish its initialization", || {
            pb.started.load(Ordering::SeqCst) == 2
        })
        .await;
        // The supervisor adopts the attempt's instance, and the instance then
        // completes a turn: an order written at the adoption would have
        // reached it at that turn's boundary.
        let deadline = tokio::time::Instant::now() + BOUND;
        loop {
            let read = rig.script(vec![Op::Read(b.clone())]).await;
            if read[0].starts_with("alive=true") {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the supervisor adopts b's attempt"
            );
            sleep(Duration::from_millis(2)).await;
        }
        child_handle(&b)
            .await
            .send(ChildMsg::Ping)
            .await
            .expect("b handles a message");
        assert_eq!(pc.drop_order(0), usize::MAX, "c is still being stopped");
        assert_eq!(pb.pre_stops.load(Ordering::SeqCst), 1, "b waits behind c");
        pc.pre_stop_gate.store(false, Ordering::SeqCst);
        until_started(&[&pa, &pc], 2).await;
        wait_until(BOUND, "b to be started in its slot", || {
            pb.built.load(Ordering::SeqCst) == 3 && pb.started.load(Ordering::SeqCst) == 3
        })
        .await;
        // The group reached b after c ended: the attempt's instance was asked
        // to stop only then.
        assert_eq!(pb.pre_stops.load(Ordering::SeqCst), 2);
        assert!(
            pc.drop_order(0) < pre_stop_at(&pb, 1),
            "the attempt was stopped in its turn, after c"
        );
        // The chain starts a, b, c in that order.
        assert!(
            pb.last_start.load(Ordering::SeqCst) > pa.last_start.load(Ordering::SeqCst),
            "b's post-group incarnation started after a's"
        );
        wait_for_alive(&rig, &c).await;
        sleep(QUIET).await;
        assert_eq!(pa.built.load(Ordering::SeqCst), 2);
        assert_eq!(pb.built.load(Ordering::SeqCst), 3);
        assert_eq!(pc.built.load(Ordering::SeqCst), 2);
        assert_eq!(pb.max_live.load(Ordering::SeqCst), 1);
        assert!(rig.sup.is_alive());
    });
}

/// Releases a blocked factory when the test ends, however it ends: a thread
/// of the runtime stuck in the factory would hold the runtime's shutdown.
struct ReleaseFactory(Arc<Probe>);

impl Drop for ReleaseFactory {
    fn drop(&mut self) {
        self.0.factory_hold.store(false, Ordering::SeqCst);
    }
}

#[test]
fn group_stop_orders_an_attempt_that_passed_its_check_before_the_plan_cancelled_it() {
    // The factory blocks its thread in `block_in_place`, which needs the
    // multi-thread flavor; one worker is enough.
    common::run_multi_thread_only(1, async {
        let rig = supervisor(one_for_all()).await;
        let (a, b) = (uname("a"), uname("b"));
        let (pa, pb, _) = probes();
        let _release = ReleaseFactory(pb.clone());
        let window = Duration::from_millis(400);
        rig.spawn_child(
            &a,
            &pa,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        // b has no start timeout: nothing but its stop order ends a hung init.
        rig.spawn_child(
            &b,
            &pb,
            Shutdown::Timeout(window),
            RestartType::Permanent,
            0,
        )
        .await;
        wait_until(BOUND, "the children to finish starting", || {
            [&pa, &pb]
                .iter()
                .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        // b is bounced; its attempt passes its check and blocks in the
        // factory, before it published a lane.
        pb.factory_hold.store(true, Ordering::SeqCst);
        rig.ok(Cmd::Stop { name: b.clone() }).await;
        wait_until(BOUND, "b's attempt to enter its factory", || {
            pb.in_factory.load(Ordering::SeqCst)
        })
        .await;
        // a crashes: the plan's cancel flag comes too late for that attempt,
        // and the queue's head is b.
        crash(&child_handle(&a).await).await;
        wait_until(BOUND, "the group to be evaluated", || {
            rig.events_for(&a)
                .iter()
                .any(|event| matches!(event.reason, StopReason::Failure(_)))
        })
        .await;
        // The attempt goes on and its instance hangs in its initialization.
        pb.held.store(true, Ordering::SeqCst);
        pb.factory_hold.store(false, Ordering::SeqCst);
        // The order the queue wrote stops it within b's window, its kill grace
        // and the settle time.
        let bound = window + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE + Duration::from_millis(500);
        wait_until(
            upper(false, bound),
            "the group to stop b's hung attempt",
            || pb.built.load(Ordering::SeqCst) >= 2 && pb.drop_order(1) != usize::MAX,
        )
        .await;
        pb.held.store(false, Ordering::SeqCst);
        wait_until(BOUND, "the group to restart a and b", || {
            pa.started.load(Ordering::SeqCst) == 2 && pb.started.load(Ordering::SeqCst) == 2
        })
        .await;
        assert_eq!(pb.built.load(Ordering::SeqCst), 3);
        assert!(rig.sup.is_alive());
    });
}

#[test]
fn group_restart_cancels_a_waiting_attempt_at_the_plan_while_a_slower_member_is_stopped() {
    common::run(0, async {
        let sys = ActorSystem::create(uname("cancel-at-plan")).unwrap();
        let rig = supervisor_on(&sys, one_for_all()).await;
        let (a, p, w, c) = (uname("a"), uname("p"), uname("w"), uname("c"));
        let (pa, pw, pc) = probes();
        rig.spawn_child(
            &a,
            &pa,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        // P is a supervisor child with a short window; its worker W sits in a
        // turn that never reaches a boundary, which ends only when W's kill
        // grace of two seconds has passed.
        let p_built = Arc::new(AtomicUsize::new(0));
        rig.ok(Cmd::SpawnSupervisorChild {
            name: p.clone(),
            shutdown: Shutdown::Timeout(Duration::from_millis(100)),
            restart: RestartType::Permanent,
            config: plain(),
            built: p_built.clone(),
            events: Arc::new(Mutex::new(Vec::new())),
            knobs: Arc::new(Knobs::default()),
        })
        .await;
        rig.spawn_child(
            &c,
            &pc,
            Shutdown::Timeout(Duration::from_secs(5)),
            RestartType::Permanent,
            0,
        )
        .await;
        let inner = sys
            .get::<Sup>(&p)
            .expect("the child supervisor is registered");
        let spawned = inner
            .send(Cmd::SpawnWithGrace {
                name: w.clone(),
                probe: pw.clone(),
                shutdown: Shutdown::Timeout(Duration::from_secs(5)),
                kill_grace: Duration::from_secs(2),
            })
            .await
            .expect("the child supervisor answers");
        assert!(matches!(spawned, Reply::Done(Ok(()))), "{spawned:?}");
        wait_until(BOUND, "a and c to finish starting", || {
            pa.started.load(Ordering::SeqCst) == 1 && pc.started.load(Ordering::SeqCst) == 1
        })
        .await;
        let child_w = child_in(&sys, &w).await;
        child_w
            .notify(ChildMsg::Park)
            .await
            .expect("start the parked turn");
        wait_until(BOUND, "W's turn to park", || {
            pw.parked.load(Ordering::SeqCst)
        })
        .await;
        // P is bounced: its window passes and it is aborted with W's turn
        // still running, so P's restart attempt waits for W's subtree.
        rig.ok(Cmd::Stop { name: p.clone() }).await;
        assert_eq!(p_built.load(Ordering::SeqCst), 1, "the attempt waits");
        // a crashes while P's attempt waits: the queue is [c, P], and c's stop
        // waits for the test, so the queue is not at P. The plan's cancel flag
        // ends the attempt, whose member then reads as down, not restarting.
        pc.pre_stop_gate.store(true, Ordering::SeqCst);
        crash(&child_in(&sys, &a).await).await;
        wait_until(BOUND, "c to be asked to stop", || {
            pc.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        let deadline = Instant::now() + Duration::from_secs(5);
        let answer = loop {
            let lines = rig
                .script(vec![Op::TerminateWithin(p.clone(), Duration::from_secs(4))])
                .await;
            if !restarting(&lines[0]) {
                break lines[0].clone();
            }
            assert!(
                Instant::now() < deadline,
                "P's attempt was still in flight at the bound: {lines:?}"
            );
            sleep(Duration::from_millis(5)).await;
        };
        assert_eq!(answer, "Ok(())");
        assert_eq!(
            pc.drop_order(0),
            usize::MAX,
            "the attempt ended while c was still being stopped"
        );
        pc.pre_stop_gate.store(false, Ordering::SeqCst);
        wait_until(BOUND, "the group to restart a and c", || {
            pa.started.load(Ordering::SeqCst) == 2 && pc.started.load(Ordering::SeqCst) == 2
        })
        .await;
        assert_eq!(p_built.load(Ordering::SeqCst), 1, "P stays down");
        assert!(rig.sup.is_alive());
    });
}

/// OneForAll `[a, b, c, ...]`, b's restart attempt in flight in its
/// initialization (started by a bounce, or by a `restart_child` of a
/// terminated b) and the queue at c when `a` crashes: the attempt's
/// initialization fails before the queue reaches b.
async fn attempt_fails_before_its_turn(manual: bool) {
    // One restart per minute: the failed attempt is part of the group, and a
    // charge for it would end the supervisor.
    let rig =
        supervisor(SupervisionConfig::one_for_all().max_restarts(1, Duration::from_secs(60))).await;
    let (a, b, c) = (uname("a"), uname("b"), uname("c"));
    let (pa, pb, pc) = probes();
    start_members(&rig, &[(&a, &pa), (&b, &pb), (&c, &pc)]).await;
    if manual {
        rig.ok(Cmd::Terminate { name: b.clone() }).await;
        pb.held.store(true, Ordering::SeqCst);
        rig.ok(Cmd::RestartChild { name: b.clone() }).await;
    } else {
        pb.held.store(true, Ordering::SeqCst);
        rig.ok(Cmd::Stop { name: b.clone() }).await;
    }
    wait_until(BOUND, "b's new instance to be built", || {
        pb.built.load(Ordering::SeqCst) == 2
    })
    .await;
    pc.pre_stop_gate.store(true, Ordering::SeqCst);
    crash(&child_handle(&a).await).await;
    wait_until(BOUND, "c to be asked to stop", || {
        pc.pre_stops.load(Ordering::SeqCst) == 1
    })
    .await;
    // The attempt's initialization fails while c is being stopped, before the
    // queue reaches b; the failure is reported at once.
    pb.fail_next.store(1, Ordering::SeqCst);
    pb.held.store(false, Ordering::SeqCst);
    wait_until(BOUND, "the failed attempt to be reported", || {
        rig.events_for(&b).iter().any(|event| {
            matches!(&event.reason, StopReason::Failure(err) if err.to_string().contains("refused to start"))
        })
    })
    .await;
    assert_eq!(pc.drop_order(0), usize::MAX, "c is still being stopped");
    pc.pre_stop_gate.store(false, Ordering::SeqCst);
    until_started(&[&pa, &pc], 2).await;
    wait_until(BOUND, "b to be started in its slot", || {
        pb.built.load(Ordering::SeqCst) == 3 && pb.started.load(Ordering::SeqCst) == 2
    })
    .await;
    wait_for_alive(&rig, &b).await;
    sleep(QUIET).await;
    // The failed attempt is reported once, as the group's restart of b, with
    // the error its initialization returned.
    let reports = rig.events_for(&b);
    assert_eq!(reports.len(), 2, "the stop and the attempt: {reports:?}");
    assert_eq!(
        reports[0].action,
        if manual {
            SupervisionAction::Removed
        } else {
            SupervisionAction::RestartInitiated
        }
    );
    assert_eq!(reports[1].action, SupervisionAction::RestartInitiated);
    assert!(
        matches!(&reports[1].reason, StopReason::Failure(err) if err.to_string().contains("refused to start")),
        "{reports:?}"
    );
    assert_eq!(
        pb.pre_stops.load(Ordering::SeqCst),
        1,
        "never asked to stop"
    );
    assert!(rig.sup.is_alive(), "no charge for the failed attempt");
    assert!(rig
        .events
        .lock()
        .unwrap()
        .iter()
        .all(|event| event.action != SupervisionAction::BudgetExhausted));
}

#[test]
fn a_committed_attempt_whose_init_fails_before_its_turn_reports_the_init_error_through_the_member_route(
) {
    common::run(0, async {
        attempt_fails_before_its_turn(false).await;
        attempt_fails_before_its_turn(true).await;
    });
}

/// OneForAll `[a, b, c]` with `b` of the given restart type and `c`'s stop
/// waiting for the test: `a` crashes, and the call under test is made while
/// the group waits on `c`.
async fn non_permanent_member_group(restart: RestartType) -> (Rig, [String; 3], [Arc<Probe>; 3]) {
    let rig =
        supervisor(SupervisionConfig::one_for_all().max_restarts(3, Duration::from_secs(60))).await;
    let (a, pa) = member(&rig, "a", Duration::from_secs(2), 0).await;
    let b = uname("b");
    let pb = Arc::new(Probe::default());
    rig.ok(Cmd::Spawn {
        name: b.clone(),
        probe: pb.clone(),
        shutdown: Shutdown::Timeout(Duration::from_secs(2)),
        restart,
        vetoes: 0,
    })
    .await;
    let (c, pc) = member(&rig, "c", Duration::from_secs(2), 0).await;
    wait_until(BOUND, "the children to start", || {
        [&pa, &pb, &pc]
            .iter()
            .all(|p| p.started.load(Ordering::SeqCst) == 1)
    })
    .await;
    pc.pre_stop_gate.store(true, Ordering::SeqCst);
    crash(&child_handle(&a).await).await;
    wait_until(BOUND, "the group to ask c to stop", || {
        pc.pre_stops.load(Ordering::SeqCst) == 1
    })
    .await;
    (rig, [a, b, c], [pa, pb, pc])
}

#[test]
fn stop_child_of_a_transient_member_of_a_pending_group_stops_it_and_keeps_it_down_through_the_retry(
) {
    common::run(0, async {
        let (rig, [a, b, c], [pa, pb, pc]) = {
            // c's restart fails once, so the group retries.
            let parts = non_permanent_member_group(RestartType::Transient).await;
            parts.2[2].fail_next.store(1, Ordering::SeqCst);
            parts
        };
        let lines = rig
            .script(vec![Op::Read(b.clone()), Op::StopOutcome(b.clone())])
            .await;
        assert_eq!(lines[0], "alive=true pending=false", "{lines:?}");
        assert_eq!(lines[1], "Ok(Stopped)", "never absorbed: {lines:?}");
        assert!(
            pb.drop_order(0) != usize::MAX,
            "b was stopped at once, beside c"
        );
        assert_eq!(pc.drop_order(0), usize::MAX, "c is still being stopped");
        pc.pre_stop_gate.store(false, Ordering::SeqCst);
        until_idle(&rig, &[&a, &b, &c]).await;
        wait_for_alive(&rig, &c).await;
        sleep(QUIET).await;
        assert!(
            pb.drop_order(0) < pc.drop_order(0),
            "b's end was recorded before c's"
        );
        removed_once(&rig, &b);
        // The group restarted a and c, c's start failed, and the retry
        // restarted a and c again, never b; b's spec is kept.
        assert_eq!(pa.built.load(Ordering::SeqCst), 3);
        assert_eq!(pc.built.load(Ordering::SeqCst), 3);
        assert_eq!(pb.built.load(Ordering::SeqCst), 1);
        assert_eq!(
            rig.script(vec![Op::Read(b.clone())]).await[0],
            "alive=false pending=false"
        );
        let lines = rig.script(vec![Op::Restart(b.clone())]).await;
        assert_eq!(lines[0], "Ok(())", "{lines:?}");
        wait_until(BOUND, "b to start again", || {
            pb.started.load(Ordering::SeqCst) == 2
        })
        .await;
    });
}

#[test]
fn stop_child_of_a_temporary_member_of_a_pending_group_stops_and_removes_it() {
    common::run(0, async {
        let (rig, [a, b, c], [pa, pb, pc]) =
            non_permanent_member_group(RestartType::Temporary).await;
        let lines = rig
            .script(vec![Op::Read(b.clone()), Op::StopOutcome(b.clone())])
            .await;
        assert_eq!(lines[0], "alive=true pending=false", "{lines:?}");
        assert_eq!(lines[1], "Ok(Stopped)", "never absorbed: {lines:?}");
        assert_eq!(pc.drop_order(0), usize::MAX, "c is still being stopped");
        pc.pre_stop_gate.store(false, Ordering::SeqCst);
        until_idle(&rig, &[&a, &c]).await;
        wait_for_alive(&rig, &c).await;
        sleep(QUIET).await;
        assert!(
            pb.drop_order(0) < pc.drop_order(0),
            "b's end was recorded before c's"
        );
        assert_eq!(pb.built.load(Ordering::SeqCst), 1);
        assert_eq!(pa.built.load(Ordering::SeqCst), 2);
        assert_eq!(pc.built.load(Ordering::SeqCst), 2);
        removed_once(&rig, &b);
        let lines = rig
            .script(vec![Op::Read(b.clone()), Op::Restart(b.clone())])
            .await;
        assert_eq!(lines[0], "no such child", "{lines:?}");
        assert!(lines[1].contains("not found"), "{lines:?}");
    });
}

fn pool() -> SupervisionConfig {
    SupervisionConfig::simple_one_for_one().max_restarts(5, Duration::from_secs(60))
}

#[test]
fn simple_one_for_one_shutdown_stops_children_concurrently() {
    common::run(
        0,
        simple_one_for_one_shutdown_stops_children_concurrently_body(false),
    );
}

#[test]
fn simple_one_for_one_shutdown_stops_children_concurrently_paused() {
    common::run_paused(simple_one_for_one_shutdown_stops_children_concurrently_body(true));
}

async fn simple_one_for_one_shutdown_stops_children_concurrently_body(paused: bool) {
    let rig = supervisor(pool()).await;
    let mut children: Vec<(String, Arc<Probe>)> = Vec::new();
    for _ in 0..20 {
        let (name, probe) = (uname("w"), Arc::new(Probe::default()));
        rig.spawn_child(
            &name,
            &probe,
            Shutdown::Timeout(Duration::from_secs(1)),
            RestartType::Permanent,
            0,
        )
        .await;
        probe.pre_stop_sleep_ms.store(200, Ordering::SeqCst);
        children.push((name, probe));
    }
    wait_until(BOUND, "the children to finish starting", || {
        children
            .iter()
            .all(|(_, probe)| probe.started.load(Ordering::SeqCst) == 1)
    })
    .await;
    // 20 stops of 200 ms take 4 s one after the other.
    let took = stop_supervisor(&rig, upper(paused, Duration::from_secs(2))).await;
    assert!(took < upper(paused, Duration::from_secs(2)), "{took:?}");
    let last_begun = children
        .iter()
        .map(|(_, probe)| pre_stop_at(probe, 0))
        .max()
        .unwrap();
    let first_ended = children
        .iter()
        .map(|(_, probe)| probe.drop_order(0))
        .min()
        .unwrap();
    assert!(
        last_begun < first_ended,
        "every `pre_stop` began before the first child ended"
    );
}

#[test]
fn simple_one_for_one_shutdown_joins_a_stopping_child_and_orders_a_restarting_one() {
    common::run(
        0,
        simple_one_for_one_shutdown_joins_a_stopping_child_and_orders_a_restarting_one_body(false),
    );
}

#[test]
fn simple_one_for_one_shutdown_joins_a_stopping_child_and_orders_a_restarting_one_paused() {
    common::run_paused(
        simple_one_for_one_shutdown_joins_a_stopping_child_and_orders_a_restarting_one_body(true),
    );
}

async fn simple_one_for_one_shutdown_joins_a_stopping_child_and_orders_a_restarting_one_body(
    paused: bool,
) {
    let rig = supervisor(pool()).await;
    let window = Duration::from_secs(2);
    let (x, px) = (uname("x"), Arc::new(Probe::default()));
    let (y, py) = (uname("y"), Arc::new(Probe::default()));
    let (z, pz) = (uname("z"), Arc::new(Probe::default()));
    // x vetoes every request, so a second request would be noticed, and
    // its first `pre_stop` waits for the test. y's window is short.
    for (name, probe, shutdown, vetoes) in [
        (&x, &px, Shutdown::Timeout(window), always_vetoes()),
        (&y, &py, Shutdown::Timeout(Duration::from_millis(500)), 0),
        (&z, &pz, Shutdown::Timeout(Duration::from_millis(500)), 0),
    ] {
        rig.spawn_child(name, probe, shutdown, RestartType::Permanent, vetoes)
            .await;
    }
    wait_until(BOUND, "the children to finish starting", || {
        [&px, &py, &pz]
            .iter()
            .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
    })
    .await;
    // x is stopping through a bounce whose caller gave up.
    px.pre_stop_gate.store(true, Ordering::SeqCst);
    rig.ok(Cmd::StopAbandon {
        name: x.clone(),
        wait: Duration::ZERO,
    })
    .await;
    wait_until(BOUND, "x to be asked to stop", || {
        px.pre_stops.load(Ordering::SeqCst) == 1
    })
    .await;
    // y crashes and its new instance stays in its initialization.
    py.held.store(true, Ordering::SeqCst);
    crash(&child_handle(&y).await).await;
    wait_until(BOUND, "y's new instance to be built", || {
        py.built.load(Ordering::SeqCst) == 2
    })
    .await;
    // The supervisor is stopped. The one step of its teardown joins x and
    // orders y's attempt, whose end shows the step ran; only then does x's
    // first `pre_stop` return.
    rig.sup
        .stop(StopReason::Graceful)
        .await
        .expect("the stop is accepted");
    let bound = upper(
        paused,
        window + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE + Duration::from_millis(500),
    );
    // y's window is the short one: its attempt ends long before x's
    // window does, which is when the supervisor's state would be dropped.
    let y_bound = upper(
        paused,
        Duration::from_millis(500)
            + ASSUMED_KILL_GRACE
            + ASSUMED_ABORT_SETTLE
            + Duration::from_millis(500),
    );
    wait_until(y_bound, "y's attempt to be stopped by the teardown", || {
        py.live.load(Ordering::SeqCst) == 0
    })
    .await;
    px.pre_stop_gate.store(false, Ordering::SeqCst);
    within(
        bound,
        "the supervisor to publish Stopped",
        rig.sup.wait_stopped(),
    )
    .await;
    assert_eq!(
        px.pre_stops.load(Ordering::SeqCst),
        1,
        "x's stop was joined, never given a second ladder"
    );
    for probe in [&px, &py, &pz] {
        assert_eq!(probe.live.load(Ordering::SeqCst), 0, "every child ended");
    }
    assert_eq!(py.built.load(Ordering::SeqCst), 2);
}

#[test]
fn group_restart_cancels_an_unspawned_attempt_at_the_plan_while_a_slower_member_is_stopped() {
    // One thread: nothing else runs between the statements of the handler, and
    // the supervisor reaches the group evaluation before the attempt it
    // started is polled for the first time.
    common::run_current_thread_only(async {
        let rig = supervisor(one_for_all()).await;
        let (a, c, d) = (uname("a"), uname("c"), uname("d"));
        let (pa, pc, pd) = probes();
        start_members(&rig, &[(&a, &pa), (&c, &pc), (&d, &pd)]).await;
        // d, started last, is the head of the queue, and its stop waits for
        // the test: the queue is not at c while the test looks.
        pd.pre_stop_gate.store(true, Ordering::SeqCst);
        let lines = rig
            .script(vec![Op::CrashAndWait(a.clone()), Op::Stop(c.clone())])
            .await;
        assert_eq!(lines[1], "Ok(())", "{lines:?}");
        wait_until(BOUND, "d to be asked to stop", || {
            pd.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        sleep(QUIET).await;
        // The plan's flag cancelled c's attempt before its factory ran: only
        // the first instance and the bounce's request exist.
        assert_eq!(
            pc.built.load(Ordering::SeqCst),
            1,
            "the cancelled attempt never calls the factory"
        );
        pd.pre_stop_gate.store(false, Ordering::SeqCst);
        until_started(&[&pa, &pc, &pd], 2).await;
        assert_eq!(pc.max_live.load(Ordering::SeqCst), 1);
        assert!(rig.sup.is_alive());
    });
}

// ---------------------------------------------------------------------------
// A factory the supervisor's own loop drops
// ---------------------------------------------------------------------------

/// The supervisor stopped with the panic of `child`'s factory drop, made in
/// its own loop right after the operation that removed the spec: its
/// `on_stopped` saw that reason, and `child`'s end, taken by the turn and not
/// delivered there, is delivered once, with its name, before `on_stopped`.
async fn assert_stopped_by_the_factory_drop(rig: &Rig, child: &str) {
    let want = StopReason::Failure(ActorError::Panic(FACTORY_DROP_PANIC.to_string()));
    let reason = timeout(BOUND, rig.sup.wait_stopped())
        .await
        .expect("the supervisor ends");
    assert_eq!(reason, want);
    assert_eq!(
        rig.knobs.stopped.lock().unwrap().clone(),
        vec![want.clone()],
        "on_stopped saw the reason the supervisor ends with"
    );
    let events = rig.events_for(child);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].child_name.as_deref(), Some(child));
    let order = rig.knobs.order.lock().unwrap().clone();
    let delivered = order
        .iter()
        .position(|line| line.starts_with(&format!("delivery:{child}:")));
    let stopped = order.iter().position(|line| line == "on_stopped");
    assert!(
        delivered.is_some() && stopped.is_some() && delivered < stopped,
        "the end is delivered before on_stopped: {order:?}"
    );
}

/// A `Temporary` child that stops is removed by the strategy: the loop
/// prunes its spec and drops its factory.
#[test]
fn a_factory_the_strategy_prunes_stops_the_supervisor_with_its_panic() {
    common::run(0, async {
        let rig = supervisor(SupervisionConfig::default()).await;
        let c = uname("c");
        let pc = Arc::new(Probe::default());
        rig.spawn_child(
            &c,
            &pc,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Temporary,
            0,
        )
        .await;
        wait_until(BOUND, "c to start", || {
            pc.started.load(Ordering::SeqCst) == 1
        })
        .await;
        pc.factory_drop_panics.store(true, Ordering::SeqCst);
        child_handle(&c)
            .await
            .stop(StopReason::Graceful)
            .await
            .expect("accepted");
        assert_stopped_by_the_factory_drop(&rig, &c).await;
        pc.factory_drop_panics.store(false, Ordering::SeqCst);
    });
}

/// A `Temporary` member a group stops is reported removed: the loop prunes
/// its spec when its end arrives.
#[test]
fn a_factory_pruned_with_a_group_member_s_end_stops_the_supervisor_with_its_panic() {
    common::run(0, async {
        let rig = supervisor(one_for_all()).await;
        let (a, b) = (uname("a"), uname("b"));
        let (pa, pb, _) = probes();
        spawn_members(&rig, &[(&a, &pa)]).await;
        rig.spawn_child(
            &b,
            &pb,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Temporary,
            0,
        )
        .await;
        wait_until(BOUND, "a and b to start", || {
            pa.started.load(Ordering::SeqCst) == 1 && pb.started.load(Ordering::SeqCst) == 1
        })
        .await;
        pb.factory_drop_panics.store(true, Ordering::SeqCst);
        crash(&child_handle(&a).await).await;
        assert_stopped_by_the_factory_drop(&rig, &b).await;
        pb.factory_drop_panics.store(false, Ordering::SeqCst);
    });
}

/// A `terminate_child` whose future is dropped right after its commit: the
/// loop classifies the child's end itself, removes the `Temporary` spec and
/// drops its factory.
#[test]
fn a_factory_removed_by_the_loop_s_classification_stops_the_supervisor_with_its_panic() {
    common::run(0, async {
        let rig = supervisor(SupervisionConfig::default()).await;
        let c = uname("c");
        let pc = Arc::new(Probe::default());
        rig.spawn_child(
            &c,
            &pc,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Temporary,
            0,
        )
        .await;
        wait_until(BOUND, "c to start", || {
            pc.started.load(Ordering::SeqCst) == 1
        })
        .await;
        pc.factory_drop_panics.store(true, Ordering::SeqCst);
        // c's stop waits in its pre_stop until the call is abandoned, so its
        // end reaches the loop with the stop still unclassified.
        pc.pre_stop_gate.store(true, Ordering::SeqCst);
        rig.ok(Cmd::TerminateAbandon {
            name: c.clone(),
            wait: Duration::ZERO,
        })
        .await;
        pc.pre_stop_gate.store(false, Ordering::SeqCst);
        assert_stopped_by_the_factory_drop(&rig, &c).await;
        pc.factory_drop_panics.store(false, Ordering::SeqCst);
    });
}

/// RestForOne `[x, y, z]`: y's restart is held and then fails; a `Temporary`
/// child n spawned meanwhile crashes and its death waits behind the group.
/// The retry covers n and absorbs its death: the absorption removes n's spec,
/// and its factory is dropped once the retry's plan is recorded.
#[test]
fn a_factory_a_group_absorption_removes_stops_the_supervisor_with_its_panic() {
    common::run(0, async {
        let rig =
            supervisor(SupervisionConfig::rest_for_one().max_restarts(2, Duration::from_secs(60)))
                .await;
        let (x, y, z, n) = (uname("x"), uname("y"), uname("z"), uname("n"));
        let (px, py, pz) = probes();
        let pn = Arc::new(Probe::default());
        spawn_members(&rig, &[(&x, &px), (&y, &py), (&z, &pz)]).await;
        wait_until(BOUND, "the children to finish starting", || {
            [&px, &py, &pz]
                .iter()
                .all(|probe| probe.started.load(Ordering::SeqCst) == 1)
        })
        .await;
        py.held.store(true, Ordering::SeqCst);
        py.fail_next.store(1, Ordering::SeqCst);
        crash(&child_handle(&y).await).await;
        wait_until(BOUND, "the chain to start y", || {
            py.built.load(Ordering::SeqCst) == 2
        })
        .await;
        rig.spawn_child(
            &n,
            &pn,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Temporary,
            0,
        )
        .await;
        wait_until(BOUND, "n to start", || {
            pn.started.load(Ordering::SeqCst) == 1
        })
        .await;
        pn.factory_drop_panics.store(true, Ordering::SeqCst);
        crash(&child_handle(&n).await).await;
        wait_routed(&rig, &n).await;
        py.held.store(false, Ordering::SeqCst);
        assert_stopped_by_the_factory_drop(&rig, &n).await;
        pn.factory_drop_panics.store(false, Ordering::SeqCst);
    });
}

/// How the supervisor and the child's factory are set up while a restart
/// attempt is inside its build.
#[derive(Clone, Copy)]
struct InBuild {
    /// A pool, whose teardown orders every child in one step, or one for
    /// one, whose teardown stops its children newest first.
    pool: bool,
    /// The factory's capture panics when dropped.
    factory_drop_panics: bool,
    /// The instance the attempt built panics when dropped.
    value_drop_panics: bool,
}

/// A child's restart attempt is inside its factory call when its
/// supervisor's teardown begins and releases the child's factory: the
/// attempt's reference is then the factory's last one, so the attempt
/// starts no incarnation, and the factory and the instance it built drop in
/// the attempt once the build returns. A panic of the factory's drop is the
/// supervisor's, exactly as when the teardown drops the factory: a graceful
/// stop ends as that panic, after `on_stopped` received `Graceful`, and an
/// earlier failure stands. The stop of a sibling, which the teardown begins
/// only after it released every factory, marks the moment the attempt's
/// reference became the last one; the sibling's `pre_stop` holds the
/// teardown there until the attempt's factory call is let go.
async fn stop_with_an_attempt_inside_its_build(shape: InBuild, stop: StopReason) {
    let config = if shape.pool {
        pool()
    } else {
        SupervisionConfig::default()
    };
    let rig = supervisor(config).await;
    let c = uname("c");
    let d = uname("d");
    let pc = Arc::new(Probe::default());
    let pd = Arc::new(Probe::default());
    for (name, probe) in [(&c, &pc), (&d, &pd)] {
        rig.spawn_child(
            name,
            probe,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
    }
    wait_until(BOUND, "c and d to start", || {
        pc.started.load(Ordering::SeqCst) == 1 && pd.started.load(Ordering::SeqCst) == 1
    })
    .await;
    // c's restart attempt enters its factory call and stays inside it.
    pc.factory_hold.store(true, Ordering::SeqCst);
    crash(&child_handle(&c).await).await;
    wait_until(BOUND, "the attempt to enter c's factory", || {
        pc.in_factory.load(Ordering::SeqCst)
    })
    .await;
    pc.factory_drop_panics
        .store(shape.factory_drop_panics, Ordering::SeqCst);
    pc.instance_drop_panics
        .store(shape.value_drop_panics, Ordering::SeqCst);
    // The teardown reaches d; d's pre_stop waits.
    pd.stop_held.store(true, Ordering::SeqCst);
    rig.sup.stop(stop.clone()).await.expect("accepted");
    wait_until(BOUND, "the teardown to reach d", || {
        pd.pre_stops.load(Ordering::SeqCst) == 1
    })
    .await;
    pc.factory_hold.store(false, Ordering::SeqCst);
    pd.stop_held.store(false, Ordering::SeqCst);
    let reason = timeout(BOUND, rig.sup.wait_stopped())
        .await
        .expect("the supervisor ends");
    pc.factory_drop_panics.store(false, Ordering::SeqCst);
    pc.instance_drop_panics.store(false, Ordering::SeqCst);
    let want = match (&stop, shape.factory_drop_panics) {
        (StopReason::Failure(_), _) | (_, false) => stop.clone(),
        (_, true) => StopReason::Failure(ActorError::Panic(FACTORY_DROP_PANIC.to_string())),
    };
    assert_eq!(
        reason, want,
        "the factory's drop panic reads the same wherever the factory drops"
    );
    assert_eq!(
        rig.knobs.stopped.lock().unwrap().clone(),
        vec![stop],
        "on_stopped ran before the teardown"
    );
    assert_eq!(pc.built.load(Ordering::SeqCst), 2, "the attempt built");
    assert_eq!(
        pc.started.load(Ordering::SeqCst),
        1,
        "the attempt started no incarnation"
    );
    assert_eq!(
        pc.live.load(Ordering::SeqCst),
        0,
        "no instance outlives the stop"
    );
    assert_eq!(
        pc.instance_drop_panicked.load(Ordering::SeqCst),
        usize::from(shape.value_drop_panics),
        "the built instance's drop ran outside the factory's unwind"
    );
}

#[test]
fn a_factory_whose_attempt_is_inside_the_build_drops_with_the_teardown() {
    let shape = InBuild {
        pool: false,
        factory_drop_panics: true,
        value_drop_panics: false,
    };
    common::run_multi_thread_only(
        4,
        stop_with_an_attempt_inside_its_build(shape, StopReason::Graceful),
    );
}

#[test]
fn a_factory_and_an_instance_whose_drops_both_panic_in_an_attempt_read_the_factory_panic() {
    let shape = InBuild {
        pool: false,
        factory_drop_panics: true,
        value_drop_panics: true,
    };
    common::run_multi_thread_only(
        4,
        stop_with_an_attempt_inside_its_build(shape, StopReason::Graceful),
    );
}

#[test]
fn an_attempt_inside_its_build_at_the_teardown_starts_nothing() {
    let shape = InBuild {
        pool: false,
        factory_drop_panics: false,
        value_drop_panics: false,
    };
    common::run_multi_thread_only(
        4,
        stop_with_an_attempt_inside_its_build(shape, StopReason::Graceful),
    );
}

#[test]
fn a_pool_attempt_inside_its_build_at_the_teardown_starts_nothing() {
    let shape = InBuild {
        pool: true,
        factory_drop_panics: false,
        value_drop_panics: false,
    };
    common::run_multi_thread_only(
        4,
        stop_with_an_attempt_inside_its_build(shape, StopReason::Graceful),
    );
}

#[test]
fn a_pool_attempt_inside_its_build_reads_the_factory_panic() {
    let shape = InBuild {
        pool: true,
        factory_drop_panics: true,
        value_drop_panics: false,
    };
    common::run_multi_thread_only(
        4,
        stop_with_an_attempt_inside_its_build(shape, StopReason::Graceful),
    );
}

#[test]
fn a_pool_attempt_inside_its_build_keeps_an_earlier_failure() {
    let shape = InBuild {
        pool: true,
        factory_drop_panics: true,
        value_drop_panics: false,
    };
    common::run_multi_thread_only(
        4,
        stop_with_an_attempt_inside_its_build(
            shape,
            StopReason::Failure(ActorError::user("an earlier failure")),
        ),
    );
}

/// A `Kill` reaches the supervisor while a restart attempt is inside its
/// build and the teardown has released the child's factory. The attempt
/// task is aborted, but the build is a blocking call, so the attempt runs
/// on until its first await: it starts no incarnation, and the instance it
/// built drops in the attempt's own task, the one the factory ran in.
#[test]
fn a_killed_supervisor_s_attempt_inside_its_build_drops_its_instance_in_the_attempt() {
    common::run_multi_thread_only(4, async {
        let rig = supervisor(SupervisionConfig::default()).await;
        let c = uname("c");
        let d = uname("d");
        let pc = Arc::new(Probe::default());
        let pd = Arc::new(Probe::default());
        for (name, probe) in [(&c, &pc), (&d, &pd)] {
            rig.spawn_child(
                name,
                probe,
                Shutdown::Timeout(Duration::from_secs(2)),
                RestartType::Permanent,
                0,
            )
            .await;
        }
        wait_until(BOUND, "c and d to start", || {
            pc.started.load(Ordering::SeqCst) == 1 && pd.started.load(Ordering::SeqCst) == 1
        })
        .await;
        pc.factory_hold.store(true, Ordering::SeqCst);
        crash(&child_handle(&c).await).await;
        wait_until(BOUND, "the attempt to enter c's factory", || {
            pc.in_factory.load(Ordering::SeqCst)
        })
        .await;
        pd.stop_held.store(true, Ordering::SeqCst);
        rig.sup.stop(StopReason::Graceful).await.expect("accepted");
        wait_until(BOUND, "the teardown to reach d", || {
            pd.pre_stops.load(Ordering::SeqCst) == 1
        })
        .await;
        rig.sup.stop(StopReason::Kill).await.expect("accepted");
        let reason = timeout(BOUND, rig.sup.wait_stopped())
            .await
            .expect("the supervisor ends");
        assert_eq!(reason, StopReason::Kill);
        pc.factory_hold.store(false, Ordering::SeqCst);
        pd.stop_held.store(false, Ordering::SeqCst);
        wait_until(BOUND, "the built instance to drop", || {
            pc.drop_tasks.lock().unwrap().len() == 2
        })
        .await;
        let built_in = pc.build_tasks.lock().unwrap()[1];
        let dropped_in = pc.drop_tasks.lock().unwrap()[1];
        assert!(built_in.is_some(), "the factory ran in a task");
        assert_eq!(
            dropped_in, built_in,
            "the instance dropped in the attempt's task"
        );
        assert_eq!(
            pc.started.load(Ordering::SeqCst),
            1,
            "no incarnation started"
        );
    });
}

/// A child's restart attempt is in flight when its supervisor stops, its
/// build already done. An attempt holds the factory only while it builds,
/// when it can hold the last reference (see the tests of an attempt inside
/// its build); past its build it holds none, so the factory drops with the
/// supervisor's teardown, before its reason is final. Its panic makes the
/// graceful stop a panic, after `on_stopped` received `Graceful`. The
/// attempt's incarnation stays held in its initialization until the
/// supervisor has ended, so the attempt is still in flight at the teardown
/// on every runtime flavor.
#[test]
fn a_factory_whose_attempt_is_in_flight_drops_with_the_teardown() {
    common::run(0, async {
        let rig = supervisor(SupervisionConfig::default()).await;
        let c = uname("c");
        let pc = Arc::new(Probe::default());
        rig.spawn_child(
            &c,
            &pc,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        wait_until(BOUND, "c to start", || {
            pc.started.load(Ordering::SeqCst) == 1
        })
        .await;
        // c's restart attempt builds its instance, held in its initialization.
        pc.held.store(true, Ordering::SeqCst);
        crash(&child_handle(&c).await).await;
        wait_until(BOUND, "the attempt to build c", || {
            pc.built.load(Ordering::SeqCst) == 2
        })
        .await;
        pc.factory_drop_panics.store(true, Ordering::SeqCst);
        rig.sup.stop(StopReason::Graceful).await.expect("accepted");
        let reason = timeout(BOUND, rig.sup.wait_stopped())
            .await
            .expect("the supervisor ends");
        pc.held.store(false, Ordering::SeqCst);
        pc.factory_drop_panics.store(false, Ordering::SeqCst);
        assert_eq!(
            reason,
            StopReason::Failure(ActorError::Panic(FACTORY_DROP_PANIC.to_string()))
        );
        assert_eq!(
            rig.knobs.stopped.lock().unwrap().clone(),
            vec![StopReason::Graceful],
            "on_stopped ran before the teardown"
        );
    });
}
// ---------------------------------------------------------------------------
// A killed supervisor's children: supervisors stop in order, workers are killed
// ---------------------------------------------------------------------------

/// What the members of one tree did, in order.
#[derive(Default)]
struct Tape {
    events: Mutex<Vec<(Instant, String)>>,
    /// The gate a `NodeMsg::BlockOnGate` turn blocks its thread on, and a
    /// `StopWork::UntilOpened` `on_stopped` awaits.
    gate: (Mutex<bool>, std::sync::Condvar),
    /// While set, the factory of every member refuses to build, after it noted
    /// that it was entered.
    factory_refuses: AtomicBool,
    factory_entered: AtomicBool,
}

impl Tape {
    fn note(&self, what: String) {
        self.events.lock().unwrap().push((Instant::now(), what));
    }

    fn all(&self) -> Vec<String> {
        let events = self.events.lock().unwrap();
        events.iter().map(|(_, what)| what.clone()).collect()
    }

    fn count(&self, what: &str) -> usize {
        self.all()
            .iter()
            .filter(|seen| seen.as_str() == what)
            .count()
    }

    /// The position of the `n`th (from 1) occurrence of `what`.
    fn nth(&self, what: &str, n: usize) -> Option<usize> {
        self.all()
            .iter()
            .enumerate()
            .filter(|(_, seen)| seen.as_str() == what)
            .nth(n - 1)
            .map(|(index, _)| index)
    }

    fn index(&self, what: &str) -> Option<usize> {
        self.nth(what, 1)
    }

    fn has(&self, what: &str) -> bool {
        self.index(what).is_some()
    }

    /// The instant of the first occurrence of `what`.
    fn when(&self, what: &str) -> Instant {
        let events = self.events.lock().unwrap();
        events
            .iter()
            .find(|(_, seen)| seen == what)
            .map(|(at, _)| *at)
            .unwrap_or_else(|| panic!("{what} was never noted"))
    }

    /// Whether anything noted starts with `prefix`.
    fn any_with(&self, prefix: &str) -> bool {
        self.all().iter().any(|seen| seen.starts_with(prefix))
    }

    /// Opens the gate: a `NodeMsg::BlockOnGate` turn blocked on it resumes,
    /// and a `StopWork::UntilOpened` `on_stopped` waiting on it returns.
    fn open_gate(&self) {
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
    }
}

/// Opens a tape's gate when dropped, so a turn blocked on it, or an
/// `on_stopped` waiting on it, ends however the test ends.
struct OpensTheGate(Arc<Tape>);

impl Drop for OpensTheGate {
    fn drop(&mut self) {
        self.0.open_gate();
    }
}

fn reason_word(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::Graceful => "graceful",
        StopReason::ParentRequest => "parent_request",
        StopReason::Failure(_) => "failure",
        StopReason::Cancelled => "cancelled",
        StopReason::ParentKilled => "parent_killed",
        StopReason::Kill => "kill",
        #[allow(unreachable_patterns)]
        _ => "other",
    }
}

/// The work a member's `on_stopped` does.
#[derive(Clone, Copy)]
enum StopWork {
    Nothing,
    /// It awaits this long.
    Sleep(Duration),
    /// It awaits for ever.
    Forever,
    /// It awaits until the tape's gate is opened, for at most `GATE_CAP`.
    UntilOpened,
}

/// What a member of a tree is and does. A member with children supervises
/// them: it spawns them in order in `on_started`.
#[derive(Clone)]
struct Member {
    label: String,
    /// Whether it is registered under its label.
    named: bool,
    shutdown: Shutdown,
    restart: RestartType,
    kill_grace: Option<Duration>,
    /// Its `pre_stop` vetoes every request.
    veto: bool,
    stop: StopWork,
    /// It supervises as a pool.
    pool: bool,
    supervisor: bool,
    /// It stops itself in the `on_child_stopped` that reports a restart it
    /// initiated.
    stops_itself_on_restart: bool,
    /// Its `on_stopped` opens the tape's gate and waits, for a bounded time,
    /// until a factory was entered.
    opens_the_gate_on_stopped: bool,
    children: Vec<Member>,
}

impl Member {
    fn worker(label: &str, shutdown: Shutdown) -> Member {
        Member {
            label: label.to_string(),
            named: true,
            shutdown,
            restart: RestartType::Permanent,
            kill_grace: None,
            veto: false,
            stop: StopWork::Nothing,
            pool: false,
            supervisor: false,
            stops_itself_on_restart: false,
            opens_the_gate_on_stopped: false,
            children: Vec::new(),
        }
    }

    fn supervisor(label: &str, shutdown: Shutdown, children: Vec<Member>) -> Member {
        Member {
            supervisor: true,
            children,
            ..Member::worker(label, shutdown)
        }
    }

    fn stopping(mut self, work: StopWork) -> Member {
        self.stop = work;
        self
    }

    fn vetoing(mut self) -> Member {
        self.veto = true;
        self
    }

    fn kill_grace(mut self, grace: Duration) -> Member {
        self.kill_grace = Some(grace);
        self
    }

    fn anonymous(mut self) -> Member {
        self.named = false;
        self.children = self.children.into_iter().map(Member::anonymous).collect();
        self
    }

    fn pool(mut self) -> Member {
        self.pool = true;
        self
    }

    /// The same tree with every label made unique to one test run.
    fn unique(mut self, run: &str) -> Member {
        self.label = format!("{}-{run}", self.label);
        self.children = self.children.into_iter().map(|c| c.unique(run)).collect();
        self
    }
}

/// A member of a tree: records what it does on a shared tape.
struct Node {
    member: Member,
    tape: Arc<Tape>,
}

#[derive(Clone)]
enum NodeMsg {
    /// A turn that never ends; noted as `parked:<label>` when it begins.
    Park,
    /// A turn that blocks its thread until the tape's gate is opened; noted
    /// as `blocked:<label>` when it begins and `unblocked:<label>` when it
    /// ends.
    BlockOnGate,
    /// A turn that holds until released, after signalling that it began.
    Hold(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>),
    Crash,
}

impl Actor for Node {
    type Message = NodeMsg;
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        self.tape.note(format!("started:{}", self.member.label));
        for child in self.member.children.clone() {
            let tape = self.tape.clone();
            let spec = child.clone();
            let mut builder = ctx
                .spawn_child(move || {
                    if tape.factory_refuses.load(Ordering::SeqCst) {
                        tape.factory_entered.store(true, Ordering::SeqCst);
                        panic!("the factory refuses to build");
                    }
                    Node {
                        member: spec.clone(),
                        tape: tape.clone(),
                    }
                })
                .shutdown(child.shutdown)
                .restart_type(child.restart);
            if child.named {
                builder = builder.named(child.label.clone());
            }
            if let Some(grace) = child.kill_grace {
                builder = builder.kill_grace(grace);
            }
            if child.supervisor {
                let config = if child.pool {
                    ActorConfig::default().with_supervision(pool())
                } else {
                    ActorConfig::default().supervisor()
                };
                builder = builder.with_config(config);
            }
            builder.await?;
        }
        Ok(())
    }

    async fn handle(&mut self, msg: NodeMsg, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let label = self.member.label.clone();
        match msg {
            NodeMsg::Park => {
                self.tape.note(format!("parked:{label}"));
                std::future::pending::<()>().await;
            }
            NodeMsg::BlockOnGate => {
                self.tape.note(format!("blocked:{label}"));
                let (open, opened) = &self.tape.gate;
                let mut open = open.lock().unwrap();
                while !*open {
                    open = opened.wait(open).unwrap();
                }
                drop(open);
                self.tape.note(format!("unblocked:{label}"));
            }
            NodeMsg::Hold(entered, release) => {
                entered.notify_one();
                release.notified().await;
            }
            NodeMsg::Crash => panic!("node crashed on command"),
        }
        Ok(())
    }

    async fn pre_stop(&mut self, _reason: &StopReason, _ctx: &mut ActorContext<Self>) -> bool {
        self.tape.note(format!("pre_stop:{}", self.member.label));
        !self.member.veto
    }

    async fn on_stopped(
        &mut self,
        reason: &StopReason,
        _ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let label = self.member.label.clone();
        self.tape
            .note(format!("on_stopped:{label}:{}", reason_word(reason)));
        if let StopReason::Failure(err) = reason {
            self.tape.note(format!("failure_text:{label}:{err}"));
        }
        if self.member.opens_the_gate_on_stopped {
            self.tape.open_gate();
            let waited = Instant::now();
            while !self.tape.factory_entered.load(Ordering::SeqCst)
                && waited.elapsed() < Duration::from_secs(2)
            {
                sleep(Duration::from_millis(1)).await;
            }
        }
        match self.member.stop {
            StopWork::Nothing => {}
            StopWork::Sleep(time) => sleep(time).await,
            StopWork::Forever => std::future::pending::<()>().await,
            StopWork::UntilOpened => {
                let cap = Instant::now() + GATE_CAP;
                while !*self.tape.gate.0.lock().unwrap() && Instant::now() < cap {
                    sleep(Duration::from_millis(5)).await;
                }
            }
        }
        self.tape.note(format!("stopped_done:{label}"));
        Ok(())
    }

    async fn on_child_stopped(
        &mut self,
        event: &ChildEvent,
        ctx: &mut ActorContext<Self>,
    ) -> ActorResult<()> {
        let child = event
            .child_name
            .clone()
            .unwrap_or_else(|| event.child_id.to_string());
        self.tape.note(format!(
            "child_stopped:{}:{child}:{}:{:?}",
            self.member.label,
            reason_word(&event.reason),
            event.action
        ));
        if self.member.stops_itself_on_restart
            && event.action == SupervisionAction::RestartInitiated
        {
            ctx.self_handle()
                .stop(StopReason::Graceful)
                .await
                .expect("the supervisor stops itself");
        }
        Ok(())
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.tape.note(format!("ended:{}", self.member.label));
    }
}

/// The root of a tree: a supervisor on `sys`, registered under its label.
async fn root_of(sys: &Arc<ActorSystem>, member: &Member, tape: &Arc<Tape>) -> ActorHandle<Node> {
    let config = if member.pool {
        ActorConfig::default().with_supervision(pool())
    } else {
        ActorConfig::default().supervisor()
    };
    Node {
        member: member.clone(),
        tape: tape.clone(),
    }
    .spawn()
    .named(member.label.clone())
    .on_system(sys)
    .with_config(config)
    .await
    .expect("the tree's root starts")
}

/// The registered handle of `label`, once it is there.
async fn node_named(sys: &Arc<ActorSystem>, label: &str) -> ActorHandle<Node> {
    let mut found = None;
    wait_until(BOUND, "a member to be registered", || {
        found = sys.get::<Node>(label);
        found.is_some()
    })
    .await;
    found.expect("registered")
}

/// Waits until every label's task has ended.
async fn ended(tape: &Tape, labels: &[&str], bound: Duration) {
    let labels: Vec<String> = labels
        .iter()
        .map(|label| format!("ended:{label}"))
        .collect();
    wait_until(bound, "the members to end", || {
        labels.iter().all(|label| tape.has(label))
    })
    .await;
}

fn unique_sys(base: &str) -> Arc<ActorSystem> {
    ActorSystem::create(uname(base)).unwrap()
}

async fn kill_root(root: &ActorHandle<Node>) {
    within(
        BOUND,
        "the kill to be accepted",
        root.stop(StopReason::Kill),
    )
    .await
    .expect("accepted");
    within(BOUND, "the root to stop", root.wait_stopped()).await;
}

const FIVE: Duration = Duration::from_secs(5);
const THREE: Duration = Duration::from_secs(3);
const TWO: Duration = Duration::from_secs(2);

async fn children_of_a_killed_supervisor_s_supervisor_child_stop_in_order(pool: bool) {
    let run = uname("tree");
    let work = if pool {
        StopWork::Sleep(Duration::from_millis(200))
    } else {
        StopWork::Sleep(Duration::from_millis(60))
    };
    let mut mid = Member::supervisor(
        "mid",
        Shutdown::Timeout(THREE),
        vec![
            Member::worker("a", Shutdown::Timeout(TWO)).stopping(work),
            Member::worker("b", Shutdown::Timeout(TWO)).stopping(work),
            Member::worker("c", Shutdown::Timeout(TWO)).stopping(work),
        ],
    );
    if pool {
        mid = mid.pool();
    }
    let top = Member::supervisor(
        "top",
        Shutdown::Timeout(FIVE),
        vec![mid, Member::worker("side", Shutdown::Timeout(TWO))],
    )
    .unique(&run);
    let label = |name: &str| format!("{name}-{run}");
    let (tape, sys) = (Arc::new(Tape::default()), unique_sys("tree"));
    let root = root_of(&sys, &top, &tape).await;
    let (a, b, c) = (label("a"), label("b"), label("c"));
    for name in [&a, &b, &c] {
        node_named(&sys, name).await;
    }
    wait_until(BOUND, "every member to start", || {
        tape.count(&format!("started:{a}")) == 1
            && tape.count(&format!("started:{b}")) == 1
            && tape.count(&format!("started:{c}")) == 1
    })
    .await;
    let killed_at = Instant::now();
    kill_root(&root).await;
    ended(&tape, &[&label("mid"), &a, &b, &c, &label("side")], BOUND).await;
    assert!(
        killed_at.elapsed() < THREE,
        "the whole tree ended within the middle supervisor's window: {:?}",
        killed_at.elapsed()
    );

    let at = |what: String| {
        tape.index(&what)
            .unwrap_or_else(|| panic!("{what} never happened in {:?}", tape.all()))
    };
    // The middle supervisor runs `on_stopped` with the inherited stop, and its
    // children are stopped by it afterwards.
    let mid_stopped = at(format!("on_stopped:{}:parent_killed", label("mid")));
    let begins = [&a, &b, &c].map(|name| at(format!("on_stopped:{name}:parent_request")));
    let dones = [&a, &b, &c].map(|name| at(format!("stopped_done:{name}")));
    for begin in begins {
        assert!(mid_stopped < begin, "{:?}", tape.all());
    }
    if pool {
        // A pool orders every child at once: all began before any finished.
        assert!(begins.iter().max() < dones.iter().min(), "{:?}", tape.all());
    } else {
        // Newest first, one at a time.
        assert!(
            dones[2] < begins[1] && dones[1] < begins[0],
            "{:?}",
            tape.all()
        );
    }
    // The worker directly under the killed root is killed: no `on_stopped`.
    assert!(
        !tape.any_with(&format!("on_stopped:{}", label("side"))),
        "{:?}",
        tape.all()
    );
    assert!(
        !tape.any_with(&format!("on_stopped:{}", label("top"))),
        "the killed root runs no `on_stopped`"
    );
}

#[test]
fn killed_supervisor_s_supervisor_child_runs_on_stopped_and_stops_its_children_in_order() {
    common::run(0, async {
        // A middle supervisor that is not a pool stops its children newest
        // first, one at a time; a pool orders every child at once.
        children_of_a_killed_supervisor_s_supervisor_child_stop_in_order(false).await;
        children_of_a_killed_supervisor_s_supervisor_child_stop_in_order(true).await;
    });
}

/// A death the middle supervisor has not handled when its guard fires reaches
/// its `on_child_stopped` once, with the supervisor stopping, before its
/// `on_stopped`. Both events land in one supervisor turn, so the runtime has
/// one thread.
#[test]
fn killed_supervisor_s_supervisor_child_delivers_a_pending_death_before_on_stopped() {
    common::run_current_thread_only(async {
        let run = uname("pending");
        let mut mid = Member::supervisor(
            "mid",
            Shutdown::Timeout(THREE),
            vec![Member::worker("x", Shutdown::Timeout(TWO))],
        );
        mid.children[0].restart = RestartType::Temporary;
        let top = Member::supervisor("top", Shutdown::Timeout(FIVE), vec![mid]).unique(&run);
        let label = |name: &str| format!("{name}-{run}");
        let (tape, sys) = (Arc::new(Tape::default()), unique_sys("pending"));
        let root = root_of(&sys, &top, &tape).await;
        let (mid, x) = (
            node_named(&sys, &label("mid")).await,
            node_named(&sys, &label("x")).await,
        );
        // The middle supervisor is inside a turn, so it handles nothing else
        // until the turn is released.
        let (entered, release) = (
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
        );
        mid.notify(NodeMsg::Hold(entered.clone(), release.clone()))
            .await
            .unwrap();
        within(BOUND, "the turn to begin", entered.notified()).await;
        // The child dies while the turn holds: its death waits in the plane.
        x.notify(NodeMsg::Crash).await.unwrap();
        within(BOUND, "the child to stop", x.wait_stopped()).await;
        // The root is killed. Its task ends after it dropped its supervision
        // state, and with a single thread the aborted runner of the middle
        // supervisor has run its guard by the time this wait returns.
        kill_root(&root).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        release.notify_one();
        ended(&tape, &[&label("mid")], BOUND).await;

        let delivered = format!(
            "child_stopped:{}:{}:failure:SupervisorStopping",
            label("mid"),
            label("x")
        );
        assert_eq!(tape.count(&delivered), 1, "{:?}", tape.all());
        let stopped = format!("on_stopped:{}:parent_killed", label("mid"));
        assert!(
            tape.index(&delivered) < tape.index(&stopped),
            "the death is delivered before `on_stopped`: {:?}",
            tape.all()
        );
        assert_eq!(
            tape.count(&format!("started:{}", label("x"))),
            1,
            "and nothing restarts the child"
        );
    });
}

async fn a_slow_orderly_teardown_outlasts_the_kill_grace(zero_grace: bool) {
    let run = uname("slow");
    let mut parent = Member::supervisor(
        "p",
        Shutdown::Timeout(THREE),
        vec![Member::worker("w", Shutdown::Timeout(TWO))
            .stopping(StopWork::Sleep(Duration::from_millis(700)))],
    );
    if zero_grace {
        parent = parent.kill_grace(Duration::ZERO);
    }
    let top = Member::supervisor("s", Shutdown::Timeout(FIVE), vec![parent]).unique(&run);
    let label = |name: &str| format!("{name}-{run}");
    let (tape, sys) = (Arc::new(Tape::default()), unique_sys("slow"));
    let root = root_of(&sys, &top, &tape).await;
    let (p, w) = (
        node_named(&sys, &label("p")).await,
        node_named(&sys, &label("w")).await,
    );
    wait_until(BOUND, "the worker to start", || {
        tape.has(&format!("started:{}", label("w")))
    })
    .await;
    let killed_at = Instant::now();
    within(BOUND, "the kill", sys.kill(&label("s")))
        .await
        .expect("accepted");
    // The window of the middle supervisor's `Shutdown`, the default kill
    // grace and the wait after an abort, each named once above.
    let bound = THREE + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE + Duration::from_millis(500);
    within(bound, "the root to stop", root.wait_stopped()).await;
    within(bound, "the middle supervisor to stop", p.wait_stopped()).await;
    within(bound, "the worker to stop", w.wait_stopped()).await;
    assert!(killed_at.elapsed() < bound, "{:?}", killed_at.elapsed());

    let at = |what: String| {
        tape.index(&what)
            .unwrap_or_else(|| panic!("{what} never happened in {:?}", tape.all()))
    };
    let parent_stopped = at(format!("on_stopped:{}:parent_killed", label("p")));
    let worker_stopped = at(format!("on_stopped:{}:parent_request", label("w")));
    let worker_done = at(format!("stopped_done:{}", label("w")));
    let parent_ended = at(format!("ended:{}", label("p")));
    assert!(parent_stopped < worker_stopped, "{:?}", tape.all());
    assert!(
        worker_done < parent_ended,
        "the middle supervisor's task ends only after its worker finished a stop \
         that outlasted its kill grace: {:?}",
        tape.all()
    );
}

#[test]
fn killed_supervisor_s_supervisor_child_finishes_a_slow_orderly_teardown_past_its_kill_grace() {
    common::run(0, async {
        // The default kill grace, and a twin with none: the log is the same.
        a_slow_orderly_teardown_outlasts_the_kill_grace(false).await;
        a_slow_orderly_teardown_outlasts_the_kill_grace(true).await;
    });
}

#[test]
fn killed_supervisor_s_worker_children_skip_on_stopped() {
    common::run(0, async {
        let run = uname("workers");
        let top = Member::supervisor(
            "top",
            Shutdown::Timeout(FIVE),
            vec![
                Member::worker("one", Shutdown::Timeout(TWO)),
                Member::worker("two", Shutdown::Kill),
                Member::worker("three", Shutdown::Infinity),
            ],
        )
        .unique(&run);
        let label = |name: &str| format!("{name}-{run}");
        let (tape, sys) = (Arc::new(Tape::default()), unique_sys("workers"));
        let root = root_of(&sys, &top, &tape).await;
        let workers = [label("one"), label("two"), label("three")];
        for name in &workers {
            node_named(&sys, name).await;
        }
        wait_until(BOUND, "the workers to start", || {
            workers
                .iter()
                .all(|name| tape.has(&format!("started:{name}")))
        })
        .await;
        kill_root(&root).await;
        ended(
            &tape,
            &[&workers[0], &workers[1], &workers[2]],
            ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE + BOUND,
        )
        .await;
        for name in &workers {
            assert!(
                !tape.any_with(&format!("on_stopped:{name}")),
                "a worker under a killed supervisor is killed, whatever its own \
                 policy: {:?}",
                tape.all()
            );
            assert!(!tape.any_with(&format!("pre_stop:{name}")));
        }
    });
}

#[test]
fn inherited_stop_cannot_be_vetoed() {
    common::run(0, async {
        let run = uname("veto");
        let top = Member::supervisor(
            "top",
            Shutdown::Timeout(FIVE),
            vec![Member::supervisor("mid", Shutdown::Timeout(THREE), Vec::new()).vetoing()],
        )
        .unique(&run);
        let label = |name: &str| format!("{name}-{run}");
        let (tape, sys) = (Arc::new(Tape::default()), unique_sys("veto"));
        let root = root_of(&sys, &top, &tape).await;
        let mid = node_named(&sys, &label("mid")).await;
        kill_root(&root).await;
        within(BOUND, "the middle supervisor to stop", mid.wait_stopped()).await;
        assert!(
            tape.has(&format!("on_stopped:{}:parent_killed", label("mid"))),
            "{:?}",
            tape.all()
        );
        assert!(
            !tape.any_with(&format!("pre_stop:{}", label("mid"))),
            "`pre_stop` does not run for an inherited stop: {:?}",
            tape.all()
        );
    });
}

#[test]
fn inherited_stop_is_bounded_by_the_child_s_own_shutdown() {
    common::run(
        0,
        inherited_stop_is_bounded_by_the_child_s_own_shutdown_body(false),
    );
}

#[test]
fn inherited_stop_is_bounded_by_the_child_s_own_shutdown_paused() {
    common::run_paused(inherited_stop_is_bounded_by_the_child_s_own_shutdown_body(
        true,
    ));
}

async fn inherited_stop_is_bounded_by_the_child_s_own_shutdown_body(paused: bool) {
    let window = Duration::from_millis(300);
    let bound = upper(
        paused,
        window + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE + Duration::from_millis(500),
    );

    // The middle supervisor's child vetoes every request under a long
    // policy of its own: the middle supervisor is killed at its window,
    // and its child takes the Kill path.
    let run = uname("bound-veto");
    let top = Member::supervisor(
        "top",
        Shutdown::Timeout(FIVE),
        vec![Member::supervisor(
            "mid",
            Shutdown::Timeout(window),
            vec![Member::worker("slow", Shutdown::Timeout(Duration::from_secs(10))).vetoing()],
        )],
    )
    .unique(&run);
    let label = |name: &str| format!("{name}-{run}");
    let (tape, sys) = (Arc::new(Tape::default()), unique_sys("bound-veto"));
    let root = root_of(&sys, &top, &tape).await;
    let (mid, slow) = (
        node_named(&sys, &label("mid")).await,
        node_named(&sys, &label("slow")).await,
    );
    wait_until(BOUND, "the child to start", || {
        tape.has(&format!("started:{}", label("slow")))
    })
    .await;
    let killed_at = Instant::now();
    kill_root(&root).await;
    within(bound, "the middle supervisor to stop", mid.wait_stopped()).await;
    within(bound, "its child to stop", slow.wait_stopped()).await;
    assert!(killed_at.elapsed() >= window, "{:?}", killed_at.elapsed());
    assert!(
        tape.has(&format!("on_stopped:{}:parent_killed", label("mid"))),
        "{:?}",
        tape.all()
    );
    assert!(
        !tape.any_with(&format!("on_stopped:{}", label("slow"))),
        "the child took the Kill path: {:?}",
        tape.all()
    );

    // A second leg: a middle supervisor whose own `on_stopped` awaits for
    // ever is stopped within its window, its kill grace and the settle
    // time, which only the abort stage satisfies.
    let run = uname("bound-forever");
    let top = Member::supervisor(
        "top",
        Shutdown::Timeout(FIVE),
        vec![
            Member::supervisor("mid", Shutdown::Timeout(window), Vec::new())
                .stopping(StopWork::Forever),
        ],
    )
    .unique(&run);
    let label = |name: &str| format!("{name}-{run}");
    let (tape, sys) = (Arc::new(Tape::default()), unique_sys("bound-forever"));
    let root = root_of(&sys, &top, &tape).await;
    let mid = node_named(&sys, &label("mid")).await;
    let killed_at = Instant::now();
    kill_root(&root).await;
    within(
        bound,
        "the stuck middle supervisor to stop",
        mid.wait_stopped(),
    )
    .await;
    assert!(killed_at.elapsed() >= window, "{:?}", killed_at.elapsed());
    assert!(
        tape.has(&format!("on_stopped:{}:parent_killed", label("mid"))),
        "{:?}",
        tape.all()
    );
    assert!(
        !tape.has(&format!("stopped_done:{}", label("mid"))),
        "its `on_stopped` never returned: it was aborted"
    );
}

/// A killed supervisor's restart waits for its subtree's orderly stop for as
/// long as the subtree's budget allows. The leaf's `on_stopped` holds until
/// the test opens the gate: the test holds it far longer than the kill graces
/// and the settle time allow, and checks that the restart has not started and
/// the grandparent has not given up, then opens the gate, well inside the
/// leaf's 6 s window and its supervisor's 9 s window.
#[test]
fn killed_supervisor_restart_waits_for_its_orderly_subtree() {
    common::run(0, async {
        // Longer than a wait bounded by kill graces and settle times, with
        // room for a host that stalls; short of the leaf's window by 3.5 s.
        let hold = Duration::from_millis(2500);
        let run = uname("restart");
        let tree = Member::supervisor(
            "g",
            Shutdown::Timeout(FIVE),
            vec![Member::supervisor(
                "p",
                Shutdown::Kill,
                vec![Member::supervisor(
                    "m",
                    Shutdown::Timeout(Duration::from_secs(9)),
                    vec![
                        Member::worker("x", Shutdown::Timeout(Duration::from_secs(6)))
                            .stopping(StopWork::UntilOpened),
                    ],
                )],
            )],
        )
        .unique(&run);
        let label = |name: &str| format!("{name}-{run}");
        let (tape, sys) = (Arc::new(Tape::default()), unique_sys("restart"));
        let _opens = OpensTheGate(tape.clone());
        let g = root_of(&sys, &tree, &tape).await;
        let x = node_named(&sys, &label("x")).await;
        wait_until(BOUND, "the leaf to start", || {
            tape.has(&format!("started:{}", label("x")))
        })
        .await;
        within(BOUND, "the kill", sys.kill(&label("p")))
            .await
            .expect("accepted");
        wait_until(BOUND, "the old leaf to enter its `on_stopped`", || {
            tape.any_with(&format!("on_stopped:{}", label("x")))
        })
        .await;
        // The restarted supervisor starts its own tree again, under the same
        // names, which it can do only after the old one ended.
        let second = |name: &str| tape.nth(&format!("started:{}", label(name)), 2);
        sleep(hold).await;
        assert!(
            second("p").is_none() && !tape.has(&format!("stopped_done:{}", label("x"))),
            "no restart while the old leaf is still in its stop: {:?}",
            tape.all()
        );
        assert!(
            g.is_alive() && !tape.any_with(&format!("on_stopped:{}", label("g"))),
            "the grandparent waits for the old subtree: {:?}",
            tape.all()
        );
        tape.open_gate();
        wait_until(BOUND, "the restarted leaf to start", || {
            second("x").is_some()
        })
        .await;
        within(BOUND, "the old leaf to stop", x.wait_stopped()).await;
        let done = tape
            .index(&format!("stopped_done:{}", label("x")))
            .expect("the old leaf finished its `on_stopped`");
        assert!(
            done < second("p").expect("the supervisor restarted"),
            "the restart starts only after the old subtree finished its stop: {:?}",
            tape.all()
        );
        assert!(done < second("m").expect("its child restarted"));
        sleep(QUIET).await;
        let reports = tape
            .all()
            .iter()
            .filter(|what| {
                what.starts_with(&format!("child_stopped:{}:{}", label("g"), label("p")))
            })
            .count();
        assert_eq!(
            reports,
            1,
            "one end, one restart, and no name collision: {:?}",
            tape.all()
        );
        assert!(g.is_alive(), "the grandparent never gave up on the child");
        assert!(!tape.any_with(&format!("on_stopped:{}", label("g"))));
    });
}

/// A killed supervisor's restart waits for a supervisor grandchild whose
/// orderly stop is bounded by a 30 s `Shutdown` window: that window is part
/// of the previous subtree's stop budget, so the wait outlasts the grandchild's
/// 1.5 s stop of its worker, far longer than the kill graces alone allow,
/// and the restart starts once the previous subtree has ended.
#[test]
fn killed_supervisor_restart_waits_for_a_supervisor_grandchild_s_30_s_window() {
    common::run(0, async {
        let run = uname("window");
        let tree = Member::supervisor(
            "g",
            Shutdown::Timeout(FIVE),
            vec![Member::supervisor(
                "p",
                Shutdown::Kill,
                vec![Member::supervisor(
                    "m",
                    Shutdown::Timeout(Duration::from_secs(30)),
                    vec![Member::worker("x", Shutdown::Timeout(TWO))
                        .stopping(StopWork::Sleep(Duration::from_millis(1500)))],
                )],
            )],
        )
        .unique(&run);
        let label = |name: &str| format!("{name}-{run}");
        let (tape, sys) = (Arc::new(Tape::default()), unique_sys("window"));
        let g = root_of(&sys, &tree, &tape).await;
        let x = node_named(&sys, &label("x")).await;
        wait_until(BOUND, "the leaf to start", || {
            tape.has(&format!("started:{}", label("x")))
        })
        .await;
        within(BOUND, "the kill", sys.kill(&label("p")))
            .await
            .expect("accepted");
        let second = |name: &str| tape.nth(&format!("started:{}", label(name)), 2);
        wait_until(BOUND, "the restarted leaf to start", || {
            second("x").is_some()
        })
        .await;
        within(BOUND, "the previous leaf to stop", x.wait_stopped()).await;
        let done = tape
            .index(&format!("stopped_done:{}", label("x")))
            .expect("the previous leaf finished its `on_stopped`");
        assert!(
            done < second("p").expect("the supervisor restarted"),
            "the restart starts only after the previous subtree finished its stop: {:?}",
            tape.all()
        );
        assert!(g.is_alive(), "the grandparent never gave up on the child");
        assert!(!tape.any_with(&format!("on_stopped:{}", label("g"))));
    });
}

async fn three_level_tree_killed_at_the_root(named: bool) {
    let run = uname("levels");
    let mut top = Member::supervisor(
        "top",
        Shutdown::Timeout(FIVE),
        vec![Member::supervisor(
            "mid",
            Shutdown::Timeout(THREE),
            vec![Member::worker("leaf", Shutdown::Timeout(TWO))],
        )],
    )
    .unique(&run);
    let labels = [
        format!("top-{run}"),
        format!("mid-{run}"),
        format!("leaf-{run}"),
    ];
    // The root is registered under its label either way; its descendants
    // are anonymous when `named` is false.
    if !named {
        top.children = top.children.into_iter().map(Member::anonymous).collect();
    }
    let (tape, sys) = (Arc::new(Tape::default()), unique_sys("levels"));
    let root = root_of(&sys, &top, &tape).await;
    wait_until(BOUND, "the leaf to start", || {
        tape.has(&format!("started:{}", labels[2]))
    })
    .await;
    kill_root(&root).await;
    ended(&tape, &[&labels[0], &labels[1], &labels[2]], BOUND).await;
    let at = |what: String| {
        tape.index(&what)
            .unwrap_or_else(|| panic!("{what} never happened in {:?}", tape.all()))
    };
    assert!(
        at(format!("on_stopped:{}:parent_killed", labels[1]))
            < at(format!("on_stopped:{}:parent_request", labels[2])),
        "the middle supervisor stops its leaf in order: {:?}",
        tape.all()
    );
    assert!(!tape.any_with(&format!("on_stopped:{}", labels[0])));
}

#[test]
fn three_level_tree_killed_at_the_root_named_and_anonymous() {
    common::run(0, async {
        three_level_tree_killed_at_the_root(true).await;
        three_level_tree_killed_at_the_root(false).await;
    });
}

#[test]
fn infinity_supervisor_child_under_a_killed_parent_follows_its_policy() {
    common::run(
        0,
        infinity_supervisor_child_under_a_killed_parent_follows_its_policy_body(false),
    );
}

#[test]
fn infinity_supervisor_child_under_a_killed_parent_follows_its_policy_paused() {
    common::run_paused(
        infinity_supervisor_child_under_a_killed_parent_follows_its_policy_body(true),
    );
}

async fn infinity_supervisor_child_under_a_killed_parent_follows_its_policy_body(paused: bool) {
    // An `Infinity` middle supervisor runs `on_stopped` and its worker's
    // orderly stop however long they take, and the grandparent restarts
    // the killed supervisor only after the whole subtree ended.
    let run = uname("infinity");
    let tree = Member::supervisor(
        "g",
        Shutdown::Timeout(FIVE),
        vec![Member::supervisor(
            "p",
            Shutdown::Kill,
            vec![Member::supervisor(
                "m",
                Shutdown::Infinity,
                vec![Member::worker("x", Shutdown::Timeout(TWO))
                    .stopping(StopWork::Sleep(Duration::from_millis(1500)))],
            )
            .stopping(StopWork::Sleep(Duration::from_millis(300)))],
        )],
    )
    .unique(&run);
    let label = |name: &str| format!("{name}-{run}");
    let (tape, sys) = (Arc::new(Tape::default()), unique_sys("infinity"));
    let g = root_of(&sys, &tree, &tape).await;
    wait_until(BOUND, "the leaf to start", || {
        tape.has(&format!("started:{}", label("x")))
    })
    .await;
    within(BOUND, "the kill", sys.kill(&label("p")))
        .await
        .expect("accepted");
    wait_until(BOUND, "the restarted leaf to start", || {
        tape.nth(&format!("started:{}", label("x")), 2).is_some()
    })
    .await;
    let at = |what: String| {
        tape.index(&what)
            .unwrap_or_else(|| panic!("{what} never happened in {:?}", tape.all()))
    };
    assert!(
        at(format!("on_stopped:{}:parent_killed", label("m")))
            < at(format!("stopped_done:{}", label("m")))
    );
    let restart = tape
        .nth(&format!("started:{}", label("p")), 2)
        .expect("the supervisor restarted");
    for done in [
        format!("stopped_done:{}", label("x")),
        format!("stopped_done:{}", label("m")),
        format!("ended:{}", label("m")),
    ] {
        assert!(
            tape.index(&done).expect("finished") < restart,
            "the restart waited for {done}: {:?}",
            tape.all()
        );
    }
    sleep(QUIET).await;
    assert!(g.is_alive());
    assert_eq!(
        tape.all()
            .iter()
            .filter(|what| what.starts_with(&format!(
                "child_stopped:{}:{}",
                label("g"),
                label("p")
            )))
            .count(),
        1,
        "{:?}",
        tape.all()
    );

    // An `Infinity` worker under a killed supervisor has no orderly
    // stop: it is parked in a turn, so only the abort the reaper holds
    // for it ends it, once its kill grace has passed.
    let run = uname("infinity-worker");
    let tree = Member::supervisor(
        "s",
        Shutdown::Timeout(FIVE),
        vec![Member::worker("w", Shutdown::Infinity)],
    )
    .unique(&run);
    let label = |name: &str| format!("{name}-{run}");
    let (tape, sys) = (Arc::new(Tape::default()), unique_sys("infinity-worker"));
    let root = root_of(&sys, &tree, &tape).await;
    let worker = node_named(&sys, &label("w")).await;
    worker.notify(NodeMsg::Park).await.unwrap();
    wait_until(BOUND, "the worker to park", || {
        tape.has(&format!("parked:{}", label("w")))
    })
    .await;
    let killed_at = Instant::now();
    kill_root(&root).await;
    let bound = default_abort_bound(paused);
    within(
        bound,
        "the parked worker to be aborted",
        worker.wait_stopped(),
    )
    .await;
    assert!(killed_at.elapsed() < bound);
}

/// A worker whose turn blocks its thread for 5 s when its supervisor is
/// killed, under a supervisor child with an `Infinity` window.
#[test]
fn successor_wait_is_bounded_again_once_its_infinity_supervisor_child_ended() {
    // The blocked turn needs real parallelism.
    common::run_multi_thread_only(4, async {
        let run = uname("bounded-again");
        let tree = || {
            Member::supervisor(
                "g",
                Shutdown::Timeout(FIVE),
                vec![Member::supervisor(
                    "p",
                    Shutdown::Kill,
                    vec![
                        Member::supervisor(
                            "q",
                            Shutdown::Infinity,
                            vec![Member::worker("v", Shutdown::Timeout(TWO))
                                .stopping(StopWork::Sleep(Duration::from_millis(1200)))],
                        ),
                        Member::worker("w", Shutdown::Timeout(TWO)),
                    ],
                )],
            )
        };

        // W blocks: G's restart of P waits while Q's subtree runs, then for
        // P's own bound, which is not unbounded any more, and G stops with
        // the verdict while W's turn still runs. The turn holds its thread
        // until the test opens the gate, after the checks.
        let blocking = tree().unique(&format!("blocking-{run}"));
        let label = |name: &str| format!("{name}-blocking-{run}");
        let (tape, sys) = (Arc::new(Tape::default()), unique_sys("bounded-again"));
        let g = root_of(&sys, &blocking, &tape).await;
        let w = node_named(&sys, &label("w")).await;
        let gate = OpensTheGate(tape.clone());
        wait_until(BOUND, "the tree to start", || {
            tape.has(&format!("started:{}", label("v")))
        })
        .await;
        w.notify(NodeMsg::BlockOnGate).await.unwrap();
        wait_until(BOUND, "the turn to begin", || {
            tape.has(&format!("blocked:{}", label("w")))
        })
        .await;
        within(BOUND, "the kill", sys.kill(&label("p")))
            .await
            .expect("accepted");
        // The release bound of P: Q's stand-in 120 ms + settle, plus V's
        // budget, 120 ms + settle, plus the settle time: 540 ms. V's stop
        // takes longer than that, so a bound counted from the kill would
        // expire while Q still runs.
        let release_bound = Duration::from_millis(540);
        within(
            BOUND,
            "the grandparent to stop with the verdict",
            g.wait_stopped(),
        )
        .await;
        assert!(
            !tape.has(&format!("unblocked:{}", label("w"))),
            "the grandparent did not wait for the blocked turn: {:?}",
            tape.all()
        );
        let q_ended = tape.when(&format!("ended:{}", label("q")));
        let g_stopped = tape.when(&format!("on_stopped:{}:failure", label("g")));
        assert!(
            tape.all().iter().any(|what| {
                what.starts_with(&format!("failure_text:{}:", label("g")))
                    && what.contains(&format!("child `{}`", label("p")))
                    && what.contains("did not end in time")
            }),
            "the grandparent stopped with the unresponsive verdict: {:?}",
            tape.all()
        );
        assert!(
            g_stopped >= q_ended,
            "the verdict came after the unbounded stop ended: {:?}",
            tape.all()
        );
        assert!(
            g_stopped.duration_since(q_ended) <= release_bound + STALL_SLACK,
            "and within P's bound after it: {:?}",
            g_stopped.duration_since(q_ended)
        );
        drop(gate);

        // The control: W idle. The restart waits for Q and W and then
        // happens, with no name collision.
        let idle = tree().unique(&format!("idle-{run}"));
        let label = |name: &str| format!("{name}-idle-{run}");
        let (tape, sys) = (Arc::new(Tape::default()), unique_sys("bounded-again-idle"));
        let g = root_of(&sys, &idle, &tape).await;
        wait_until(BOUND, "the tree to start", || {
            tape.has(&format!("started:{}", label("v")))
        })
        .await;
        within(BOUND, "the kill", sys.kill(&label("p")))
            .await
            .expect("accepted");
        wait_until(BOUND, "the restarted tree to start", || {
            tape.nth(&format!("started:{}", label("v")), 2).is_some()
        })
        .await;
        let restart = tape.nth(&format!("started:{}", label("p")), 2).unwrap();
        assert!(tape.index(&format!("ended:{}", label("q"))).unwrap() < restart);
        assert!(tape.index(&format!("ended:{}", label("w"))).unwrap() < restart);
        sleep(QUIET).await;
        assert!(g.is_alive());
        assert!(!tape.any_with(&format!("on_stopped:{}", label("g"))));
    });
}

#[test]
fn transient_child_stopped_with_parent_killed_is_not_restarted() {
    common::run(0, async {
        // A restart budget of one: a restart of the `Transient` child would
        // leave none for the `Permanent` one.
        let run = uname("transient");
        let mut transient = Member::worker("transient", Shutdown::Timeout(TWO));
        transient.restart = RestartType::Transient;
        let mut temporary = Member::worker("temporary", Shutdown::Timeout(TWO));
        temporary.restart = RestartType::Temporary;
        let permanent = Member::worker("permanent", Shutdown::Timeout(TWO));
        let top = Member::supervisor(
            "top",
            Shutdown::Timeout(FIVE),
            vec![transient, temporary, permanent],
        )
        .unique(&run);
        let label = |name: &str| format!("{name}-{run}");
        let (tape, sys) = (Arc::new(Tape::default()), unique_sys("transient"));
        let root = Node {
            member: top.clone(),
            tape: tape.clone(),
        }
        .spawn()
        .named(top.label.clone())
        .on_system(&sys)
        .with_supervision(SupervisionConfig::one_for_one().max_restarts(1, Duration::from_secs(60)))
        .await
        .expect("the supervisor starts");
        let event = |name: &str, action: &str| {
            format!(
                "child_stopped:{}:{}:parent_killed:{action}",
                label("top"),
                label(name)
            )
        };
        for name in ["transient", "temporary", "permanent"] {
            let child = node_named(&sys, &label(name)).await;
            // The inherited stop raised by hand is a stop ordered from above.
            child.stop(StopReason::ParentKilled).await.unwrap();
            let action = if name == "permanent" {
                "RestartInitiated"
            } else {
                "Removed"
            };
            wait_until(BOUND, "the end to be reported", || {
                tape.has(&event(name, action))
            })
            .await;
        }
        sleep(QUIET).await;
        assert_eq!(
            tape.count(&format!("started:{}", label("transient"))),
            1,
            "a `Transient` child stopped with the inherited stop is not restarted: {:?}",
            tape.all()
        );
        assert_eq!(tape.count(&format!("started:{}", label("temporary"))), 1);
        assert_eq!(
            tape.count(&format!("started:{}", label("permanent"))),
            2,
            "a `Permanent` child is, and the budget it uses is the only one"
        );
        assert!(root.is_alive(), "no charge was made for the other two");
    });
}

#[test]
fn a_kill_already_stored_on_a_supervisor_child_keeps_its_abort_when_the_parent_is_killed() {
    common::run(
        0,
        a_kill_already_stored_on_a_supervisor_child_keeps_its_abort_when_the_parent_is_killed_body(
            false,
        ),
    );
}

#[test]
fn a_kill_already_stored_on_a_supervisor_child_keeps_its_abort_when_the_parent_is_killed_paused() {
    common::run_paused(
        a_kill_already_stored_on_a_supervisor_child_keeps_its_abort_when_the_parent_is_killed_body(
            true,
        ),
    );
}

async fn a_kill_already_stored_on_a_supervisor_child_keeps_its_abort_when_the_parent_is_killed_body(
    paused: bool,
) {
    // The root stops in order: its teardown asks `mid`, which accepts and
    // parks in `on_stopped`; the root's runner raises `Kill` at the end of
    // `mid`'s window and owes the abort a grace after it. The root is then
    // killed inside that grace. Nothing shows the runner's `Kill`, so the
    // root is killed a margin past the window (300 ms) that leaves most
    // of the grace (1 s) ahead.
    let window = Duration::from_secs(2);
    let grace = Duration::from_secs(1);
    let run = uname("stored-kill");
    let top = Member::supervisor(
        "top",
        Shutdown::Timeout(FIVE),
        vec![
            Member::supervisor("mid", Shutdown::Timeout(window), Vec::new())
                .stopping(StopWork::Forever)
                .kill_grace(grace),
        ],
    )
    .unique(&run);
    let label = |name: &str| format!("{name}-{run}");
    let (tape, sys) = (Arc::new(Tape::default()), unique_sys("stored-kill"));
    let root = root_of(&sys, &top, &tape).await;
    let mid = node_named(&sys, &label("mid")).await;
    root.stop(StopReason::Graceful).await.expect("accepted");
    let asked = Instant::now();
    wait_until(BOUND, "mid to enter on_stopped", || {
        tape.any_with(&format!("on_stopped:{}", label("mid")))
    })
    .await;
    sleep((window + Duration::from_millis(300)).saturating_sub(asked.elapsed())).await;
    root.stop(StopReason::Kill).await.expect("accepted");
    within(BOUND, "the root to stop", root.wait_stopped()).await;
    let owed = upper(
        paused,
        window + grace + ASSUMED_ABORT_SETTLE + Duration::from_millis(500),
    );
    within(Duration::from_secs(20), "mid to stop", mid.wait_stopped()).await;
    assert!(
        asked.elapsed() <= owed,
        "the Kill already raised kept its abort: ended after {:?}, owed by {owed:?}",
        asked.elapsed()
    );
}

/// A supervisor child whose second incarnation never finishes `on_started`.
struct HangsOnRestart {
    starts: Arc<AtomicUsize>,
}

impl Actor for HangsOnRestart {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        if self.starts.fetch_add(1, Ordering::SeqCst) >= 1 {
            std::future::pending::<()>().await;
        }
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        panic!("crash on command");
    }
}

struct HoldsAHangingChild {
    starts: Arc<AtomicUsize>,
    name: String,
}

impl Actor for HoldsAHangingChild {
    type Message = ();
    type Response = ();

    async fn on_started(&mut self, ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        let starts = self.starts.clone();
        ctx.spawn_child(move || HangsOnRestart {
            starts: starts.clone(),
        })
        .named(self.name.clone())
        .restart_type(RestartType::Permanent)
        .shutdown(Shutdown::Infinity)
        .start_timeout(Duration::from_millis(300))
        .kill_grace(THREE)
        .with_config(ActorConfig::default().supervisor())
        .await?;
        Ok(())
    }

    async fn handle(&mut self, _msg: (), _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

#[test]
fn a_start_timeout_kill_on_an_infinity_supervisor_child_keeps_its_abort_when_the_parent_is_killed()
{
    common::run(0, async {
        // The restart attempt raises `Kill` on its stuck incarnation at the
        // start timeout and holds the abort for the kill grace. The parent is
        // killed inside that grace: the `Kill` already raised still ends in
        // an abort, although the child's `Shutdown` is `Infinity`.
        let sys = unique_sys("start-timeout-kill");
        let starts = Arc::new(AtomicUsize::new(0));
        let name = uname("hangs");
        let top = HoldsAHangingChild {
            starts: starts.clone(),
            name: name.clone(),
        }
        .spawn()
        .named(uname("hangs-top"))
        .with_config(ActorConfig::default().supervisor())
        .on_system(&sys)
        .await
        .unwrap();
        let first = node_like::<HangsOnRestart>(&sys, &name).await;
        first.notify(()).await.unwrap();
        wait_until(BOUND, "the second incarnation to hang", || {
            starts.load(Ordering::SeqCst) >= 2
        })
        .await;
        let second = node_like::<HangsOnRestart>(&sys, &name).await;
        // The start timeout (300 ms) has passed well inside the 3 s grace.
        sleep(Duration::from_millis(800)).await;
        top.stop(StopReason::Kill).await.expect("accepted");
        within(BOUND, "the parent to stop", top.wait_stopped()).await;
        within(
            THREE + ASSUMED_ABORT_SETTLE + Duration::from_secs(2),
            "the stuck incarnation to be aborted",
            second.wait_stopped(),
        )
        .await;
    });
}

/// The registered handle of type `A` under `label`, once it is there.
async fn node_like<A: Actor>(sys: &Arc<ActorSystem>, label: &str) -> ActorHandle<A> {
    let mut found = None;
    wait_until(BOUND, "a member to be registered", || {
        found = sys.get::<A>(label);
        found.is_some()
    })
    .await;
    found.expect("registered")
}

/// A `Kill` straight from the loop while a restart attempt is inside its
/// build: the `Kill` path's release of the supervision state takes the
/// factory's spec while the attempt holds its upgrade, so the attempt ends
/// cancelled. The instance it built drops in the attempt's own task, the one
/// the factory ran in, no incarnation starts after the supervisor ended, and
/// the child's name stays free. With `factory_drop_panics`, the factory's
/// drop panics in the attempt too, and with the `tracing` feature that panic
/// is logged once, naming the supervisor and the child: the `Kill` path
/// reads no outcome of the attempt, so the line is its record.
async fn a_direct_kill_inside_the_build(factory_drop_panics: bool) {
    #[cfg(feature = "tracing")]
    let logged = warnings::install();
    let rig = supervisor(SupervisionConfig::default()).await;
    let c = uname("c");
    let pc = Arc::new(Probe::default());
    // Opens the factory gate on every exit, a failed assertion included.
    let _release = ReleaseFactory(pc.clone());
    rig.spawn_child(
        &c,
        &pc,
        Shutdown::Timeout(Duration::from_secs(2)),
        RestartType::Permanent,
        0,
    )
    .await;
    wait_until(BOUND, "c to start", || {
        pc.started.load(Ordering::SeqCst) == 1
    })
    .await;
    pc.factory_hold.store(true, Ordering::SeqCst);
    crash(&child_handle(&c).await).await;
    wait_until(BOUND, "the attempt to enter c's factory", || {
        pc.in_factory.load(Ordering::SeqCst)
    })
    .await;
    pc.factory_drop_panics
        .store(factory_drop_panics, Ordering::SeqCst);
    rig.sup.stop(StopReason::Kill).await.expect("accepted");
    let reason = timeout(BOUND, rig.sup.wait_stopped())
        .await
        .expect("the supervisor ends");
    assert_eq!(reason, StopReason::Kill);
    pc.factory_hold.store(false, Ordering::SeqCst);
    wait_until(BOUND, "the built instance to drop", || {
        pc.drop_tasks.lock().unwrap().len() == 2
    })
    .await;
    let built_in = pc.build_tasks.lock().unwrap()[1];
    let dropped_in = pc.drop_tasks.lock().unwrap()[1];
    assert!(built_in.is_some(), "the factory ran in a task");
    assert_eq!(dropped_in, built_in, "the instance dropped in the attempt");
    assert_eq!(
        pc.started.load(Ordering::SeqCst),
        1,
        "no incarnation started"
    );
    assert_eq!(pc.live.load(Ordering::SeqCst), 0);
    assert!(
        ActorSystem::default().get::<Probed>(&c).is_none(),
        "the child's name is free"
    );
    if factory_drop_panics {
        // The factory drops before the instance, in the same attempt, so its
        // panic was logged before the instance's drop was observed.
        pc.factory_drop_panics.store(false, Ordering::SeqCst);
        #[cfg(feature = "tracing")]
        {
            let supervisor = format!("supervisor={} ", rig.sup.id());
            let lines: Vec<String> = warnings::naming(&logged.lock().unwrap(), &c)
                .into_iter()
                .filter(|line| line.contains(&supervisor) && line.contains(FACTORY_DROP_PANIC))
                .collect();
            assert_eq!(lines.len(), 1, "one line naming the supervisor: {lines:?}");
        }
    }
}

#[test]
fn a_direct_kill_inside_the_build_cancels_the_attempt() {
    common::run_multi_thread_only(4, a_direct_kill_inside_the_build(false));
}

#[test]
fn a_direct_kill_inside_the_build_logs_the_factory_panic_naming_the_supervisor() {
    common::run_multi_thread_only(4, a_direct_kill_inside_the_build(true));
}

/// A graceful stop of a supervisor whose only child's restart attempt is
/// inside its build, the build let go as soon as the stop is accepted: the
/// teardown's release of the factory and the build's end come in either
/// order. When the release comes first the attempt is cancelled and its
/// instance drops in the attempt's own task; when the build ends first the
/// incarnation starts and the teardown stops it. Either way the stop ends
/// `Graceful` and nothing outlives it.
async fn stop_gracefully_with_the_only_child_inside_its_build(pool: bool) {
    let config = if pool {
        self::pool()
    } else {
        SupervisionConfig::default()
    };
    let rig = supervisor(config).await;
    let c = uname("c");
    let pc = Arc::new(Probe::default());
    // Opens the factory gate on every exit, a failed assertion included.
    let _release = ReleaseFactory(pc.clone());
    rig.spawn_child(
        &c,
        &pc,
        Shutdown::Timeout(Duration::from_secs(2)),
        RestartType::Permanent,
        0,
    )
    .await;
    wait_until(BOUND, "c to start", || {
        pc.started.load(Ordering::SeqCst) == 1
    })
    .await;
    pc.factory_hold.store(true, Ordering::SeqCst);
    crash(&child_handle(&c).await).await;
    wait_until(BOUND, "the attempt to enter c's factory", || {
        pc.in_factory.load(Ordering::SeqCst)
    })
    .await;
    rig.sup.stop(StopReason::Graceful).await.expect("accepted");
    pc.factory_hold.store(false, Ordering::SeqCst);
    let reason = timeout(BOUND, rig.sup.wait_stopped())
        .await
        .expect("the supervisor ends");
    assert_eq!(reason, StopReason::Graceful);
    assert_eq!(
        pc.live.load(Ordering::SeqCst),
        0,
        "nothing outlives the stop"
    );
    let built_in = pc.build_tasks.lock().unwrap()[1];
    let dropped_in = pc.drop_tasks.lock().unwrap()[1];
    if dropped_in == built_in {
        assert_eq!(
            pc.started.load(Ordering::SeqCst),
            1,
            "a cancelled attempt starts nothing"
        );
    } else {
        assert_eq!(
            pc.started.load(Ordering::SeqCst),
            2,
            "a build that ended first starts its incarnation"
        );
    }
}

#[test]
fn a_graceful_stop_with_the_only_child_inside_its_build() {
    common::run_multi_thread_only(
        4,
        stop_gracefully_with_the_only_child_inside_its_build(false),
    );
}

#[test]
fn a_graceful_pool_stop_with_the_only_child_inside_its_build() {
    common::run_multi_thread_only(
        4,
        stop_gracefully_with_the_only_child_inside_its_build(true),
    );
}

/// A delivery of a child's end that panics during the run stops the
/// supervisor with that panic, and the panic is the supervisor's last error
/// when `on_stopped` runs, as a delivery's `Err` is.
#[test]
fn a_delivery_panic_during_the_run_is_the_last_error_on_stopped_reads() {
    common::run(0, async {
        let rig = supervisor(plain()).await;
        let c = uname("c");
        let pc = Arc::new(Probe::default());
        rig.spawn_child(
            &c,
            &pc,
            Shutdown::Timeout(Duration::from_secs(2)),
            RestartType::Permanent,
            0,
        )
        .await;
        wait_until(BOUND, "c to start", || {
            pc.started.load(Ordering::SeqCst) == 1
        })
        .await;
        rig.knobs.faults.lock().unwrap().push_back(Fault::Panic);
        crash(&child_handle(&c).await).await;
        let reason = timeout(BOUND, rig.sup.wait_stopped())
            .await
            .expect("the supervisor ends");
        assert!(
            matches!(&reason, StopReason::Failure(ActorError::Panic(text)) if text.contains("the delivery panics")),
            "{reason:?}"
        );
        let last = rig.knobs.last_errors.lock().unwrap().clone();
        assert_eq!(last.len(), 1, "on_stopped ran once: {last:?}");
        assert!(
            last[0]
                .as_deref()
                .is_some_and(|err| err.contains("the delivery panics")),
            "on_stopped read the delivery's panic as the last error: {last:?}"
        );
    });
}

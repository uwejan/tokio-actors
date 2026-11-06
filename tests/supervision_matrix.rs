#![allow(dead_code)]
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

/// The longest a wait for a condition may take. It only limits how long a real
/// hang takes to report; a slow host must not read as one.
const HANG_BOUND: Duration = Duration::from_secs(60);

/// How far past its exact instant an outcome may land on the paused clock:
/// one poll step of a waiting loop and the timers' rounding, far short of any
/// window a test configures.
const PAUSED_SLACK: Duration = Duration::from_millis(30);

/// The upper bound of a timed outcome: its `exact` instant plus
/// [`PAUSED_SLACK`] on the paused clock, where no stalled host can move it,
/// and a hang bound past it on a real-time runtime, loose enough for a host
/// that stalls its threads.
fn upper(paused: bool, exact: Duration) -> Duration {
    if paused {
        exact + PAUSED_SLACK
    } else {
        exact + BOUND
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

/// Polls `pred` until it holds. The bound only limits how long a real hang takes to report. A shorter `bound` from a caller is
/// raised to `HANG_BOUND`.
async fn wait_until(bound: Duration, what: &str, mut pred: impl FnMut() -> bool) {
    let bound = bound.max(HANG_BOUND);
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
    /// Instances that began `on_started`, before its `held` wait.
    began: AtomicUsize,
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
        self.probe.began.fetch_add(1, Ordering::SeqCst);
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
            let cap = Instant::now() + GATE_CAP;
            while !ActorSystem::default()
                .get::<Probed>(&name)
                .is_some_and(|handle| handle.is_alive())
                && Instant::now() < cap
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
            wait: Duration::ZERO,
        })
        .await;
        wait_until(BOUND, "the child to veto the first request", || {
            probe.pre_stops_returned.load(Ordering::SeqCst) == 1
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

// ---------------------------------------------------------------------------
// The group's stop and the parent's stop belong to the runners too
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Calls on a child whose restart is in flight, and on a stuck child
// ---------------------------------------------------------------------------

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

/// Polls until `name` reads alive. The bound only limits how long a real hang takes to report.
async fn wait_for_alive(rig: &Rig, name: &str) {
    let deadline = Instant::now() + HANG_BOUND;
    while !is_alive(rig, name).await {
        assert!(Instant::now() < deadline, "{name} never ran");
        sleep(Duration::from_millis(5)).await;
    }
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
    // the latest, on the exact instant of the paused clock.
    let took = stop_supervisor(
        &rig,
        upper(paused, window + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE),
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
    wait_until(BOUND, "the second incarnation to begin its init", || {
        probe.began.load(Ordering::SeqCst) == 2
    })
    .await;
    let bound = upper(
        paused,
        Duration::from_millis(300) + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE,
    );
    let took = stop_supervisor(&rig, bound).await;
    assert!(took < bound, "{took:?}");
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
                // report is drained first: the roster reads `b` ended once
                // its runner recorded the fate.
                Op::UntilNotAlive(b.clone()),
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
        pc.turn_held.store(true, Ordering::SeqCst);
        let lines = rig
            .script(vec![
                Op::CrashAndWait(a.clone()),
                Op::HeldFail(c.clone()),
                Op::UntilInTurn(pc.clone()),
                // `c` is still in its turn when the call commits: its failure
                // comes after the stop's request.
                Op::StopThenEndTurn(c.clone(), pc.clone()),
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

        // Two failing deliveries: the first error stands.
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
            matches!(&reported[0].reason, StopReason::Failure(err) if err.to_string().contains("one")),
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

/// Polls the supervisor until `children()` reads the child restart-pending.
/// The bound only limits how long a real hang takes to report.
async fn until_pending(rig: &Rig, name: &str) {
    let deadline = Instant::now() + HANG_BOUND;
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
/// restart-pending: no group restart pends over them. The bound only limits how long a real hang takes to report.
async fn until_idle(rig: &Rig, names: &[&str]) {
    let deadline = Instant::now() + HANG_BOUND;
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
        wait_until(BOUND, "W to park", || pw.parked.load(Ordering::SeqCst)).await;
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
        wait_until(BOUND, "W to park", || pw.parked.load(Ordering::SeqCst)).await;
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

        // The supervisor reaches `Stopped` at once, before the child's 2 s
        // window could end: the child is not asked to stop.
        within(
            upper(paused, Duration::ZERO),
            "the supervisor to publish Stopped",
            slow.wait_stopped(),
        )
        .await;
        assert!(started.elapsed() < upper(paused, Duration::ZERO));
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
/// grace and the wait after an abort, and the child's own window
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
    assert!(killed_at.elapsed() < default_abort_bound(paused));
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
    upper(paused, ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE)
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

fn pool() -> SupervisionConfig {
    SupervisionConfig::simple_one_for_one().max_restarts(5, Duration::from_secs(60))
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
    let bound = upper(paused, window + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE);
    // y's window is the short one: its attempt ends long before x's
    // window does, which is when the supervisor's state would be dropped.
    let y_bound = upper(
        paused,
        Duration::from_millis(500) + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE,
    );
    wait_until(y_bound, "y's attempt to be stopped by the teardown", || {
        py.live.load(Ordering::SeqCst) == 0
    })
    .await;
    // The teardown's one step orders y's attempt while x's stop is still being
    // joined: x's first `pre_stop` is held, so x is alive. An attempt that is
    // ordered only when the supervisor's state is dropped ends after x ended.
    assert_eq!(
        px.live.load(Ordering::SeqCst),
        1,
        "y's attempt was ordered while x's stop was still joined"
    );
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
const TWO: Duration = Duration::from_secs(2);

#[test]
fn inherited_stop_is_bounded_by_the_child_s_own_shutdown_paused() {
    common::run_paused(inherited_stop_is_bounded_by_the_child_s_own_shutdown_body(
        true,
    ));
}

async fn inherited_stop_is_bounded_by_the_child_s_own_shutdown_body(paused: bool) {
    let window = Duration::from_millis(300);
    let bound = upper(paused, window + ASSUMED_KILL_GRACE + ASSUMED_ABORT_SETTLE);

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

//! The interval at which the turn-watchdog sweep checks: a quarter of the
//! turn warning threshold, never below 50 ms and never above 1 s.

use std::time::Duration;

use super::sweep_tick;

const FLOOR: Duration = Duration::from_millis(50);
const CEILING: Duration = Duration::from_secs(1);

/// A zero threshold gives the floor and the largest duration gives the
/// ceiling, with no overflow.
#[test]
fn the_extreme_thresholds_give_the_floor_and_the_ceiling() {
    assert_eq!(sweep_tick(Duration::ZERO), FLOOR);
    assert_eq!(sweep_tick(Duration::MAX), CEILING);
}

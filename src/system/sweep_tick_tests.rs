//! The interval at which the turn-watchdog sweep checks: a quarter of the
//! turn warning threshold, never below 50 ms and never above 1 s.

use std::time::Duration;

use super::sweep_tick;

const FLOOR: Duration = Duration::from_millis(50);
const CEILING: Duration = Duration::from_secs(1);

/// A threshold whose quarter lies inside the bounds gives exactly that
/// quarter.
#[test]
fn a_threshold_with_an_in_range_quarter_gives_exactly_the_quarter() {
    assert_eq!(
        sweep_tick(Duration::from_millis(400)),
        Duration::from_millis(100)
    );
    assert_eq!(
        sweep_tick(Duration::from_secs(2)),
        Duration::from_millis(500)
    );
    assert_eq!(
        sweep_tick(Duration::from_millis(204)),
        Duration::from_millis(51)
    );
    assert_eq!(
        sweep_tick(Duration::from_millis(3996)),
        Duration::from_millis(999)
    );
}

/// A threshold whose quarter is below 50 ms gives 50 ms.
#[test]
fn a_small_threshold_gives_the_floor() {
    assert_eq!(sweep_tick(Duration::from_millis(100)), FLOOR);
    assert_eq!(sweep_tick(Duration::from_millis(199)), FLOOR);
    assert_eq!(sweep_tick(Duration::from_nanos(1)), FLOOR);
}

/// A threshold whose quarter is above 1 s gives 1 s.
#[test]
fn a_large_threshold_gives_the_ceiling() {
    assert_eq!(sweep_tick(Duration::from_secs(5)), CEILING);
    assert_eq!(sweep_tick(Duration::from_millis(4004)), CEILING);
    assert_eq!(sweep_tick(Duration::from_secs(3600)), CEILING);
}

/// The 200 ms and 4 s thresholds sit exactly on the bounds and give them.
#[test]
fn the_boundary_thresholds_give_exactly_the_bounds() {
    assert_eq!(sweep_tick(Duration::from_millis(200)), FLOOR);
    assert_eq!(sweep_tick(Duration::from_secs(4)), CEILING);
}

/// A zero threshold gives the floor and the largest duration gives the
/// ceiling, with no overflow.
#[test]
fn the_extreme_thresholds_give_the_floor_and_the_ceiling() {
    assert_eq!(sweep_tick(Duration::ZERO), FLOOR);
    assert_eq!(sweep_tick(Duration::MAX), CEILING);
}

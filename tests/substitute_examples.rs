//! Runs the substitute examples' own code and checks what their actors answered.

#[cfg(feature = "telemetry")]
#[allow(dead_code)]
#[path = "../examples/flood_guard.rs"]
mod flood_guard;
#[allow(dead_code)]
#[path = "../examples/heir_table.rs"]
mod heir_table;
#[allow(dead_code)]
#[path = "../examples/idle_shrink.rs"]
mod idle_shrink;
#[allow(dead_code)]
#[path = "../examples/stash.rs"]
mod stash;

use std::time::Duration;

use tokio::time::timeout;

#[tokio::test]
async fn stash() {
    let out = timeout(Duration::from_secs(30), stash::run())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(out.refused, [3]);
    assert_eq!(out.handled, [1, 2, 4]);
}

#[tokio::test]
async fn heir_table() {
    let out = timeout(Duration::from_secs(30), heir_table::run())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(out.inherited, 2);
    assert_eq!(out.value, Some(20));
}

#[tokio::test]
async fn idle_shrink() {
    let out = timeout(Duration::from_secs(30), idle_shrink::run())
        .await
        .unwrap()
        .unwrap();
    assert!(out.busy >= 1000);
    assert_eq!(out.idle, 0);
}

#[cfg(feature = "telemetry")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flood_guard() {
    use tokio_actors::StopReason;

    let out = timeout(Duration::from_secs(30), flood_guard::run())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(out.killed, std::slice::from_ref(&out.flooded));
    assert_eq!(out.reason, StopReason::Kill);
}

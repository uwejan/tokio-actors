//! `PANIC_CAPTURE_AVAILABLE` reflects the build's panic strategy.

// The documented way for a downstream crate to refuse a `panic = "abort"`
// build: this line must keep compiling in an unwinding test build.
const _: () = assert!(tokio_actors::PANIC_CAPTURE_AVAILABLE);

#[test]
fn panic_capture_is_available_when_unwinding() {
    assert_eq!(
        tokio_actors::PANIC_CAPTURE_AVAILABLE,
        cfg!(panic = "unwind")
    );
}

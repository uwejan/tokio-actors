//! The process map of named systems and the reapers of a system that goes: a
//! dropped system removes only its own slot, and a reaper that holds a
//! pending entry when its system is dropped still fires it at its deadline
//! and then ends.

fn unique(base: &str) -> String {
    format!("{base}-{}", uuid::Uuid::new_v4())
}

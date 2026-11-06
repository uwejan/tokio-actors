//! What an idle actor holds on the heap, and what a bare bounded channel of
//! the same message size holds, measured with a counting allocator. The gate
//! fails when an idle actor takes more than its budget; both figures print
//! with `--nocapture`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::mpsc;
use tokio_actors::{
    actor::{context::ActorContext, Actor, ActorExt},
    ActorConfig, ActorResult,
};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);

// Forwards every call to the system allocator and counts the live bytes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(new_size, Ordering::Relaxed);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

struct Idle;

impl Actor for Idle {
    type Message = [u8; 64];
    type Response = ();

    async fn handle(&mut self, _msg: [u8; 64], _ctx: &mut ActorContext<Self>) -> ActorResult<()> {
        Ok(())
    }
}

/// The most heap bytes one idle actor may hold, for a mailbox of 64 slots and
/// of 1 slot.
const IDLE_ACTOR_BUDGET: usize = 7 * 1024 + 512;

const ACTORS: usize = 1000;

fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_actor_stays_within_its_budget() {
    // Warm the runtime so its own one-off allocations are not counted.
    let warm = Idle.spawn().await.unwrap();
    drop(warm);
    tokio::task::yield_now().await;

    let mut figures = Vec::new();
    for capacity in [1usize, 64] {
        let before = live();
        let mut handles = Vec::with_capacity(ACTORS);
        for _ in 0..ACTORS {
            handles.push(
                Idle.spawn()
                    .with_config(ActorConfig::default().with_mailbox_capacity(capacity))
                    .await
                    .unwrap(),
            );
        }
        tokio::task::yield_now().await;
        let per_actor = (live() - before) / ACTORS;
        figures.push((capacity, per_actor));
        drop(handles);
        tokio::task::yield_now().await;
    }

    let before = live();
    let channels: Vec<_> = (0..ACTORS).map(|_| mpsc::channel::<[u8; 64]>(64)).collect();
    let channel_bytes = (live() - before) / ACTORS;
    drop(channels);
    let before = live();
    let unbounded: Vec<_> = (0..ACTORS)
        .map(|_| mpsc::unbounded_channel::<tokio_actors::actor::ActorEnvelope<Idle>>())
        .collect();
    let unbounded_bytes = (live() - before) / ACTORS;
    drop(unbounded);
    println!(
        "bare unbounded channel heap bytes (actor envelope): {unbounded_bytes}; envelope size {}",
        std::mem::size_of::<tokio_actors::actor::ActorEnvelope<Idle>>()
    );

    println!(
        "idle actor heap bytes: capacity 1 = {}, capacity 64 = {}",
        figures[0].1, figures[1].1
    );
    println!("bare bounded channel heap bytes (64-byte message, capacity 64): {channel_bytes}");
    for (capacity, bytes) in figures {
        assert!(
            bytes <= IDLE_ACTOR_BUDGET,
            "an idle actor with {capacity} slots holds {bytes} bytes, over the {IDLE_ACTOR_BUDGET} budget"
        );
    }
}

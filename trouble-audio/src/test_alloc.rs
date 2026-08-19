//! A byte-counting `#[global_allocator]`, shared by every test that needs to prove real
//! allocation behavior (e.g. `lc3::tests`, `cis::tests`, `cig::tests`) - only one
//! `#[global_allocator]` is allowed per binary, so this lives in one place.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

struct CountingAllocator;

static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);
static FREED_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Total bytes ever allocated so far (monotonic, frees aren't subtracted) - compare a
/// before/after snapshot to measure one operation's allocation.
pub(crate) fn allocated() -> usize {
    ALLOCATED_BYTES.load(Ordering::Relaxed)
}

/// Total bytes ever freed so far (monotonic) - `allocated() - freed()` is the live footprint,
/// for tests proving memory is actually reclaimed.
pub(crate) fn freed() -> usize {
    FREED_BYTES.load(Ordering::Relaxed)
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        FREED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

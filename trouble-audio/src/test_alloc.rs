//! A byte-counting `#[global_allocator]`, shared by every test module that needs to prove real
//! allocation behavior (not just approximate) - e.g. `lc3::tests` checking `heap_bytes()` matches
//! what `Lc3MonoDecoder::new`/`Lc3MonoEncoder::new` really allocate, or `cis::tests` checking a
//! reconnect reuses an existing codec instead of leaking a new one. Only one `#[global_allocator]`
//! is allowed per binary, so this lives in one place rather than being redefined per test module.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

struct CountingAllocator;

static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Total bytes ever passed to [`GlobalAlloc::alloc`] so far - monotonically increasing (frees
/// aren't subtracted), so callers compare a before/after snapshot to measure one operation's
/// allocation, same as any other allocation-counting harness.
pub(crate) fn allocated() -> usize {
    ALLOCATED_BYTES.load(Ordering::Relaxed)
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

//! Heap accounting for finding what an import keeps in memory.
//!
//! Built with the `mem-stats` feature, the `geors` binary installs
//! [`Counting`] as its global allocator, and [`mark`] logs the heap size at
//! each import step: current bytes, the peak since the previous mark, and
//! the change. Without the feature, [`mark`] does nothing.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static LAST: AtomicIsize = AtomicIsize::new(0);

/// System allocator that counts live heap bytes.
pub struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
            grow(new_size);
        }
        p
    }
}

fn grow(n: usize) {
    let now = CURRENT.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

/// Log the heap at an import step (no-op without the `mem-stats` feature).
pub fn mark(step: &str) {
    if !cfg!(feature = "mem-stats") {
        return;
    }
    let mb = |b: usize| (b as f64 / 1048576.0 * 10.0).round() / 10.0;
    let now = CURRENT.load(Ordering::Relaxed);
    let peak = PEAK.swap(now, Ordering::Relaxed);
    let change = now as isize - LAST.swap(now as isize, Ordering::Relaxed);
    tracing::info!(
        heap_mb = mb(now),
        peak_mb = mb(peak),
        change_mb = (change as f64 / 1048576.0 * 10.0).round() / 10.0,
        "mem: {step}"
    );
}

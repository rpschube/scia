//! A counting global allocator for "this path must not allocate" tests.
//!
//! Besides counting, it records a backtrace and the thread name for every
//! allocation that happens while a watch window is open, so a failure names
//! its source instead of just reporting a number. The recording itself
//! allocates; a thread-local re-entrancy guard keeps those allocations out of
//! the count and out of the records.
#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::backtrace::Backtrace;
use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static WATCHING: AtomicBool = AtomicBool::new(false);
static STRAYS: Mutex<Vec<String>> = Mutex::new(Vec::new());
const MAX_STRAYS: usize = 8;

thread_local! {
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

pub struct CountingAllocator;

impl CountingAllocator {
    fn note(&self, what: &str, size: usize) {
        let entered = IN_HOOK.with(|f| {
            if f.get() {
                return false;
            }
            f.set(true);
            true
        });
        if !entered {
            return; // an allocation made by the recorder itself
        }
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        if WATCHING.load(Ordering::Relaxed) {
            let name = std::thread::current()
                .name()
                .map(str::to_owned)
                .unwrap_or_else(|| "<unnamed>".to_owned());
            let bt = Backtrace::force_capture();
            if let Ok(mut v) = STRAYS.lock()
                && v.len() < MAX_STRAYS
            {
                v.push(format!("{what} {size} bytes on thread `{name}`:\n{bt}"));
            }
        }
        IN_HOOK.with(|f| f.set(false));
    }
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.note("alloc", layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        self.note("realloc", new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.note("alloc_zeroed", layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
}

/// Runs `f` inside a watch window and returns its result, the number of
/// allocations observed, and the recorded stray descriptions.
pub fn watch<T>(f: impl FnOnce() -> T) -> (T, usize, Vec<String>) {
    if let Ok(mut v) = STRAYS.lock() {
        v.clear();
    }
    let before = ALLOCATIONS.load(Ordering::SeqCst);
    WATCHING.store(true, Ordering::SeqCst);
    let out = f();
    WATCHING.store(false, Ordering::SeqCst);
    let after = ALLOCATIONS.load(Ordering::SeqCst);
    let strays = STRAYS.lock().map(|v| v.clone()).unwrap_or_default();
    (out, after - before, strays)
}

/// Asserts that `f` performs no allocations; on failure prints every recorded
/// stray with its backtrace.
pub fn assert_no_alloc<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let (out, n, strays) = watch(f);
    assert!(
        n == 0,
        "{label} allocated {n} time(s):\n{}",
        strays.join("\n---\n")
    );
    out
}

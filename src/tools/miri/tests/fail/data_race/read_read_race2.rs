//@compile-flags: -Zmiri-preemption-rate=0.0
// Avoid accidental synchronization via address reuse inside `thread::spawn`.
//@compile-flags: -Zmiri-address-reuse-cross-thread-rate=0

use std::sync::atomic::{AtomicU16, Ordering};
use std::thread;

// Make sure races between atomic and non-atomic reads are detected.
// This seems harmless but C++ does not allow them, so we can't allow them for now either.
// This test coverse the case where the atomic access come first.
fn main() {
    let a = AtomicU16::new(0);

    thread::scope(|s| {
        s.spawn(|| {
            // We also put a non-atomic access here, but that should *not* be reported.
            let ptr = &a as *const AtomicU16 as *mut u16;
            unsafe { ptr.read() };
            // Then do the atomic access.
            a.load(Ordering::SeqCst);
        });
        s.spawn(|| {
            thread::yield_now();

            let ptr = &a as *const AtomicU16 as *mut u16;
            unsafe { ptr.read() };
            //~^ ERROR: Data race detected between (1) atomic load on thread `unnamed-1` and (2) non-atomic read on thread `unnamed-2`
        });
    });
}

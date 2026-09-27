// Test-only requested-layout accounting, enabled for a single thread/scope.
// Realloc reserves both old and new layouts until the call returns. This is
// conservative requested overlap, not allocator-internal overhead or RSS.
#[allow(unsafe_code)]
pub(crate) mod tracking {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
    };
    thread_local! {
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
        static LIVE: Cell<isize> = const { Cell::new(0) };
        static PEAK: Cell<isize> = const { Cell::new(0) };
        static BASE: Cell<usize> = const { Cell::new(0) };
        static STACK: Cell<usize> = const { Cell::new(0) };
    }
    fn charge(n: isize) {
        let marker = 0u8;
        if ACTIVE.try_with(Cell::get).unwrap_or(false) {
            LIVE.with(|v| {
                v.set(v.get() + n);
                PEAK.with(|p| p.set(p.get().max(v.get())));
            });
            STACK.with(|s| {
                BASE.with(|b| s.set(s.get().max(b.get().abs_diff(&marker as *const _ as usize))))
            });
        }
    }
    struct Tracking;
    unsafe impl GlobalAlloc for Tracking {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            let p = unsafe { System.alloc(l) };
            if !p.is_null() {
                charge(l.size() as isize);
            }
            p
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            charge(-(l.size() as isize));
            unsafe { System.dealloc(p, l) }
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
            charge(n as isize);
            let q = unsafe { System.realloc(p, l, n) };
            charge(-if q.is_null() {
                n as isize
            } else {
                l.size() as isize
            });
            q
        }
    }
    #[global_allocator]
    static ALLOCATOR: Tracking = Tracking;
    pub(crate) fn measure(f: impl FnOnce()) -> (isize, usize) {
        let marker = 0u8;
        LIVE.with(|v| v.set(0));
        PEAK.with(|v| v.set(0));
        STACK.with(|v| v.set(0));
        BASE.with(|v| v.set(&marker as *const _ as usize));
        ACTIVE.with(|v| v.set(true));
        f();
        ACTIVE.with(|v| v.set(false));
        (PEAK.with(Cell::get), STACK.with(Cell::get))
    }
}

//! Bidirectional HWND ↔ WindowId map.
//!
//! HWND values get reused by the kernel; if we let raw HWNDs leak past
//! `tile-win`, an event for a recycled HWND would silently address some
//! other window in `tile-core`'s state. So we mint our own monotonically-
//! increasing `WindowId` and keep both directions of the mapping here.
//!
//! ## Concurrency
//!
//! The previous design used two separate `RwLock`s (one per direction)
//! with a check-then-insert pattern. Two threads calling `intern(h)` for
//! the same HWND could both miss the read, both bump `next`, and end up
//! minting two WindowIds for one HWND — half of subsequent state events
//! would be routed under the new id and half under the old, and the BSP
//! tree would acquire duplicate leaves. (That was the "tabs came away"
//! crash: a duplicate leaf turned into a panic when the layout tree
//! tried to walk an inconsistent split.)
//!
//! Fix: collapse into a single `Mutex` so check-and-insert is atomic.
//! Lock contention is irrelevant here — these calls fire at most a few
//! times per second across the whole daemon.
//!
//! Hook callbacks and the daemon's discovery tick both call `intern`
//! from different threads, so this isn't theoretical.
//!
//! ## Lock ordering
//!
//! Single mutex → no ordering concerns. Previous code had `intern` and
//! `forget` taking `fwd` before `rev`, while `forget_id` took `rev`
//! before `fwd` — a textbook deadlock setup that only avoided going off
//! the rails because contention was so low. With one mutex this whole
//! class of problem is gone.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tile_core::WindowId;

#[cfg(windows)]
use windows::Win32::Foundation::HWND;

#[derive(Default)]
struct Inner {
    fwd: HashMap<isize, WindowId>, // HWND raw value → WindowId
    rev: HashMap<WindowId, isize>,
}

#[derive(Default)]
pub struct HwndMap {
    inner: Mutex<Inner>,
    next: AtomicU64,
}

impl HwndMap {
    pub fn new() -> Self { Self { next: AtomicU64::new(1), inner: Mutex::new(Inner::default()) } }

    #[cfg(windows)]
    pub fn intern(&self, hwnd: HWND) -> WindowId {
        let raw = hwnd.0 as isize;
        let mut inner = self.inner.lock();
        if let Some(id) = inner.fwd.get(&raw).copied() {
            return id;
        }
        let id = WindowId(self.next.fetch_add(1, Ordering::Relaxed));
        inner.fwd.insert(raw, id);
        inner.rev.insert(id, raw);
        id
    }

    #[cfg(windows)]
    pub fn lookup_hwnd(&self, id: WindowId) -> Option<HWND> {
        self.inner.lock().rev.get(&id).copied().map(|raw| HWND(raw as *mut _))
    }

    #[cfg(windows)]
    pub fn forget(&self, hwnd: HWND) -> Option<WindowId> {
        let raw = hwnd.0 as isize;
        let mut inner = self.inner.lock();
        let id = inner.fwd.remove(&raw)?;
        inner.rev.remove(&id);
        Some(id)
    }

    pub fn forget_id(&self, id: WindowId) {
        let mut inner = self.inner.lock();
        if let Some(raw) = inner.rev.remove(&id) {
            inner.fwd.remove(&raw);
        }
    }

    /// Lookup-only: do *not* mint a new id. Used by hook arms that fire
    /// after a window's manageability degrades (e.g. MINIMIZESTART —
    /// the window is iconic by the time the event arrives, so calling
    /// `intern` would race the manageability filter). Returns `None` if
    /// we never tracked this HWND.
    pub fn peek(&self, raw_hwnd: isize) -> Option<WindowId> {
        self.inner.lock().fwd.get(&raw_hwnd).copied()
    }

    /// Snapshot of all tracked `(WindowId, raw_hwnd)` pairs. Used by the
    /// dead-HWND sweep so we don't hold the map lock across Win32 calls.
    pub fn snapshot(&self) -> Vec<(WindowId, isize)> {
        let inner = self.inner.lock();
        inner.rev.iter().map(|(id, raw)| (*id, *raw)).collect()
    }

    /// Atomic compound operation used by the hook's owner-of-tracked
    /// dialog filter: in a single lock acquisition, check whether
    /// `owner_raw` is currently tracked, and (if not) intern `child`.
    /// Returns `None` if the owner *was* tracked (caller should drop
    /// the child as a dialog), `Some(id)` if the child was interned.
    ///
    /// Doing this in two separate calls (`peek` then `intern`) opens
    /// a window where another thread could free the owner between
    /// our checks, and the child would slip into the layout as a
    /// "normal" top-level window. With one mutex acquisition the
    /// race is closed.
    #[cfg(windows)]
    pub fn intern_unless_owned(
        &self,
        child: HWND,
        owner_raw: isize,
    ) -> Option<WindowId> {
        let mut inner = self.inner.lock();
        if inner.fwd.contains_key(&owner_raw) {
            return None;
        }
        let raw = child.0 as isize;
        if let Some(id) = inner.fwd.get(&raw).copied() {
            return Some(id);
        }
        let id = WindowId(self.next.fetch_add(1, Ordering::Relaxed));
        inner.fwd.insert(raw, id);
        inner.rev.insert(id, raw);
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concurrent interns of the same HWND must mint exactly one WindowId.
    /// (The previous two-RwLock design failed this.)
    #[test]
    fn intern_is_atomic_under_concurrency() {
        use std::sync::Arc;
        use std::thread;

        let map = Arc::new(HwndMap::new());
        let raw_hwnd: isize = 0xDEADBEEF;

        let mut handles = Vec::new();
        for _ in 0..16 {
            let m = map.clone();
            handles.push(thread::spawn(move || {
                #[cfg(windows)]
                {
                    let hwnd = windows::Win32::Foundation::HWND(raw_hwnd as *mut _);
                    m.intern(hwnd)
                }
                #[cfg(not(windows))]
                {
                    let _ = (m, raw_hwnd); // silence unused on non-Windows test runs
                    WindowId(0)
                }
            }));
        }
        let ids: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // Every thread must have observed the SAME WindowId for that HWND.
        let first = ids[0];
        for id in &ids {
            assert_eq!(*id, first, "concurrent intern minted multiple ids: {ids:?}");
        }
    }

    /// intern_unless_owned must return None when the owner is currently
    /// tracked, and must NOT have minted a new id for the child as a
    /// side effect. Hook's confirmation-dialog filter depends on this.
    #[cfg(windows)]
    #[test]
    fn intern_unless_owned_rejects_when_owner_tracked() {
        use windows::Win32::Foundation::HWND;
        let map = HwndMap::new();
        let owner = HWND(0xAA00 as *mut _);
        let child = HWND(0xAA01 as *mut _);
        let _owner_id = map.intern(owner);
        let next_before = map.next.load(Ordering::Relaxed);
        let result = map.intern_unless_owned(child, owner.0 as isize);
        assert!(result.is_none(), "owner is tracked; child must be rejected");
        // Did NOT mint a new id for the child as a side effect.
        let next_after = map.next.load(Ordering::Relaxed);
        assert_eq!(next_before, next_after, "rejection must not bump next_id");
        assert!(map.peek(child.0 as isize).is_none(), "child must not be tracked");
    }

    /// Same call when the owner is NOT tracked: must intern the child and
    /// return its fresh WindowId. Subsequent calls return the same id.
    #[cfg(windows)]
    #[test]
    fn intern_unless_owned_admits_when_owner_unknown() {
        use windows::Win32::Foundation::HWND;
        let map = HwndMap::new();
        let child = HWND(0xBB01 as *mut _);
        // owner_raw = 0 (no owner): caller signals "no parent to check."
        let id1 = map.intern_unless_owned(child, 0).expect("should admit");
        let id2 = map.intern_unless_owned(child, 0).expect("should admit");
        assert_eq!(id1, id2, "re-call must return the same id");
    }
}

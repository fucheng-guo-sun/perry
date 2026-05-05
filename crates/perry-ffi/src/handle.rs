//! Handle registry — opaque integer IDs for Rust objects that
//! survive across the FFI boundary (added in v0.5.x of the
//! perry-ffi v0.5 surface — non-breaking; pure additions).
//!
//! Most non-trivial wrappers (mysql2 connection pools, ws clients,
//! ioredis pipelines, even simple ones like lru-cache) need to
//! hand a long-lived Rust object to TypeScript and get it back
//! later. We can't pass Rust ownership directly across `extern "C"`
//! — the runtime can't drop a `Box<MyType>` because it doesn't know
//! `MyType`'s vtable. Instead we register the object in a global
//! [`DashMap`], return a small integer handle to TypeScript, and
//! every method call comes back through the FFI with the handle
//! plus a type-aware downcast.
//!
//! # Layout
//!
//! Single process-wide [`DashMap`] keyed by [`Handle`] (a `i64`).
//! Each `i64` is allocated atomically from a counter starting at 1
//! — `0` is reserved as `INVALID_HANDLE` so `register_handle` can
//! never produce a falsy value (matches JS truthiness semantics
//! for type checks like `if (handle)`).
//!
//! perry-stdlib has its own copy of this same registry (in
//! `crates/perry-stdlib/src/common/handle.rs`). They are separate
//! integer spaces — perry-ffi-allocated handles cannot be looked
//! up via perry-stdlib's `get_handle`, and vice versa. Programs
//! that link both registries (e.g. via the well-known flip) just
//! end up with two `DashMap` statics; each wrapper consults the
//! registry it was compiled against, so handles never collide.
//!
//! # Safety
//!
//! [`get_handle`] / [`get_handle_mut`] return `'static` references
//! by exploiting the fact that DashMap entries are stable while
//! they exist. The caller must not drop the handle (via
//! [`take_handle`] / [`drop_handle`]) while a borrow is live.
//! Single-threaded FFI usage — the typical pattern — has no
//! aliasing problem; multi-threaded wrappers should use
//! [`with_handle`] which scopes the borrow under a closure.

use std::any::Any;
use std::sync::atomic::{AtomicI64, Ordering};

use dashmap::DashMap;
use once_cell::sync::Lazy;

/// Opaque integer handle to a Rust object. `0` is reserved as
/// [`INVALID_HANDLE`]; valid handles start at `1`.
pub type Handle = i64;

/// Sentinel value for "no handle" / null. Never returned by
/// [`register_handle`]; may be passed in by FFI callers when the
/// JS side has `null` / `undefined`.
pub const INVALID_HANDLE: Handle = 0;

static HANDLES: Lazy<DashMap<Handle, Box<dyn Any + Send + Sync>>> = Lazy::new(DashMap::new);
static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);

/// Register `value` under a fresh handle and return the handle.
///
/// `T` must be `Send + Sync + 'static` — the registry is shared
/// across threads (tokio workers may resolve promises that touch
/// handle data while the main thread is also touching it).
pub fn register_handle<T: 'static + Send + Sync>(value: T) -> Handle {
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::SeqCst);
    HANDLES.insert(handle, Box::new(value));
    handle
}

/// Look up a handle and run `f` against the borrowed value.
/// Recommended over [`get_handle`] — the borrow is scoped, so
/// concurrent [`take_handle`] / [`drop_handle`] can't dangle it.
pub fn with_handle<T: 'static + Send + Sync, R, F: FnOnce(&T) -> R>(
    handle: Handle,
    f: F,
) -> Option<R> {
    HANDLES
        .get(&handle)
        .and_then(|entry| entry.value().downcast_ref::<T>().map(f))
}

/// Look up a handle and run `f` against a mutable borrow. Same
/// caveats as [`with_handle`].
pub fn with_handle_mut<T: 'static + Send + Sync, R, F: FnOnce(&mut T) -> R>(
    handle: Handle,
    f: F,
) -> Option<R> {
    HANDLES
        .get_mut(&handle)
        .and_then(|mut entry| entry.value_mut().downcast_mut::<T>().map(f))
}

/// Borrow the handle's value as `&'static T`. The reference is
/// only stable as long as the handle is in the registry — drop
/// or take it while a borrow is outstanding and you've got a
/// dangle. Prefer [`with_handle`] when possible.
pub fn get_handle<T: 'static + Send + Sync>(handle: Handle) -> Option<&'static T> {
    // SAFETY: DashMap entries are heap-allocated `Box<dyn Any>`s
    // whose contents don't move while in the map. The returned
    // reference points into that Box; it stays valid until the
    // entry is removed (which is the caller's responsibility to
    // sequence correctly).
    HANDLES.get(&handle).and_then(|entry| {
        let ptr = entry.value().downcast_ref::<T>()? as *const T;
        Some(unsafe { &*ptr })
    })
}

/// Mutable counterpart to [`get_handle`].
pub fn get_handle_mut<T: 'static + Send + Sync>(handle: Handle) -> Option<&'static mut T> {
    HANDLES.get_mut(&handle).and_then(|mut entry| {
        let ptr = entry.value_mut().downcast_mut::<T>()? as *mut T;
        Some(unsafe { &mut *ptr })
    })
}

/// Remove the handle from the registry and return its value if
/// the type matches. After this, the handle is no longer valid.
pub fn take_handle<T: 'static + Send + Sync>(handle: Handle) -> Option<T> {
    HANDLES
        .remove(&handle)
        .and_then(|(_, boxed)| boxed.downcast::<T>().ok())
        .map(|b| *b)
}

/// Remove a handle and drop its value. Returns `true` if the
/// handle existed.
pub fn drop_handle(handle: Handle) -> bool {
    HANDLES.remove(&handle).is_some()
}

/// True if the handle currently maps to a registered object.
pub fn handle_exists(handle: Handle) -> bool {
    HANDLES.contains_key(&handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_simple_value() {
        let h = register_handle(42_i64);
        assert_ne!(h, INVALID_HANDLE);
        let v = with_handle::<i64, _, _>(h, |v| *v).expect("present");
        assert_eq!(v, 42);
        assert!(drop_handle(h));
        assert!(!handle_exists(h));
    }

    #[test]
    fn mutable_access_persists() {
        struct Counter(u32);
        let h = register_handle(Counter(0));
        with_handle_mut::<Counter, _, _>(h, |c| c.0 += 1).expect("present");
        with_handle_mut::<Counter, _, _>(h, |c| c.0 += 1).expect("present");
        let n = with_handle::<Counter, _, _>(h, |c| c.0).expect("present");
        assert_eq!(n, 2);
        drop_handle(h);
    }

    #[test]
    fn type_mismatch_returns_none() {
        let h = register_handle(42_i64);
        // Same handle, wrong type — no value comes back.
        let r = with_handle::<String, _, _>(h, |s| s.clone());
        assert!(r.is_none());
        drop_handle(h);
    }

    #[test]
    fn handles_are_unique() {
        let a = register_handle(1_i32);
        let b = register_handle(2_i32);
        assert_ne!(a, b);
        drop_handle(a);
        drop_handle(b);
    }
}

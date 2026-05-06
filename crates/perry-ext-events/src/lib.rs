//! Native bindings for Node's `events` module — `EventEmitter`
//! with `on` / `emit` / `removeListener` / `removeAllListeners` /
//! `listenerCount`.
//!
//! First wrapper port that exercises perry-ffi's GC-root-scanner
//! surface (added in v0.5.546). User closures passed to
//! `emitter.on(event, cb)` live inside an `EventEmitterHandle`
//! value in the registry; without an explicit GC scanner, a
//! malloc-triggered GC between `.on()` and `.emit()` would sweep
//! the closure (issue #35 pattern).

use perry_ffi::{
    gc_register_root_scanner, get_handle_mut, iter_handles_of, read_string, register_handle,
    Handle, JsClosure, JsString, RawClosureHeader, StringHeader,
};
use std::collections::HashMap;

/// Event listeners stored as raw closure pointers (i64 to satisfy
/// Send + Sync — the underlying ClosureHeader is managed by
/// perry-runtime's GC, not us).
pub struct EventEmitterHandle {
    listeners: HashMap<String, Vec<i64>>,
}

impl Default for EventEmitterHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl EventEmitterHandle {
    pub fn new() -> Self {
        EventEmitterHandle {
            listeners: HashMap::new(),
        }
    }
}

static EVENTS_GC_REGISTERED: std::sync::Once = std::sync::Once::new();

fn ensure_gc_scanner_registered() {
    EVENTS_GC_REGISTERED.call_once(|| {
        gc_register_root_scanner(scan_events_roots);
    });
}

/// GC root scanner: visit every registered EventEmitterHandle,
/// mark every listener closure pointer as a root.
fn scan_events_roots(mark: &mut dyn FnMut(f64)) {
    iter_handles_of::<EventEmitterHandle, _>(|emitter| {
        for cb_vec in emitter.listeners.values() {
            for &cb in cb_vec.iter() {
                if cb != 0 {
                    // POINTER_TAG (0x7FFD) over the closure pointer.
                    let boxed =
                        f64::from_bits(0x7FFD_0000_0000_0000 | (cb as u64 & 0x0000_FFFF_FFFF_FFFF));
                    mark(boxed);
                }
            }
        }
    });
}

unsafe fn read_str(ptr: *const StringHeader) -> Option<String> {
    let handle = JsString::from_raw(ptr as *mut StringHeader);
    read_string(handle).map(String::from)
}

/// `new EventEmitter()` — returns a handle to the emitter.
#[no_mangle]
pub extern "C" fn js_event_emitter_new() -> Handle {
    ensure_gc_scanner_registered();
    register_handle(EventEmitterHandle::new())
}

/// `emitter.on(eventName, listener)` — register a listener.
/// Returns the emitter handle for chaining.
///
/// # Safety
///
/// `event_name_ptr` must be null or a Perry-runtime `StringHeader`.
/// `callback_ptr` is a raw closure pointer (the runtime's
/// `ClosureHeader` cast to i64); 0 is the no-op sentinel.
#[no_mangle]
pub unsafe extern "C" fn js_event_emitter_on(
    handle: Handle,
    event_name_ptr: *const StringHeader,
    callback_ptr: i64,
) -> Handle {
    ensure_gc_scanner_registered();
    let Some(event_name) = read_str(event_name_ptr) else {
        return handle;
    };
    if callback_ptr == 0 {
        return handle;
    }
    if let Some(emitter) = get_handle_mut::<EventEmitterHandle>(handle) {
        emitter
            .listeners
            .entry(event_name)
            .or_insert_with(Vec::new)
            .push(callback_ptr);
    }
    handle
}

/// `emitter.emit(eventName, arg)` — fire `arg` to every listener.
/// Returns true if any listeners ran.
///
/// # Safety
///
/// `event_name_ptr` must be null or a Perry-runtime `StringHeader`.
#[no_mangle]
pub unsafe extern "C" fn js_event_emitter_emit(
    handle: Handle,
    event_name_ptr: *const StringHeader,
    arg: f64,
) -> bool {
    let Some(event_name) = read_str(event_name_ptr) else {
        return false;
    };
    if let Some(emitter) = get_handle_mut::<EventEmitterHandle>(handle) {
        if let Some(listeners) = emitter.listeners.get(&event_name) {
            if listeners.is_empty() {
                return false;
            }
            // Clone first to avoid borrow conflicts during dispatch.
            let listeners_copy: Vec<i64> = listeners.clone();
            for cb_ptr in listeners_copy {
                if cb_ptr != 0 {
                    let closure = JsClosure::from_raw(cb_ptr as *const RawClosureHeader);
                    let _ = closure.call1(arg);
                }
            }
            return true;
        }
    }
    false
}

/// `emitter.emit(eventName)` — no-args variant.
///
/// # Safety
///
/// `event_name_ptr` must be null or a Perry-runtime `StringHeader`.
#[no_mangle]
pub unsafe extern "C" fn js_event_emitter_emit0(
    handle: Handle,
    event_name_ptr: *const StringHeader,
) -> bool {
    let Some(event_name) = read_str(event_name_ptr) else {
        return false;
    };
    if let Some(emitter) = get_handle_mut::<EventEmitterHandle>(handle) {
        if let Some(listeners) = emitter.listeners.get(&event_name) {
            if listeners.is_empty() {
                return false;
            }
            let listeners_copy: Vec<i64> = listeners.clone();
            for cb_ptr in listeners_copy {
                if cb_ptr != 0 {
                    let closure = JsClosure::from_raw(cb_ptr as *const RawClosureHeader);
                    let _ = closure.call0();
                }
            }
            return true;
        }
    }
    false
}

/// `emitter.removeListener(event, listener)`.
///
/// # Safety
///
/// `event_name_ptr` must be null or a Perry-runtime `StringHeader`.
#[no_mangle]
pub unsafe extern "C" fn js_event_emitter_remove_listener(
    handle: Handle,
    event_name_ptr: *const StringHeader,
    callback_ptr: i64,
) -> Handle {
    let Some(event_name) = read_str(event_name_ptr) else {
        return handle;
    };
    if let Some(emitter) = get_handle_mut::<EventEmitterHandle>(handle) {
        if let Some(listeners) = emitter.listeners.get_mut(&event_name) {
            listeners.retain(|&p| p != callback_ptr);
        }
    }
    handle
}

/// `emitter.removeAllListeners()` (or `(event)` to scope by event).
///
/// # Safety
///
/// `event_name_ptr` must be null or a Perry-runtime `StringHeader`.
#[no_mangle]
pub unsafe extern "C" fn js_event_emitter_remove_all_listeners(
    handle: Handle,
    event_name_ptr: *const StringHeader,
) -> Handle {
    if let Some(emitter) = get_handle_mut::<EventEmitterHandle>(handle) {
        if event_name_ptr.is_null() {
            emitter.listeners.clear();
        } else if let Some(event_name) = read_str(event_name_ptr) {
            emitter.listeners.remove(&event_name);
        }
    }
    handle
}

/// `emitter.listenerCount(eventName)`.
///
/// # Safety
///
/// `event_name_ptr` must be null or a Perry-runtime `StringHeader`.
#[no_mangle]
pub unsafe extern "C" fn js_event_emitter_listener_count(
    handle: Handle,
    event_name_ptr: *const StringHeader,
) -> f64 {
    let Some(event_name) = read_str(event_name_ptr) else {
        return 0.0;
    };
    if let Some(emitter) = get_handle_mut::<EventEmitterHandle>(handle) {
        if let Some(listeners) = emitter.listeners.get(&event_name) {
            return listeners.len() as f64;
        }
    }
    0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use perry_ffi::alloc_string;

    #[test]
    fn new_emitter_starts_empty() {
        let h = js_event_emitter_new();
        let event_name = alloc_string("foo");
        let count = unsafe { js_event_emitter_listener_count(h, event_name.as_raw() as *const _) };
        assert_eq!(count, 0.0);
    }

    #[test]
    fn add_then_count_listeners() {
        let h = js_event_emitter_new();
        let event_name = alloc_string("change");
        // Use a non-zero sentinel for the callback; we never emit
        // here so we don't actually invoke it.
        let _ = unsafe { js_event_emitter_on(h, event_name.as_raw() as *const _, 0xDEADBEEF_i64) };
        let _ = unsafe { js_event_emitter_on(h, event_name.as_raw() as *const _, 0xCAFEBABE_i64) };
        let count = unsafe { js_event_emitter_listener_count(h, event_name.as_raw() as *const _) };
        assert_eq!(count, 2.0);
    }

    #[test]
    fn remove_listener_drops_one() {
        let h = js_event_emitter_new();
        let event_name = alloc_string("data");
        unsafe {
            js_event_emitter_on(h, event_name.as_raw() as *const _, 1);
            js_event_emitter_on(h, event_name.as_raw() as *const _, 2);
            js_event_emitter_remove_listener(h, event_name.as_raw() as *const _, 1);
        }
        let count = unsafe { js_event_emitter_listener_count(h, event_name.as_raw() as *const _) };
        assert_eq!(count, 1.0);
    }

    #[test]
    fn remove_all_clears() {
        let h = js_event_emitter_new();
        let event_name = alloc_string("x");
        unsafe {
            js_event_emitter_on(h, event_name.as_raw() as *const _, 1);
            js_event_emitter_on(h, event_name.as_raw() as *const _, 2);
            js_event_emitter_remove_all_listeners(h, std::ptr::null());
        }
        let count = unsafe { js_event_emitter_listener_count(h, event_name.as_raw() as *const _) };
        assert_eq!(count, 0.0);
    }
}

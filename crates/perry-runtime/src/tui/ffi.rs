//! C ABI surface called by perry-codegen.

use std::sync::Mutex;
use std::sync::OnceLock;

use crate::string::StringHeader;
use crate::value::js_nanbox_pointer;

use super::cell::Grid;
use super::color::Color;
use super::render;
use super::tree::{box_add_child, paint, register, Node};

/// Singleton grid — sized to the current terminal at first render.
static GRID: OnceLock<Mutex<Grid>> = OnceLock::new();

fn grid() -> &'static Mutex<Grid> {
    GRID.get_or_init(|| {
        let (w, h) = current_term_size();
        Mutex::new(Grid::new(w, h))
    })
}

/// Read the current terminal size via TIOCGWINSZ. Falls back to 80x24
/// when stdout isn't a TTY.
fn current_term_size() -> (u16, u16) {
    #[cfg(unix)]
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            return (ws.ws_col, ws.ws_row);
        }
    }
    (80, 24)
}

// ---------------------------------------------------------------------------
// Widget factories
// ---------------------------------------------------------------------------

/// `Text(content)` — single-line text widget. Returns a NaN-boxed
/// POINTER handle.
#[no_mangle]
pub extern "C" fn js_perry_tui_text(content_ptr: *const StringHeader) -> f64 {
    let content = unsafe { read_string(content_ptr) };
    let h = register(Node::Text {
        content,
        fg: Color::Default,
        bg: Color::Default,
        style: super::cell::Style::default(),
    });
    js_nanbox_pointer(h)
}

/// `Box()` — empty container. Children are added via
/// `js_perry_tui_box_add_child`. Style props (flexDirection, gap, …)
/// are set via the `js_perry_tui_box_set_*` family below — typically
/// emitted by the codegen as a follow-up to a Box-with-style call shape
/// `Box({ flexDirection: "row" }, [children])`.
#[no_mangle]
pub extern "C" fn js_perry_tui_box() -> f64 {
    let h = register(Node::Box {
        children: Vec::new(),
        fg: Color::Default,
        bg: Color::Default,
        style: super::style::BoxStyle::default(),
    });
    js_nanbox_pointer(h)
}

/// Mutate a Box's style. Wraps `tree::with_node_mut` so the per-FFI
/// boilerplate stays small. Silently no-ops on non-Box handles.
fn with_box_style_mut(handle: i64, f: impl FnOnce(&mut super::style::BoxStyle)) {
    super::tree::with_node_mut(handle, |n| {
        if let Node::Box { style, .. } = n {
            f(style);
        }
    });
}

/// `Box.flexDirection = "row" | "column"` — emitted by the codegen
/// when a Box style object includes `flexDirection`.
#[no_mangle]
pub extern "C" fn js_perry_tui_box_set_flex_direction(
    handle: i64,
    value_ptr: *const StringHeader,
) -> f64 {
    let s = unsafe { read_string(value_ptr) };
    let dir = super::style::parse_flex_direction(&s);
    with_box_style_mut(handle, |style| style.flex_direction = dir);
    f64::from_bits(0x7FFC_0000_0000_0001)
}

#[no_mangle]
pub extern "C" fn js_perry_tui_box_set_justify_content(
    handle: i64,
    value_ptr: *const StringHeader,
) -> f64 {
    let s = unsafe { read_string(value_ptr) };
    let v = super::style::parse_justify_content(&s);
    with_box_style_mut(handle, |style| style.justify_content = v);
    f64::from_bits(0x7FFC_0000_0000_0001)
}

#[no_mangle]
pub extern "C" fn js_perry_tui_box_set_align_items(
    handle: i64,
    value_ptr: *const StringHeader,
) -> f64 {
    let s = unsafe { read_string(value_ptr) };
    let v = super::style::parse_align_items(&s);
    with_box_style_mut(handle, |style| style.align_items = v);
    f64::from_bits(0x7FFC_0000_0000_0001)
}

#[no_mangle]
pub extern "C" fn js_perry_tui_box_set_gap(handle: i64, gap: f64) -> f64 {
    let g = gap.max(0.0) as u16;
    with_box_style_mut(handle, |style| style.gap = g);
    f64::from_bits(0x7FFC_0000_0000_0001)
}

#[no_mangle]
pub extern "C" fn js_perry_tui_box_set_padding(handle: i64, padding: f64) -> f64 {
    let p = padding.max(0.0) as u16;
    with_box_style_mut(handle, |style| style.padding = p);
    f64::from_bits(0x7FFC_0000_0000_0001)
}

#[no_mangle]
pub extern "C" fn js_perry_tui_box_set_width(handle: i64, width: f64) -> f64 {
    let w = width.max(0.0) as u16;
    with_box_style_mut(handle, |style| style.width = Some(w));
    f64::from_bits(0x7FFC_0000_0000_0001)
}

#[no_mangle]
pub extern "C" fn js_perry_tui_box_set_height(handle: i64, height: f64) -> f64 {
    let h = height.max(0.0) as u16;
    with_box_style_mut(handle, |style| style.height = Some(h));
    f64::from_bits(0x7FFC_0000_0000_0001)
}

/// Append a child to a Box. Both args are unboxed POINTER handles.
#[no_mangle]
pub extern "C" fn js_perry_tui_box_add_child(parent: i64, child: i64) -> f64 {
    box_add_child(parent, child);
    f64::from_bits(0x7FFC_0000_0000_0001) // TAG_UNDEFINED
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

/// `render(root)` — paint one frame. Phase 3 (#358) routes through
/// the Taffy layout pass before paint so flexbox styles take effect.
#[no_mangle]
pub extern "C" fn js_perry_tui_render(root: i64) -> f64 {
    let (w, h) = current_term_size();
    let mut g = grid().lock().unwrap();
    g.resize(w, h);
    g.clear_back();
    let rects = super::layout::compute_layout(root, w, h);
    super::tree::paint_with_layout(&mut g, root, &rects);
    render::flush(&mut g);
    f64::from_bits(0x7FFC_0000_0000_0001)
}

/// Same as `js_perry_tui_render` but exposed to other tui submodules
/// (the render loop in run.rs) without the FFI wrapper.
pub(super) fn paint_root_for_run(root: i64) {
    let (w, h) = current_term_size();
    let mut g = grid().lock().unwrap();
    g.resize(w, h);
    g.clear_back();
    let rects = super::layout::compute_layout(root, w, h);
    super::tree::paint_with_layout(&mut g, root, &rects);
    render::flush(&mut g);
}

/// Initialize the renderer — clear screen and home the cursor.
#[no_mangle]
pub extern "C" fn js_perry_tui_enter() -> f64 {
    render::enter();
    f64::from_bits(0x7FFC_0000_0000_0001)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

unsafe fn read_string(ptr: *const StringHeader) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let len = (*ptr).byte_len as usize;
    let data = (ptr as *const u8).add(std::mem::size_of::<StringHeader>());
    let slice = std::slice::from_raw_parts(data, len);
    String::from_utf8_lossy(slice).into_owned()
}

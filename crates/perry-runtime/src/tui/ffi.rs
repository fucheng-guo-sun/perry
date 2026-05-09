//! C ABI surface called by perry-codegen.

use std::sync::Mutex;
use std::sync::OnceLock;

use crate::string::StringHeader;

use super::cell::Grid;
use super::color::{parse_color, Color};
use super::render;
use super::style::{Edges, Length};
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

/// `Text(content)` — single-line text widget. Returns the raw widget
/// handle as i64; the dispatch table's NR_PTR contract NaN-boxes it.
/// (Returning f64 here works accidentally — Rust compiles
/// `f64::from_bits(u64)` as a register-to-register move so the u64
/// stays in RAX while the f64 ends up in XMM0 with the same bit
/// pattern, and the IR's `call i64` reads RAX. But that's a fragile
/// happenstance; explicit i64 is the canonical contract.)
#[no_mangle]
pub extern "C" fn js_perry_tui_text(content_ptr: *const StringHeader) -> i64 {
    let content = unsafe { read_string(content_ptr) };
    register(Node::Text {
        content,
        fg: Color::Default,
        bg: Color::Default,
        style: super::cell::Style::default(),
    })
}

/// `Text(content, { fg, bg, bold, italic, underline, reverse })` — same as
/// `js_perry_tui_text` but with style props applied. `fg` / `bg` are
/// strings (named palette like `"red"`, hex `#rrggbb`, or empty for
/// default); `style_bits` packs the four boolean style flags into the
/// existing `Style` u8. Used by the codegen when a Text call has a
/// trailing options object literal. (#405 Phase 3.5.)
#[no_mangle]
pub extern "C" fn js_perry_tui_text_styled(
    content_ptr: *const StringHeader,
    fg_ptr: *const StringHeader,
    bg_ptr: *const StringHeader,
    style_bits: f64,
) -> i64 {
    let content = unsafe { read_string(content_ptr) };
    let fg = parse_color(&unsafe { read_string(fg_ptr) });
    let bg = parse_color(&unsafe { read_string(bg_ptr) });
    let bits = style_bits.max(0.0) as u8;
    register(Node::Text {
        content,
        fg,
        bg,
        style: super::cell::Style(bits),
    })
}

/// `Box()` — empty container. Children are added via
/// `js_perry_tui_box_add_child`. Style props (flexDirection, gap, …)
/// are set via the `js_perry_tui_box_set_*` family below — typically
/// emitted by the codegen as a follow-up to a Box-with-style call shape
/// `Box({ flexDirection: "row" }, [children])`.
#[no_mangle]
pub extern "C" fn js_perry_tui_box() -> i64 {
    register(Node::Box {
        children: Vec::new(),
        fg: Color::Default,
        bg: Color::Default,
        style: super::style::BoxStyle::default(),
    })
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
    with_box_style_mut(handle, |style| style.padding = Edges::all(p));
    f64::from_bits(0x7FFC_0000_0000_0001)
}

/// Per-side padding setter — emitted by codegen when the user passes
/// `padding: { top, right, bottom, left }`. Missing fields default to
/// 0 cells. (#405 Phase 3.5.)
#[no_mangle]
pub extern "C" fn js_perry_tui_box_set_padding_each(
    handle: i64,
    top: f64,
    right: f64,
    bottom: f64,
    left: f64,
) -> f64 {
    let edges = Edges {
        top: top.max(0.0) as u16,
        right: right.max(0.0) as u16,
        bottom: bottom.max(0.0) as u16,
        left: left.max(0.0) as u16,
    };
    with_box_style_mut(handle, |style| style.padding = edges);
    f64::from_bits(0x7FFC_0000_0000_0001)
}

#[no_mangle]
pub extern "C" fn js_perry_tui_box_set_width(handle: i64, width: f64) -> f64 {
    let w = width.max(0.0) as u16;
    with_box_style_mut(handle, |style| style.width = Some(Length::Cells(w)));
    f64::from_bits(0x7FFC_0000_0000_0001)
}

#[no_mangle]
pub extern "C" fn js_perry_tui_box_set_height(handle: i64, height: f64) -> f64 {
    let h = height.max(0.0) as u16;
    with_box_style_mut(handle, |style| style.height = Some(Length::Cells(h)));
    f64::from_bits(0x7FFC_0000_0000_0001)
}

/// Percentage-of-parent width. `pct` is 0.0..=100.0; out-of-range
/// values are clamped. (#405 Phase 3.5.)
#[no_mangle]
pub extern "C" fn js_perry_tui_box_set_width_pct(handle: i64, pct: f64) -> f64 {
    let l = Length::percent(pct as f32);
    with_box_style_mut(handle, |style| style.width = Some(l));
    f64::from_bits(0x7FFC_0000_0000_0001)
}

#[no_mangle]
pub extern "C" fn js_perry_tui_box_set_height_pct(handle: i64, pct: f64) -> f64 {
    let l = Length::percent(pct as f32);
    with_box_style_mut(handle, |style| style.height = Some(l));
    f64::from_bits(0x7FFC_0000_0000_0001)
}

#[no_mangle]
pub extern "C" fn js_perry_tui_box_set_flex_grow(handle: i64, grow: f64) -> f64 {
    let g = grow.max(0.0) as u16;
    with_box_style_mut(handle, |style| style.flex_grow = g);
    f64::from_bits(0x7FFC_0000_0000_0001)
}

#[no_mangle]
pub extern "C" fn js_perry_tui_box_set_flex_shrink(handle: i64, shrink: f64) -> f64 {
    let s = shrink.max(0.0) as u16;
    with_box_style_mut(handle, |style| style.flex_shrink = s);
    f64::from_bits(0x7FFC_0000_0000_0001)
}

#[no_mangle]
pub extern "C" fn js_perry_tui_box_set_flex_basis(handle: i64, cells: f64) -> f64 {
    let n = cells.max(0.0) as u16;
    with_box_style_mut(handle, |style| style.flex_basis = Some(Length::Cells(n)));
    f64::from_bits(0x7FFC_0000_0000_0001)
}

#[no_mangle]
pub extern "C" fn js_perry_tui_box_set_flex_basis_pct(handle: i64, pct: f64) -> f64 {
    let l = Length::percent(pct as f32);
    with_box_style_mut(handle, |style| style.flex_basis = Some(l));
    f64::from_bits(0x7FFC_0000_0000_0001)
}

// ---------------------------------------------------------------------------
// Phase 4 widgets — Spacer + ProgressBar.
// ---------------------------------------------------------------------------

/// `Spacer()` — empty Box with `flex_grow: 1`. In a row layout it
/// pushes siblings apart; in a column layout it pushes them up/down.
/// Equivalent to `Box({ flexGrow: 1 })` — provided as its own FFI for
/// the more discoverable name.
#[no_mangle]
pub extern "C" fn js_perry_tui_spacer() -> i64 {
    let mut s = super::style::BoxStyle::default();
    s.flex_grow = 1;
    super::tree::register(Node::Box {
        children: Vec::new(),
        fg: Color::Default,
        bg: Color::Default,
        style: s,
    })
}

/// `ProgressBar(value, max, width)` — renders `[====    ]`-style filled
/// bar. value/max → fraction of `width` cells filled with `=`; the
/// rest are spaces. Brackets are added at both ends so the widget's
/// total width is `width + 2`. Returns a Text widget handle.
#[no_mangle]
pub extern "C" fn js_perry_tui_progress_bar(value: f64, max: f64, width: f64) -> i64 {
    let w = width.max(1.0) as usize;
    let frac = if max > 0.0 {
        (value / max).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = (frac * (w as f64)).round() as usize;
    let mut s = String::with_capacity(w + 2);
    s.push('[');
    for _ in 0..filled {
        s.push('=');
    }
    for _ in filled..w {
        s.push(' ');
    }
    s.push(']');
    super::tree::register(Node::Text {
        content: s,
        fg: Color::Default,
        bg: Color::Default,
        style: super::cell::Style::default(),
    })
}

// ---------------------------------------------------------------------------
// Phase 4.5 widgets — Spinner + Input + List + Select + TextArea.
// ---------------------------------------------------------------------------

/// `Spinner(frame)` — animated character cycling through `-\|/` based
/// on a frame counter. Caller bumps the frame number from a state slot
/// to animate; pass 0 for a static dash. Returns a Text widget.
///
/// Returns the raw widget handle as i64 (NOT NaN-boxed). The dispatch
/// table's NR_PTR contract NaN-boxes the result. Returning f64 here
/// would mismatch the IR-declared i64 return type and clobber the
/// value through the System V ABI (i64 in RAX, f64 in XMM0).
#[no_mangle]
pub extern "C" fn js_perry_tui_spinner(frame: f64) -> i64 {
    const CHARS: [char; 4] = ['-', '\\', '|', '/'];
    let idx = (frame.max(0.0) as usize) % CHARS.len();
    let s = CHARS[idx].to_string();
    super::tree::register(Node::Text {
        content: s,
        fg: Color::Default,
        bg: Color::Default,
        style: super::cell::Style::default(),
    })
}

/// `Input(value)` — single-line text input renderer. The widget shows
/// `value` followed by a `_` cursor character. The user wires their
/// own keypress handler (via `useInput`) that mutates a state slot
/// holding the value; the widget is purely visual. Returns a Text
/// widget.
///
/// v1 limitation: cursor is always at the end of the value. Cursor
/// repositioning (left/right arrow inside text) is a Phase 4.6
/// follow-up — needs per-cell style support so the cursor's char can
/// be rendered with reverse-video at an arbitrary position.
#[no_mangle]
pub extern "C" fn js_perry_tui_input(value_ptr: *const StringHeader) -> i64 {
    let value = unsafe { read_string(value_ptr) };
    let display = format!("{}_", value);
    super::tree::register(Node::Text {
        content: display,
        fg: Color::Default,
        bg: Color::Default,
        style: super::cell::Style::default(),
    })
}

/// Read items from a JS array of strings into an owned `Vec<String>`.
/// Used by List / Select. The array is unboxed at the codegen call
/// site (NA_PTR in the dispatch table); each element is read via
/// `js_array_get_f64_unchecked` and converted to a string via the
/// runtime's `js_jsvalue_to_string`.
fn read_string_array(items_ptr: i64) -> Vec<String> {
    use crate::array::{js_array_get_f64_unchecked, js_array_length, ArrayHeader};
    use crate::value::js_jsvalue_to_string;
    let arr = items_ptr as *const ArrayHeader;
    if arr.is_null() {
        return Vec::new();
    }
    unsafe {
        let len = js_array_length(arr);
        let mut out = Vec::with_capacity(len as usize);
        for i in 0..len {
            let v = js_array_get_f64_unchecked(arr, i);
            let s_ptr = js_jsvalue_to_string(v);
            if s_ptr.is_null() {
                out.push(String::new());
                continue;
            }
            let s_len = (*s_ptr).byte_len as usize;
            let data = (s_ptr as *const u8).add(std::mem::size_of::<StringHeader>());
            let bytes = std::slice::from_raw_parts(data, s_len);
            out.push(String::from_utf8_lossy(bytes).into_owned());
        }
        out
    }
}

/// `List(items, selected)` — vertical list of items as a Box of Text
/// children. The `selected` index (default -1 = no selection) is
/// rendered with reverse-video. Returns a Box handle suitable for
/// adding to a parent layout.
#[no_mangle]
pub extern "C" fn js_perry_tui_list(items_ptr: i64, selected: f64) -> i64 {
    let items = read_string_array(items_ptr);
    let sel = selected as i32;
    let parent = super::tree::register(Node::Box {
        children: Vec::new(),
        fg: Color::Default,
        bg: Color::Default,
        style: super::style::BoxStyle::default(),
    });
    for (i, item) in items.iter().enumerate() {
        let is_sel = i as i32 == sel;
        let style = if is_sel {
            super::cell::Style(super::cell::Style::REVERSE)
        } else {
            super::cell::Style::default()
        };
        let child = super::tree::register(Node::Text {
            content: item.clone(),
            fg: Color::Default,
            bg: Color::Default,
            style,
        });
        super::tree::box_add_child(parent, child);
    }
    parent
}

/// `Select(items, selected)` — alias for `List` with an enforced
/// non-negative selection. Caller's state holds the selected index;
/// this exists as a separate name for readability and so a future
/// v1.5 can diverge (e.g. add a `>` indicator on the selected row).
#[no_mangle]
pub extern "C" fn js_perry_tui_select(items_ptr: i64, selected: f64) -> i64 {
    js_perry_tui_list(items_ptr, selected.max(0.0))
}

/// `TextArea(value)` — multi-line text renderer. Splits `value` on
/// `\n` and emits one Text per line inside a Box (column layout).
/// Like Input, the widget is purely visual — the user wires keypress
/// → state.set themselves. Returns a Box handle.
#[no_mangle]
pub extern "C" fn js_perry_tui_text_area(value_ptr: *const StringHeader) -> i64 {
    let value = unsafe { read_string(value_ptr) };
    let parent = super::tree::register(Node::Box {
        children: Vec::new(),
        fg: Color::Default,
        bg: Color::Default,
        style: super::style::BoxStyle::default(),
    });
    for line in value.split('\n') {
        let child = super::tree::register(Node::Text {
            content: line.to_string(),
            fg: Color::Default,
            bg: Color::Default,
            style: super::cell::Style::default(),
        });
        super::tree::box_add_child(parent, child);
    }
    parent
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

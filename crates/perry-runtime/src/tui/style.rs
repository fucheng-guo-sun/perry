//! Style props for Box widgets — the subset of flexbox we expose in
//! v0.3 (Phase 3 of #358). Mirrors Taffy's `Style` shape but uses
//! integer cell units and string-named enums so the FFI surface stays
//! simple.
//!
//! User-facing TS shape:
//!
//! ```typescript
//! Box({
//!   flexDirection: "row" | "column",
//!   justifyContent: "start" | "center" | "end" | "space-between" | "space-around",
//!   alignItems: "start" | "center" | "end" | "stretch",
//!   gap: number,
//!   padding: number,
//!   width: number,
//!   height: number,
//! }, [child1, child2])
//! ```

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FlexDirection {
    Row,
    #[default]
    Column,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum JustifyContent {
    #[default]
    Start,
    Center,
    End,
    SpaceBetween,
    SpaceAround,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AlignItems {
    #[default]
    Start,
    Center,
    End,
    Stretch,
}

/// A Box's style. Defaults match the v0.1 vertical-stack behavior so
/// existing code keeps working without supplying a style.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct BoxStyle {
    pub flex_direction: FlexDirection,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    /// Cells of space between adjacent children.
    pub gap: u16,
    /// Cells of padding on every side.
    pub padding: u16,
    /// Explicit width in cells. `None` = auto (fill parent or content).
    pub width: Option<u16>,
    /// Explicit height in cells.
    pub height: Option<u16>,
}

/// Parse a flexDirection string into the enum. Unknown strings fall
/// back to Column (the default vertical stack).
pub fn parse_flex_direction(s: &str) -> FlexDirection {
    match s {
        "row" => FlexDirection::Row,
        _ => FlexDirection::Column,
    }
}

pub fn parse_justify_content(s: &str) -> JustifyContent {
    match s {
        "center" => JustifyContent::Center,
        "end" | "flex-end" => JustifyContent::End,
        "space-between" => JustifyContent::SpaceBetween,
        "space-around" => JustifyContent::SpaceAround,
        _ => JustifyContent::Start,
    }
}

pub fn parse_align_items(s: &str) -> AlignItems {
    match s {
        "center" => AlignItems::Center,
        "end" | "flex-end" => AlignItems::End,
        "stretch" => AlignItems::Stretch,
        _ => AlignItems::Start,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flex_direction_parsing() {
        assert_eq!(parse_flex_direction("row"), FlexDirection::Row);
        assert_eq!(parse_flex_direction("column"), FlexDirection::Column);
        assert_eq!(parse_flex_direction(""), FlexDirection::Column);
        assert_eq!(parse_flex_direction("garbage"), FlexDirection::Column);
    }

    #[test]
    fn justify_content_parsing() {
        assert_eq!(parse_justify_content("start"), JustifyContent::Start);
        assert_eq!(parse_justify_content("center"), JustifyContent::Center);
        assert_eq!(parse_justify_content("end"), JustifyContent::End);
        assert_eq!(parse_justify_content("flex-end"), JustifyContent::End);
        assert_eq!(
            parse_justify_content("space-between"),
            JustifyContent::SpaceBetween
        );
        assert_eq!(parse_justify_content("garbage"), JustifyContent::Start);
    }

    #[test]
    fn align_items_parsing() {
        assert_eq!(parse_align_items("start"), AlignItems::Start);
        assert_eq!(parse_align_items("center"), AlignItems::Center);
        assert_eq!(parse_align_items("stretch"), AlignItems::Stretch);
    }

    #[test]
    fn default_box_style_is_column_zero() {
        let s = BoxStyle::default();
        assert_eq!(s.flex_direction, FlexDirection::Column);
        assert_eq!(s.gap, 0);
        assert_eq!(s.padding, 0);
        assert_eq!(s.width, None);
    }
}

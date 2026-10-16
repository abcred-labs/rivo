use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutProfile {
    Wide,
    Overlay,
    Compact,
    TooSmall,
}

pub fn layout_profile(width: u16, height: u16) -> LayoutProfile {
    if width < 60 || height < 20 {
        LayoutProfile::TooSmall
    } else if width >= 120 {
        LayoutProfile::Wide
    } else if width >= 80 {
        LayoutProfile::Overlay
    } else {
        LayoutProfile::Compact
    }
}

pub fn display_width(text: &str) -> usize {
    text.graphemes(true).map(UnicodeWidthStr::width).sum()
}

pub fn colors_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

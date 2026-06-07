//! Translate cell attributes and colors into SGR escape sequences.
//!
//! The renderer emits these so the dumb client terminal reproduces the
//! daemon-side colors/attributes. We compute a full SGR reset+set for a given
//! [`CellAttributes`]; the differ only re-emits when the pen actually changes,
//! so the cost of being explicit here is paid rarely.

use std::fmt::Write;
use termwiz::cell::{CellAttributes, Intensity, Underline};
use termwiz::color::ColorAttribute;

/// Emit the SGR sequence that sets the terminal pen to exactly `attrs`,
/// starting from a reset. Always begins with `\x1b[0m` so no stale attribute
/// leaks across a pen change.
pub fn sgr_for(attrs: &CellAttributes) -> String {
    let mut s = String::from("\x1b[0m");

    match attrs.intensity() {
        Intensity::Bold => s.push_str("\x1b[1m"),
        Intensity::Half => s.push_str("\x1b[2m"),
        Intensity::Normal => {}
    }
    if attrs.italic() {
        s.push_str("\x1b[3m");
    }
    match attrs.underline() {
        Underline::None => {}
        _ => s.push_str("\x1b[4m"),
    }
    if attrs.reverse() {
        s.push_str("\x1b[7m");
    }
    if attrs.invisible() {
        s.push_str("\x1b[8m");
    }
    if attrs.strikethrough() {
        s.push_str("\x1b[9m");
    }
    push_color(&mut s, attrs.foreground(), true);
    push_color(&mut s, attrs.background(), false);
    s
}

fn push_color(s: &mut String, color: ColorAttribute, foreground: bool) {
    let base = if foreground { 38 } else { 48 };
    match color {
        ColorAttribute::Default => {}
        ColorAttribute::PaletteIndex(idx) => {
            let _ = write!(s, "\x1b[{base};5;{idx}m");
        }
        ColorAttribute::TrueColorWithDefaultFallback(rgb)
        | ColorAttribute::TrueColorWithPaletteFallback(rgb, _) => {
            let (r, g, b, _) = rgb.to_srgb_u8();
            let _ = write!(s, "\x1b[{base};2;{r};{g};{b}m");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termwiz::color::ColorAttribute;

    #[test]
    fn default_attrs_is_just_reset() {
        let a = CellAttributes::default();
        assert_eq!(sgr_for(&a), "\x1b[0m");
    }

    #[test]
    fn bold_red_foreground() {
        let mut a = CellAttributes::default();
        a.set_intensity(Intensity::Bold);
        a.set_foreground(ColorAttribute::PaletteIndex(1));
        let s = sgr_for(&a);
        assert!(s.starts_with("\x1b[0m"));
        assert!(s.contains("\x1b[1m"));
        assert!(s.contains("\x1b[38;5;1m"));
    }

    #[test]
    fn truecolor_background() {
        let mut a = CellAttributes::default();
        a.set_background(ColorAttribute::TrueColorWithDefaultFallback(
            (1.0, 0.0, 0.0, 1.0).into(),
        ));
        let s = sgr_for(&a);
        assert!(s.contains("\x1b[48;2;255;0;0m"));
    }
}

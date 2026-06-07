//! tmux-style status bar formatting.
//!
//! Parses format strings containing:
//! - variable tokens: `#S` (session), `#W` (window), `#H` (host), `#I` (window
//!   index), `#P` (pane index),
//! - strftime time tokens: `%H %M %S %d %b %y %Y %m %p` etc.,
//! - style spans: `#[fg=colour,bg=colour,bold]` ... applied to following text.
//!
//! Output is a list of styled spans the renderer paints into the status row,
//! with left / centre / right justification across the three segments
//! (status-left, window list, status-right).

use termwiz::cell::CellAttributes;
use termwiz::color::ColorAttribute;

/// A run of text with uniform attributes.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub text: String,
    pub attrs: CellAttributes,
}

/// One window in the status-bar window list.
pub struct WindowEntry {
    pub index: u32,
    pub name: String,
    pub active: bool,
}

/// Build a tmux-style window list as styled spans: `1:name 2:name* 3:name`,
/// with the active window in reverse video and marked `*`. `base` supplies the
/// bar's background/foreground so inactive entries blend in.
pub fn window_list(entries: &[WindowEntry], base: &CellAttributes) -> Vec<Span> {
    let mut spans = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let marker = if e.active { "*" } else { "" };
        let mut attrs = base.clone();
        if e.active {
            attrs.set_reverse(true);
        }
        spans.push(Span {
            text: format!("{}:{}{marker}", e.index, e.name),
            attrs,
        });
        if i + 1 < entries.len() {
            spans.push(Span {
                text: " ".to_string(),
                attrs: base.clone(),
            });
        }
    }
    spans
}

/// Values substituted into format variable tokens.
#[derive(Debug, Clone, Default)]
pub struct StatusContext {
    pub session: String,
    pub window: String,
    pub window_index: u32,
    pub pane_index: u32,
    pub host: String,
    /// Broken-out local time for strftime tokens (so core stays clock-free).
    pub time: TimeParts,
}

/// Pre-computed local time components. The daemon fills these from the OS; core
/// only formats, keeping it deterministic and testable.
#[derive(Debug, Clone, Default)]
pub struct TimeParts {
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub day: u8,
    pub month: u8,
    pub year: u16,
}

impl TimeParts {
    fn month_abbrev(&self) -> &'static str {
        const M: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        M.get((self.month.max(1) - 1) as usize)
            .copied()
            .unwrap_or("Jan")
    }
}

/// Parse a format string into styled spans, substituting `ctx`.
pub fn format(fmt: &str, ctx: &StatusContext) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut attrs = CellAttributes::default();
    let mut cur = String::new();
    let mut chars = fmt.chars().peekable();

    macro_rules! flush {
        () => {
            if !cur.is_empty() {
                spans.push(Span {
                    text: std::mem::take(&mut cur),
                    attrs: attrs.clone(),
                });
            }
        };
    }

    while let Some(c) = chars.next() {
        match c {
            '#' => match chars.peek() {
                Some('[') => {
                    // Style span: #[...]
                    flush!();
                    chars.next(); // consume '['
                    let mut spec = String::new();
                    for sc in chars.by_ref() {
                        if sc == ']' {
                            break;
                        }
                        spec.push(sc);
                    }
                    apply_style(&mut attrs, &spec);
                }
                Some('#') => {
                    chars.next();
                    cur.push('#');
                }
                Some(&v) => {
                    chars.next();
                    cur.push_str(&substitute_var(v, ctx));
                }
                None => cur.push('#'),
            },
            '%' => match chars.next() {
                Some(t) => cur.push_str(&substitute_time(t, ctx)),
                None => cur.push('%'),
            },
            _ => cur.push(c),
        }
    }
    flush!();
    spans
}

fn substitute_var(v: char, ctx: &StatusContext) -> String {
    match v {
        'S' => ctx.session.clone(),
        'W' => ctx.window.clone(),
        'H' => ctx.host.clone(),
        'I' => ctx.window_index.to_string(),
        'P' => ctx.pane_index.to_string(),
        other => format!("#{other}"),
    }
}

fn substitute_time(t: char, ctx: &StatusContext) -> String {
    let tm = &ctx.time;
    match t {
        'H' => format!("{:02}", tm.hour),
        'M' => format!("{:02}", tm.minute),
        'S' => format!("{:02}", tm.second),
        'd' => format!("{:02}", tm.day),
        'm' => format!("{:02}", tm.month),
        'b' => tm.month_abbrev().to_string(),
        'Y' => format!("{:04}", tm.year),
        'y' => format!("{:02}", tm.year % 100),
        'p' => if tm.hour < 12 { "AM" } else { "PM" }.to_string(),
        '%' => "%".to_string(),
        other => format!("%{other}"),
    }
}

/// Apply a comma-separated style spec like "fg=red,bg=colour24,bold".
fn apply_style(attrs: &mut CellAttributes, spec: &str) {
    for part in spec.split(',') {
        let part = part.trim();
        if let Some(c) = part.strip_prefix("fg=") {
            attrs.set_foreground(parse_color(c));
        } else if let Some(c) = part.strip_prefix("bg=") {
            attrs.set_background(parse_color(c));
        } else {
            match part {
                "bold" => {
                    attrs.set_intensity(termwiz::cell::Intensity::Bold);
                }
                "dim" => {
                    attrs.set_intensity(termwiz::cell::Intensity::Half);
                }
                "underscore" | "underline" => {
                    attrs.set_underline(termwiz::cell::Underline::Single);
                }
                "reverse" => {
                    attrs.set_reverse(true);
                }
                "italics" | "italic" => {
                    attrs.set_italic(true);
                }
                "none" | "default" => {
                    *attrs = CellAttributes::default();
                }
                _ => {}
            }
        }
    }
}

/// Parse a tmux color name or `colourN` / `N` index into a ColorAttribute.
pub fn parse_color(s: &str) -> ColorAttribute {
    let named = match s {
        "black" => Some(0),
        "red" => Some(1),
        "green" => Some(2),
        "yellow" => Some(3),
        "blue" => Some(4),
        "magenta" => Some(5),
        "cyan" => Some(6),
        "white" => Some(7),
        "brightblack" | "grey" | "gray" => Some(8),
        "brightred" => Some(9),
        "brightgreen" => Some(10),
        "brightyellow" => Some(11),
        "brightblue" => Some(12),
        "brightmagenta" => Some(13),
        "brightcyan" => Some(14),
        "brightwhite" => Some(15),
        "default" => return ColorAttribute::Default,
        _ => None,
    };
    if let Some(idx) = named {
        return ColorAttribute::PaletteIndex(idx);
    }
    // colourNNN or plain NNN palette index.
    let digits = s
        .strip_prefix("colour")
        .or_else(|| s.strip_prefix("color"))
        .unwrap_or(s);
    if let Ok(idx) = digits.parse::<u8>() {
        ColorAttribute::PaletteIndex(idx)
    } else {
        ColorAttribute::Default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> StatusContext {
        StatusContext {
            session: "work".into(),
            window: "shell".into(),
            window_index: 1,
            pane_index: 0,
            host: "winhost".into(),
            time: TimeParts {
                hour: 14,
                minute: 5,
                second: 9,
                day: 7,
                month: 6,
                year: 2026,
            },
        }
    }

    fn joined(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn substitutes_session_and_window() {
        let s = format("[#S] #W", &ctx());
        assert_eq!(joined(&s), "[work] shell");
    }

    #[test]
    fn substitutes_host_and_indices() {
        let s = format("#H #I:#P", &ctx());
        assert_eq!(joined(&s), "winhost 1:0");
    }

    #[test]
    fn formats_time_tokens() {
        let s = format("%H:%M %d-%b-%y", &ctx());
        assert_eq!(joined(&s), "14:05 07-Jun-26");
    }

    #[test]
    fn literal_hash_and_percent() {
        let s = format("100## %%", &ctx());
        assert_eq!(joined(&s), "100# %");
    }

    #[test]
    fn style_spans_split_and_carry_attrs() {
        let s = format("#[fg=red,bold]ERR#[default] ok", &ctx());
        // Three spans: "ERR" (red bold), then default-attr " ok".
        assert!(s.iter().any(|sp| sp.text.contains("ERR")));
        let err = s.iter().find(|sp| sp.text == "ERR").unwrap();
        assert_eq!(err.attrs.intensity(), termwiz::cell::Intensity::Bold);
        assert_eq!(err.attrs.foreground(), ColorAttribute::PaletteIndex(1));
    }

    #[test]
    fn parses_colour_index() {
        assert_eq!(parse_color("colour24"), ColorAttribute::PaletteIndex(24));
        assert_eq!(parse_color("124"), ColorAttribute::PaletteIndex(124));
        assert_eq!(parse_color("cyan"), ColorAttribute::PaletteIndex(6));
        assert_eq!(parse_color("default"), ColorAttribute::Default);
    }

    #[test]
    fn window_list_marks_active_and_separates() {
        let entries = vec![
            WindowEntry {
                index: 1,
                name: "bash".into(),
                active: false,
            },
            WindowEntry {
                index: 2,
                name: "vim".into(),
                active: true,
            },
            WindowEntry {
                index: 3,
                name: "logs".into(),
                active: false,
            },
        ];
        let base = CellAttributes::default();
        let spans = window_list(&entries, &base);
        let text: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(text, "1:bash 2:vim* 3:logs");
        // The active entry ("2:vim*") is reverse-video.
        let active = spans.iter().find(|s| s.text == "2:vim*").unwrap();
        assert!(active.attrs.reverse());
        // Inactive entries are not.
        let inactive = spans.iter().find(|s| s.text == "1:bash").unwrap();
        assert!(!inactive.attrs.reverse());
    }
}

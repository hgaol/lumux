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

use crate::render::display_width;

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
        let mut attrs = base.clone();
        if e.active {
            attrs.set_reverse(true);
        }
        spans.push(Span {
            text: entry_text(e),
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

/// The display text for one window entry in the list (must match `window_list`).
fn entry_text(e: &WindowEntry) -> String {
    let marker = if e.active { "*" } else { "" };
    format!("{}:{}{marker}", e.index, e.name)
}

/// Build the window list from per-window tmux format strings (window-status-format
/// / -current-format), joined by `separator`. Each entry's `#I`/`#W`/`#F` tokens
/// and `#[...]` style spans are expanded against a context built from that entry,
/// so a config can color the current window differently from the rest. `base`
/// supplies the fallback background so untinted spans blend into the bar.
///
/// Returns the spans plus the per-entry hit ranges (0-based position, start, end
/// column relative to the segment start), so status-bar clicks still map to the
/// right window — the ranges are computed from the actual rendered widths.
pub fn window_list_formatted(
    entries: &[WindowEntry],
    inactive_fmt: &str,
    current_fmt: &str,
    separator: &str,
    ctx: &StatusContext,
    base: &CellAttributes,
) -> (Vec<Span>, Vec<(usize, usize, usize)>) {
    let mut spans = Vec::new();
    let mut ranges = Vec::new();
    let mut col = 0usize;
    for (i, e) in entries.iter().enumerate() {
        let ectx = StatusContext {
            window: e.name.clone(),
            window_index: e.index,
            flags: if e.active {
                "*".to_string()
            } else {
                String::new()
            },
            // Session/host/time carry through so a format may reference them.
            session: ctx.session.clone(),
            host: ctx.host.clone(),
            pane_index: ctx.pane_index,
            client_prefix: ctx.client_prefix,
            time: ctx.time.clone(),
        };
        let fmt = if e.active { current_fmt } else { inactive_fmt };
        let entry_spans = format(fmt, &ectx);
        let width: usize = entry_spans
            .iter()
            .map(|span| display_width(&span.text))
            .sum();
        // Entries default their background to the bar base when unset, so the
        // pill fills correctly (mirrors StyledStatus::paint's inheritance).
        for mut sp in entry_spans {
            if sp.attrs.background() == ColorAttribute::Default {
                sp.attrs.set_background(base.background());
            }
            spans.push(sp);
        }
        ranges.push((i, col, col + width));
        col += width;
        if i + 1 < entries.len() && !separator.is_empty() {
            spans.push(Span {
                text: separator.to_string(),
                attrs: base.clone(),
            });
            col += display_width(separator);
        }
    }
    (spans, ranges)
}

/// Column ranges, within the centre segment, occupied by each window entry —
/// `(entry_position, start_col, end_col)` where columns are relative to the
/// start of the centre segment. The separator space between entries belongs to
/// no entry. Used to map a status-bar click back to the window the user hit; the
/// formatting mirrors [`window_list`] exactly so the ranges line up with what is
/// drawn. `entry_position` is the 0-based index into the window list.
pub fn window_list_hit_ranges(entries: &[WindowEntry]) -> Vec<(usize, usize, usize)> {
    let mut ranges = Vec::new();
    let mut col = 0usize;
    for (i, e) in entries.iter().enumerate() {
        let width = display_width(&entry_text(e));
        ranges.push((i, col, col + width));
        col += width;
        if i + 1 < entries.len() {
            col += 1; // separator space
        }
    }
    ranges
}

/// Values substituted into format variable tokens.
#[derive(Debug, Clone, Default)]
pub struct StatusContext {
    pub session: String,
    pub window: String,
    pub window_index: u32,
    pub pane_index: u32,
    pub host: String,
    /// Window flags for `#F` (e.g. `*` current, `-` last). Empty for the plain
    /// status segments; filled per-entry when formatting the window list.
    pub flags: String,
    /// Whether the client has the prefix armed (tmux `#{?client_prefix,…}`): the
    /// prefix key was pressed and lumux is awaiting the next command key.
    pub client_prefix: bool,
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
                Some('{') => {
                    // Format block: #{...}. Read to the matching '}' (braces may
                    // nest) then evaluate. The whole block joins the current span
                    // (blocks carry no style of their own).
                    chars.next(); // consume '{'
                    let mut body = String::new();
                    let mut depth = 1;
                    for bc in chars.by_ref() {
                        match bc {
                            '{' => {
                                depth += 1;
                                body.push(bc);
                            }
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                                body.push(bc);
                            }
                            _ => body.push(bc),
                        }
                    }
                    cur.push_str(&substitute_block(&body, ctx));
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
        // tmux `#h` is the short hostname (up to the first dot).
        'h' => ctx.host.split('.').next().unwrap_or(&ctx.host).to_string(),
        'I' => ctx.window_index.to_string(),
        'P' => ctx.pane_index.to_string(),
        'F' => ctx.flags.clone(),
        other => format!("#{other}"),
    }
}

/// Evaluate the body of a `#{...}` format block against `ctx`.
///
/// Supports:
/// - `?cond,then,else` — tmux conditional: emits `then` when the boolean
///   variable `cond` is true, else `else`. The `then`/`else` parts are
///   themselves expanded (so `#[...]`/`#X`/`%X` tokens inside them work), and
///   commas nested inside further `#{...}` are not split on.
/// - a bare variable name (`host`, `hostname_short`, `session_name`,
///   `window_name`, `client_prefix`, …) — emits its value.
///
/// Unknown blocks emit empty (tmux does too), so a config never dumps raw
/// `#{...}` onto the bar.
fn substitute_block(body: &str, ctx: &StatusContext) -> String {
    if let Some(rest) = body.strip_prefix('?') {
        // Conditional: split into cond, then, else on TOP-LEVEL commas.
        let parts = split_top_level(rest, ',');
        let cond = parts.first().map(String::as_str).unwrap_or("");
        let then_s = parts.get(1).map(String::as_str).unwrap_or("");
        let else_s = parts.get(2).map(String::as_str).unwrap_or("");
        let chosen = if eval_condition(cond, ctx) {
            then_s
        } else {
            else_s
        };
        // Expand the chosen branch (it may hold tokens/styles). We only want its
        // text here, so join the resulting spans' text.
        return format(chosen, ctx).into_iter().map(|s| s.text).collect();
    }
    // A bare variable block, e.g. #{host} / #{session_name}.
    block_var(body, ctx)
}

/// Whether a boolean condition variable is currently true.
fn eval_condition(name: &str, ctx: &StatusContext) -> bool {
    match name {
        "client_prefix" => ctx.client_prefix,
        // A non-empty string variable is truthy (tmux semantics), e.g.
        // `#{?session_name,…}`.
        other => !block_var(other, ctx).is_empty(),
    }
}

/// Resolve a named variable used inside a `#{...}` block to its string value.
fn block_var(name: &str, ctx: &StatusContext) -> String {
    match name {
        "session_name" => ctx.session.clone(),
        "window_name" => ctx.window.clone(),
        "host" | "host_short" | "hostname" => ctx.host.clone(),
        "window_index" => ctx.window_index.to_string(),
        "pane_index" => ctx.pane_index.to_string(),
        "client_prefix" => {
            if ctx.client_prefix {
                "1".to_string()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// Split `s` on `sep`, but only at brace depth 0, so commas inside nested
/// `#{...}` blocks stay with their block. Returns owned segments.
fn split_top_level(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '#' if chars.peek() == Some(&'{') => {
                cur.push(c);
                cur.push('{');
                chars.next();
                depth += 1;
            }
            '{' => {
                depth += 1;
                cur.push(c);
            }
            '}' => {
                depth -= 1;
                cur.push(c);
            }
            c if c == sep && depth == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
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
    apply_style_spec(attrs, spec)
}

/// Apply a comma-separated tmux style spec (`fg=`, `bg=`, and attribute flags
/// like `bold`/`reverse`) onto `attrs`. Public so the daemon can style the
/// message row (tmux `message-style`) with the same parser used for `#[...]`
/// spans.
pub fn apply_style_spec(attrs: &mut CellAttributes, spec: &str) {
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

/// Parse a tmux color name, `colourN` / `N` palette index, or `#rrggbb` /
/// `#rgb` hex triplet into a ColorAttribute. Hex colors become truecolor with a
/// nearest-256-palette fallback so they still render on non-truecolor terminals.
pub fn parse_color(s: &str) -> ColorAttribute {
    let s = s.trim();
    if let Some(rgb) = parse_hex(s) {
        return rgb;
    }
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

/// Parse a `#rrggbb` or shorthand `#rgb` hex color into a truecolor attribute
/// with a nearest-palette fallback. Returns None if `s` isn't a valid hex color.
fn parse_hex(s: &str) -> Option<ColorAttribute> {
    let hex = s.strip_prefix('#')?;
    let (r, g, b) = match hex.len() {
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            (r, g, b)
        }
        // Shorthand #rgb expands each nibble (f -> ff), matching CSS/tmux.
        3 => {
            let n = |c: &str| u8::from_str_radix(c, 16).ok().map(|v| v * 17);
            (n(&hex[0..1])?, n(&hex[1..2])?, n(&hex[2..3])?)
        }
        _ => return None,
    };
    let tuple = termwiz::color::RgbColor::new_8bpc(r, g, b).to_tuple_rgba();
    Some(ColorAttribute::TrueColorWithPaletteFallback(
        tuple,
        nearest_palette_index(r, g, b),
    ))
}

/// Approximate an RGB color with the closest xterm-256 palette index, so a
/// truecolor value still renders sensibly where only 256 colors are available.
/// Uses the 6x6x6 color cube (indices 16..232) plus the grayscale ramp.
fn nearest_palette_index(r: u8, g: u8, b: u8) -> u8 {
    // Map an 8-bit channel to the 6-level cube axis (0,95,135,175,215,255).
    fn cube_axis(v: u8) -> (u8, u8) {
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let mut best = 0usize;
        let mut best_d = u16::MAX;
        for (i, &l) in LEVELS.iter().enumerate() {
            let d = (l as i16 - v as i16).unsigned_abs();
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        (best as u8, LEVELS[best])
    }
    let (ri, rv) = cube_axis(r);
    let (gi, gv) = cube_axis(g);
    let (bi, bv) = cube_axis(b);
    let cube_idx = 16 + 36 * ri + 6 * gi + bi;
    let cube_d = dist2(r, g, b, rv, gv, bv);

    // Grayscale ramp: indices 232..256 map to 8,18,...,238.
    let gray = ((r as u16 + g as u16 + b as u16) / 3) as u8;
    let gi = if gray < 8 {
        0
    } else {
        ((gray as u16 - 8) / 10).min(23) as u8
    };
    let gv = 8 + gi * 10;
    let gray_idx = 232 + gi;
    let gray_d = dist2(r, g, b, gv, gv, gv);

    if gray_d < cube_d {
        gray_idx
    } else {
        cube_idx
    }
}

fn dist2(r: u8, g: u8, b: u8, r2: u8, g2: u8, b2: u8) -> u32 {
    let d = |a: u8, b: u8| {
        let x = a as i32 - b as i32;
        (x * x) as u32
    };
    d(r, r2) + d(g, g2) + d(b, b2)
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
            ..Default::default()
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
    fn short_host_strips_the_domain() {
        let mut c = ctx();
        c.host = "dev.example.com".into();
        // #H is the full host; #h and #{host_short} are up to the first dot.
        assert_eq!(joined(&format("#H", &c)), "dev.example.com");
        assert_eq!(joined(&format("#h", &c)), "dev");
    }

    #[test]
    fn client_prefix_conditional() {
        let mut c = ctx();
        // Not armed: the conditional yields the else branch (empty here).
        c.client_prefix = false;
        assert_eq!(joined(&format("#{?client_prefix,PFX,}", &c)), "");
        // Armed: yields the then branch.
        c.client_prefix = true;
        assert_eq!(joined(&format("#{?client_prefix,PFX,}", &c)), "PFX");
        // With an else branch.
        c.client_prefix = false;
        assert_eq!(joined(&format("#{?client_prefix,ON,off}", &c)), "off");
    }

    #[test]
    fn conditional_branches_expand_inner_tokens() {
        let mut c = ctx();
        c.client_prefix = true;
        // The then-branch itself contains a #S token, which must expand.
        assert_eq!(joined(&format("#{?client_prefix,[#S],}", &c)), "[work]");
    }

    #[test]
    fn bare_variable_block() {
        let s = format("#{session_name}@#{host}", &ctx());
        assert_eq!(joined(&s), "work@winhost");
    }

    #[test]
    fn unknown_block_yields_empty_not_literal() {
        // A block lumux doesn't know must NOT dump raw "#{...}" onto the bar.
        let s = format("x#{totally_unknown_thing}y", &ctx());
        assert_eq!(joined(&s), "xy");
    }

    #[test]
    fn nested_commas_in_condition_are_not_split() {
        let mut c = ctx();
        c.client_prefix = true;
        // The then-branch holds a further #{...} with its own comma; the outer
        // split must not break on that inner comma.
        let out = joined(&format(
            "#{?client_prefix,#{?client_prefix,YES,no},off}",
            &c,
        ));
        assert_eq!(out, "YES");
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
    fn window_list_formatted_expands_tokens_and_ranges() {
        use termwiz::color::ColorAttribute;
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
        ];
        let base = CellAttributes::default();
        let (spans, ranges) = window_list_formatted(
            &entries,
            " #I:#W ",                   // inactive
            "#[fg=green,bold] #I:#W#F ", // current (with a #F flag)
            "|",
            &ctx(),
            &base,
        );
        let text: String = spans.iter().map(|s| s.text.as_str()).collect();
        // Inactive " 1:bash ", separator "|", current " 2:vim* " (#F -> *).
        assert_eq!(text, " 1:bash | 2:vim* ");
        // The current window's spans carry the green fg from its format.
        assert!(
            spans
                .iter()
                .any(|s| s.attrs.foreground() == ColorAttribute::PaletteIndex(2)),
            "current window format should apply its color"
        );
        // Hit ranges: entry 0 = cols 0..8, entry 1 starts after the 1-col sep.
        assert_eq!(ranges, vec![(0, 0, 8), (1, 9, 17)]);
    }

    #[test]
    fn message_style_spec_applies_fg_bg() {
        use termwiz::color::ColorAttribute;
        let mut a = CellAttributes::default();
        apply_style_spec(&mut a, "fg=#ff0000,bg=colour24,bold");
        assert_eq!(a.intensity(), termwiz::cell::Intensity::Bold);
        assert_eq!(a.background(), ColorAttribute::PaletteIndex(24));
        // Hex fg becomes truecolor.
        assert!(matches!(
            a.foreground(),
            ColorAttribute::TrueColorWithPaletteFallback(_, _)
        ));
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
    fn parses_hex_colors_as_truecolor() {
        use termwiz::color::RgbColor;
        // #rrggbb → truecolor with a palette fallback.
        let c = parse_color("#1e1e2e");
        match c {
            ColorAttribute::TrueColorWithPaletteFallback(tuple, _) => {
                assert_eq!(tuple, RgbColor::new_8bpc(0x1e, 0x1e, 0x2e).to_tuple_rgba());
            }
            other => panic!("expected truecolor, got {other:?}"),
        }
        // Shorthand #rgb expands each nibble.
        assert_eq!(parse_color("#fff"), parse_color("#ffffff"));
        // Leading/trailing space tolerated (values come from split style specs).
        assert_eq!(parse_color(" #89b4fa "), parse_color("#89b4fa"));
        // Malformed hex falls back to Default rather than a wrong color.
        assert_eq!(parse_color("#12"), ColorAttribute::Default);
        assert_eq!(parse_color("#gggggg"), ColorAttribute::Default);
    }

    #[test]
    fn hex_palette_fallback_is_sane() {
        // Pure white/black map to the extremes of the fallback ramp.
        let idx = |s: &str| match parse_color(s) {
            ColorAttribute::TrueColorWithPaletteFallback(_, i) => i,
            other => panic!("expected truecolor, got {other:?}"),
        };
        assert_eq!(idx("#000000"), 16); // cube corner (0,0,0)
        assert_eq!(idx("#ffffff"), 231); // cube corner (5,5,5)
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

    #[test]
    fn window_list_hit_ranges_match_rendered_text() {
        // "1:bash 2:vim* 3:logs"
        //  0123456789...
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
        let ranges = window_list_hit_ranges(&entries);
        // "1:bash" = cols 0..6, sep at 6, "2:vim*" = 7..13, sep at 13,
        // "3:logs" = 14..20.
        assert_eq!(ranges, vec![(0, 0, 6), (1, 7, 13), (2, 14, 20)]);
        // The separator column (6) belongs to no entry.
        let hit = |c: usize| {
            ranges
                .iter()
                .find(|(_, s, e)| c >= *s && c < *e)
                .map(|(i, _, _)| *i)
        };
        assert_eq!(hit(0), Some(0)); // '1'
        assert_eq!(hit(5), Some(0)); // 'h' of bash
        assert_eq!(hit(6), None); // the space
        assert_eq!(hit(7), Some(1)); // '2'
        assert_eq!(hit(12), Some(1)); // '*'
        assert_eq!(hit(14), Some(2)); // '3'
        assert_eq!(hit(19), Some(2)); // 's' of logs
        assert_eq!(hit(20), None); // past the end
    }

    #[test]
    fn built_in_window_hit_ranges_use_terminal_cell_width() {
        let entries = vec![
            WindowEntry {
                index: 1,
                name: "界".into(),
                active: false,
            },
            WindowEntry {
                index: 2,
                name: "e\u{301}".into(),
                active: true,
            },
        ];

        // "1:界" occupies four cells, followed by one separator cell. The
        // combining sequence in "2:é*" remains a single display cell.
        let ranges = window_list_hit_ranges(&entries);
        assert_eq!(ranges, vec![(0, 0, 4), (1, 5, 9)]);
        let hit = |col: usize| {
            ranges
                .iter()
                .find(|(_, start, end)| col >= *start && col < *end)
                .map(|(position, _, _)| *position)
        };
        assert_eq!(hit(3), Some(0), "second cell of the wide name");
        assert_eq!(hit(4), None, "separator");
        assert_eq!(hit(5), Some(1));
        assert_eq!(hit(8), Some(1));
        assert_eq!(hit(9), None);
    }

    #[test]
    fn formatted_window_hit_ranges_include_wide_separators() {
        let entries = vec![
            WindowEntry {
                index: 1,
                name: "界".into(),
                active: false,
            },
            WindowEntry {
                index: 2,
                name: "e\u{301}".into(),
                active: true,
            },
        ];
        let base = CellAttributes::default();
        let (_spans, ranges) =
            window_list_formatted(&entries, "#W", "#W", "🙂", &StatusContext::default(), &base);

        // Wide name 0..2, wide separator 2..4, combining name 4..5.
        assert_eq!(ranges, vec![(0, 0, 2), (1, 4, 5)]);
        assert!(ranges
            .iter()
            .all(|(_, start, end)| !(3 >= *start && 3 < *end)));
    }
}

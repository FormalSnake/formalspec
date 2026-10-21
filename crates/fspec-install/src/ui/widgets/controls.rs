use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::{Canvas, Hit};
use crate::ui::theme::fg;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Amber,
    Secondary,
    Disabled,
}

fn focus_bar(c: &mut Canvas, row: Rect, focused: bool) {
    if focused {
        let a = c.th.accent;
        c.text(row, 0, 0, "▌", fg(a));
    }
}

/// A one-row switch with its label. The row is the hit area.
pub fn toggle(c: &mut Canvas, row: Rect, on: bool, label: &str, focused: bool, enabled: bool, hit: Hit) {
    let th = *c.th;
    c.hit(row, hit.w, hit.i);
    focus_bar(c, row, focused);
    let (knob, st) = match (on, enabled) {
        (true, true) => ("━━●", fg(th.accent)),
        (true, false) => ("━━●", fg(th.faint)),
        (false, _) => ("●━━", fg(th.faint)),
    };
    c.text(row, 2, 0, knob, st);
    c.text(row, 7, 0, label, if enabled { fg(th.ink) } else { fg(th.muted) });
}

/// Items side by side; returns nothing, each item is a hit `(w, index)`.
pub fn segmented(c: &mut Canvas, row: Rect, x: u16, labels: &[&str], sel: usize, focused: bool, w: u8) {
    let th = *c.th;
    let mut x = x;
    for (i, l) in labels.iter().enumerate() {
        let text = format!(" {l} ");
        let width = text.width() as u16;
        let r = Rect::new(row.x + x, row.y, width.min(row.width.saturating_sub(x)), 1);
        let on = i == sel;
        let mut st = if on {
            fg(if focused { th.accent } else { th.ink }).bg(th.sel).add_modifier(Modifier::BOLD)
        } else {
            fg(th.muted).bg(th.surface)
        };
        if on && !th.truecolor {
            st = st.add_modifier(Modifier::UNDERLINED);
        }
        c.text(row, x, 0, &text, st);
        c.hit(r, w, i);
        x += width + 1;
    }
}

/// `‹ 4 GB ›`; the arrows are hits `(w, 0)` for less and `(w, 1)` for more.
pub fn stepper(c: &mut Canvas, row: Rect, x: u16, value: &str, focused: bool, w: u8) {
    let th = *c.th;
    let arrow = if focused { fg(th.accent) } else { fg(th.muted) };
    let vw = value.width() as u16;
    c.text(row, x, 0, "‹", arrow);
    c.text(row, x + 2, 0, value, Style::new().fg(th.ink).add_modifier(Modifier::BOLD));
    c.text(row, x + 3 + vw, 0, "›", arrow);
    c.hit(Rect::new(row.x + x, row.y, 2, 1), w, 0);
    c.hit(Rect::new(row.x + x + 2 + vw, row.y, 2, 1), w, 1);
}

pub fn button_width(label: &str) -> u16 {
    label.width() as u16 + 4
}

/// Draws a button at `x` on `row`; returns its width. `enabled == false` greys it out and drops the hit.
pub fn button(c: &mut Canvas, row: Rect, x: u16, label: &str, kind: Kind, focused: bool, hit: Hit) -> u16 {
    let th = *c.th;
    let width = button_width(label);
    let mut st = match kind {
        Kind::Primary => Style::new().fg(th.on_accent).bg(th.accent).add_modifier(Modifier::BOLD),
        Kind::Amber => Style::new().fg(th.on_warm).bg(th.amber).add_modifier(Modifier::BOLD),
        Kind::Secondary => Style::new().fg(th.ink).bg(th.sel),
        Kind::Disabled => Style::new(),
    };
    let enabled = kind != Kind::Disabled;
    if !enabled {
        st = Style::new().fg(th.faint).bg(th.surface);
    } else if focused {
        st = st.add_modifier(Modifier::UNDERLINED | Modifier::BOLD);
    }
    c.text(row, x, 0, &format!("  {label}  "), st);
    if enabled {
        c.hit(Rect::new(row.x + x, row.y, width.min(row.width.saturating_sub(x)), 1), hit.w, hit.i);
    }
    width
}

/// Muted text on the surface fill with one column of padding; returns its width.
pub fn chip(c: &mut Canvas, row: Rect, x: u16, text: &str) -> u16 {
    let th = *c.th;
    c.text(row, x, 0, &format!(" {text} "), fg(th.muted).bg(th.surface))
}

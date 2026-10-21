pub mod controls;
pub mod input;
pub mod list;
pub mod spec;

use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use super::theme::{Theme, fg};

/// What a clickable region means: `w` names the widget on the current screen, `i` the item in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub w: u8,
    pub i: usize,
}

pub const W_TRAIL: u8 = 250;
pub const W_SPEC: u8 = 251;
pub const W_MODAL: u8 = 252;

pub struct Canvas<'a> {
    pub buf: &'a mut Buffer,
    pub th: &'a Theme,
    pub hits: &'a mut Vec<(Rect, Hit)>,
    pub now: Instant,
    pub epoch: Instant,
}

impl Canvas<'_> {
    /// Draws `s` at (`x`, `y`) relative to `a`, clipped to `a`. Returns the columns used.
    pub fn text(&mut self, a: Rect, x: u16, y: u16, s: &str, st: Style) -> u16 {
        if y >= a.height || x >= a.width || s.is_empty() {
            return 0;
        }
        let (nx, _) = self.buf.set_stringn(a.x + x, a.y + y, s, (a.width - x) as usize, st);
        nx - (a.x + x)
    }

    pub fn text_right(&mut self, a: Rect, y: u16, s: &str, st: Style) {
        let w = s.width() as u16;
        let x = a.width.saturating_sub(w);
        self.text(a, x, y, s, st);
    }

    /// Like `text` but ends in an ellipsis when it does not fit.
    pub fn text_ell(&mut self, a: Rect, x: u16, y: u16, s: &str, st: Style) -> u16 {
        let room = a.width.saturating_sub(x) as usize;
        if s.width() <= room {
            return self.text(a, x, y, s, st);
        }
        let mut out = String::new();
        let mut w = 0;
        for ch in s.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if w + cw + 1 > room {
                break;
            }
            out.push(ch);
            w += cw;
        }
        out.push('…');
        self.text(a, x, y, &out, st)
    }

    pub fn fill(&mut self, a: Rect, st: Style) {
        let a = a.intersection(self.buf.area);
        if a.is_empty() {
            return;
        }
        self.buf.set_style(a, st);
    }

    /// Blanks the cells too, so nothing underneath shows through.
    pub fn clear(&mut self, a: Rect, st: Style) {
        let a = a.intersection(self.buf.area);
        for y in a.top()..a.bottom() {
            for x in a.left()..a.right() {
                let cell = &mut self.buf[(x, y)];
                cell.reset();
                cell.set_style(st);
            }
        }
    }

    pub fn hit(&mut self, a: Rect, w: u8, i: usize) {
        self.hits.push((a, Hit { w, i }));
    }

    pub fn hline(&mut self, a: Rect, y: u16, st: Style) {
        for x in 0..a.width {
            self.text(a, x, y, "─", st);
        }
    }

    pub fn muted(&self) -> Style {
        fg(self.th.muted)
    }

    pub fn faint(&self) -> Style {
        fg(self.th.faint)
    }

    pub fn ink(&self) -> Style {
        fg(self.th.ink)
    }
}

pub fn at(hits: &[(Rect, Hit)], x: u16, y: u16) -> Option<Hit> {
    let p = Position::new(x, y);
    hits.iter().rev().find(|(r, _)| r.contains(p)).map(|(_, h)| *h)
}

pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if !cur.is_empty() && cur.width() + 1 + word.width() > width {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Decimal units, as disk vendors print them.
pub fn fmt_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e12 {
        let t = b / 1e12;
        if (t - t.round()).abs() < 0.05 { format!("{} TB", t.round() as u64) } else { format!("{t:.1} TB") }
    } else if b >= 1e9 {
        format!("{} GB", (b / 1e9).round() as u64)
    } else {
        format!("{} MB", (b / 1e6).round() as u64)
    }
}

pub fn rect_in(a: Rect, x: u16, y: u16, w: u16, h: u16) -> Rect {
    Rect::new(
        a.x + x.min(a.width),
        a.y + y.min(a.height),
        w.min(a.width.saturating_sub(x)),
        h.min(a.height.saturating_sub(y)),
    )
}

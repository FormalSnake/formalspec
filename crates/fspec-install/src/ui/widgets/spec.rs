use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use similar::{ChangeTag, TextDiff};

use super::{Canvas, W_SPEC};
use crate::ui::anim::{self, MARK};
use crate::ui::theme::{Theme, fg};

const KEYWORDS: &[&str] = &["load", "def", "return", "if", "else", "for", "in", "and", "or", "not", "lambda", "pass"];
const BUILTINS: &[&str] = &["host", "user", "True", "False", "None"];

type Row = (usize, usize, Vec<(char, Style)>);

#[derive(Default)]
pub struct SpecPane {
    text: String,
    lines: Vec<String>,
    marks: Vec<Option<Instant>>,
    pub scroll: usize,
    seeded: bool,
}

impl SpecPane {
    /// Feeds the newest preview; lines that are new or changed start their `+` fade at `now`.
    pub fn update(&mut self, text: &str, now: Instant) {
        if text == self.text && self.seeded {
            return;
        }
        let new: Vec<String> = text.lines().map(str::to_string).collect();
        let mut marks = Vec::with_capacity(new.len());
        if self.seeded {
            let old = self.text.clone();
            let diff = TextDiff::from_lines(old.as_str(), text);
            for ch in diff.iter_all_changes() {
                match ch.tag() {
                    ChangeTag::Equal => marks.push(ch.old_index().and_then(|i| self.marks.get(i).copied()).flatten()),
                    ChangeTag::Insert => marks.push(Some(now)),
                    ChangeTag::Delete => {}
                }
            }
        } else {
            marks.resize(new.len(), None);
        }
        marks.resize(new.len(), None);
        self.text = text.to_string();
        self.lines = new;
        self.marks = marks;
        self.seeded = true;
    }

    pub fn animating(&self, now: Instant) -> bool {
        self.marks.iter().flatten().any(|&t| now.saturating_duration_since(t) < MARK)
    }

    pub fn wheel(&mut self, d: isize) {
        self.scroll = (self.scroll as isize + d).max(0) as usize;
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Draws the rounded pane in `a`. Lines for which `current` is true get the surface fill.
    pub fn draw(&mut self, c: &mut Canvas, a: Rect, current: &dyn Fn(&str) -> bool, motion: bool) {
        let th = *c.th;
        let b = fg(th.line);
        let right = a.width.saturating_sub(1);
        let bottom = a.height.saturating_sub(1);
        c.text(a, 0, 0, "╭", b);
        c.text(a, right, 0, "╮", b);
        c.text(a, 0, bottom, "╰", b);
        c.text(a, right, bottom, "╯", b);
        for x in 1..right {
            c.text(a, x, 0, "─", b);
            c.text(a, x, bottom, "─", b);
        }
        for y in 1..bottom {
            c.text(a, 0, y, "│", b);
            c.text(a, right, y, "│", b);
        }
        c.text(a, 2, 0, " spec.star ", fg(th.muted));
        let inner = Rect::new(a.x + 1, a.y + 1, a.width.saturating_sub(2), a.height.saturating_sub(2));
        c.hit(a, W_SPEC, 0);
        let h = inner.height as usize;
        let room = inner.width.saturating_sub(2) as usize;
        let mut rows: Vec<Row> = Vec::new();
        for (idx, line) in self.lines.iter().enumerate() {
            let styled: Vec<(char, Style)> = highlight(line, &th)
                .into_iter()
                .flat_map(|(t, st)| t.chars().map(|ch| (ch, st)).collect::<Vec<_>>())
                .collect();
            let hang = (line.chars().take_while(|c| *c == ' ').count() + 4).min(room / 2);
            let first = break_at(&styled, 0, room);
            rows.push((idx, 0, styled[..first].to_vec()));
            let mut pos = first;
            while pos < styled.len() {
                while styled.get(pos).is_some_and(|(ch, _)| *ch == ' ') {
                    pos += 1;
                }
                let end = break_at(&styled, pos, room - hang);
                rows.push((idx, hang, styled[pos..end].to_vec()));
                pos = end;
            }
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(h));
        for (row, (idx, hang, chars)) in rows.iter().skip(self.scroll).take(h).enumerate() {
            let line = &self.lines[*idx];
            let r = Rect { y: inner.y + row as u16, height: 1, ..inner };
            if current(line) {
                c.fill(r, Style::new().bg(th.surface));
            }
            if let (Some(t), 0) = (self.marks[*idx], *hang) {
                let el = c.now.saturating_duration_since(t);
                if el < MARK {
                    let col = if motion {
                        anim::lerp_color(th.accent, th.faint, el.as_secs_f32() / MARK.as_secs_f32())
                    } else {
                        th.accent
                    };
                    c.text(r, 0, 0, "+", fg(col));
                }
            }
            for (n, (ch, st)) in chars.iter().enumerate() {
                c.text(r, (2 + hang + n) as u16, 0, &ch.to_string(), *st);
            }
        }
    }
}

/// Where a row starting at `pos` with `room` columns ends: after the last space that fits, or
/// mid-word when a single word is wider than the row.
fn break_at(styled: &[(char, Style)], pos: usize, room: usize) -> usize {
    let end = (pos + room.max(1)).min(styled.len());
    if end == styled.len() {
        return end;
    }
    match styled[pos..end].iter().rposition(|(ch, _)| *ch == ' ') {
        Some(i) if i > 0 && !styled[pos..pos + i].iter().all(|(ch, _)| *ch == ' ') => pos + i + 1,
        _ => end,
    }
}

pub fn highlight(line: &str, th: &Theme) -> Vec<(String, Style)> {
    let mut out: Vec<(String, Style)> = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '#' {
            out.push((chars[i..].iter().collect(), fg(th.faint)));
            break;
        } else if ch == '"' {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '"' {
                j += if chars[j] == '\\' { 2 } else { 1 };
            }
            let end = (j + 1).min(chars.len());
            out.push((chars[i..end].iter().collect(), fg(th.ink)));
            i = end;
        } else if ch.is_alphabetic() || ch == '_' {
            let mut j = i;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            let st = if KEYWORDS.contains(&word.as_str()) || BUILTINS.contains(&word.as_str()) {
                Style::new().fg(th.accent).add_modifier(Modifier::BOLD)
            } else {
                fg(th.muted)
            };
            out.push((word, st));
            i = j;
        } else if ch.is_ascii_digit() {
            let mut j = i;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            out.push((chars[i..j].iter().collect(), fg(th.ink)));
            i = j;
        } else {
            out.push((ch.to_string(), fg(th.muted)));
            i += 1;
        }
    }
    out
}

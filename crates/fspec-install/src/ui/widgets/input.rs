use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::Canvas;
use crate::ui::theme::fg;

#[derive(Clone, Debug, Default)]
pub struct TextInput {
    pub value: String,
    /// In characters.
    pub cursor: usize,
    pub mask: bool,
}

impl TextInput {
    pub fn new(mask: bool) -> Self {
        TextInput { value: String::new(), cursor: 0, mask }
    }

    pub fn set(&mut self, v: &str) {
        self.value = v.to_string();
        self.cursor = self.len();
    }

    pub fn len(&self) -> usize {
        self.value.chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    fn byte(&self, ci: usize) -> usize {
        self.value.char_indices().nth(ci).map_or(self.value.len(), |(b, _)| b)
    }

    /// Whether the key was an editing or cursor key, so the caller does not treat it as navigation.
    pub fn key(&mut self, k: &KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match k.code {
            KeyCode::Char('u') if ctrl => {
                let b = self.byte(self.cursor);
                self.value.replace_range(..b, "");
                self.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => {
                let end = self.byte(self.cursor);
                let head: Vec<char> = self.value[..end].chars().collect();
                let mut i = head.len();
                while i > 0 && head[i - 1] == ' ' {
                    i -= 1;
                }
                while i > 0 && head[i - 1] != ' ' {
                    i -= 1;
                }
                let start = self.byte(i);
                self.value.replace_range(start..end, "");
                self.cursor = i;
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.len(),
            KeyCode::Char(c) if !ctrl && !alt => {
                let b = self.byte(self.cursor);
                self.value.insert(b, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let b = self.byte(self.cursor - 1);
                self.value.remove(b);
                self.cursor -= 1;
            }
            KeyCode::Delete if self.cursor < self.len() => {
                let b = self.byte(self.cursor);
                self.value.remove(b);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.len(),
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    pub fn shown(&self) -> String {
        if self.mask { "•".repeat(self.len()) } else { self.value.clone() }
    }
}

pub struct Field<'a> {
    pub label: &'a str,
    pub input: &'a TextInput,
    pub placeholder: &'a str,
    pub focused: bool,
    pub error: Option<&'a str>,
    pub hint: Option<&'a str>,
    pub w: u8,
    pub i: usize,
}

pub const FIELD_ROWS: u16 = 3;

/// Label row, input row on the surface fill, then an error (red) or hint (faint) row.
pub fn draw_field(c: &mut Canvas, a: Rect, f: Field) {
    let th = *c.th;
    let a = Rect { height: a.height.min(FIELD_ROWS), ..a };
    c.hit(a, f.w, f.i);
    c.text(a, 0, 0, f.label, if f.focused { fg(th.accent) } else { fg(th.muted) });
    let row = Rect { y: a.y + 1, height: 1, ..a };
    c.fill(row, Style::new().bg(th.surface));
    if f.focused {
        c.text(row, 0, 0, "▌", fg(th.accent));
    }
    let room = a.width.saturating_sub(4) as usize;
    let shown: Vec<char> = f.input.shown().chars().collect();
    let cur = f.input.cursor.min(shown.len());
    let start = if cur >= room { cur + 1 - room } else { 0 };
    if shown.is_empty() {
        c.text_ell(row, 2, 0, f.placeholder, fg(th.faint));
    } else {
        let end = (start + room).min(shown.len());
        let s: String = shown[start..end].iter().collect();
        c.text(row, 2, 0, &s, fg(th.ink));
    }
    if f.focused {
        let cx = 2 + (cur - start) as u16;
        if cx < row.width {
            let cell = &mut c.buf[(row.x + cx, row.y)];
            cell.modifier.insert(Modifier::REVERSED);
        }
    }
    if let Some(e) = f.error {
        c.text_ell(a, 2, 2, e, fg(th.red));
    } else if let Some(h) = f.hint {
        c.text_ell(a, 2, 2, h, fg(th.faint));
    }
}

/// A one-row filter box above a list: a placeholder when empty, the typed text and caret otherwise.
pub fn draw_filter(c: &mut Canvas, a: Rect, text: &str, focused: bool) {
    let th = *c.th;
    if text.is_empty() {
        c.text(a, 0, 0, "Type to filter", fg(th.faint));
    } else {
        let n = c.text(a, 0, 0, text, fg(th.ink));
        if focused {
            c.text(a, n, 0, " ", Style::new().add_modifier(Modifier::REVERSED));
        }
    }
}

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::{Flow, Hints, Shared, hints};
use crate::ui::theme::fg;
use crate::ui::widgets::list::row_chrome;
use crate::ui::widgets::{Canvas, Hit, rect_in, wrap};

const LOGO: &str = include_str!("../logo.txt");
const TAGLINE: &str = "Install a Linux system that FormalSpec manages from its first boot.";
const CARDS: [(&str, &str); 2] = [
    ("Install a new system", "Pick a distro and a preset. The installer writes the spec for you."),
    ("Restore from git", "Install a machine exactly as an existing spec declares it."),
];
const W_CARD: u8 = 0;

#[derive(Default)]
pub struct Welcome {
    pub sel: usize,
}

impl Welcome {
    pub fn key(&mut self, k: &KeyEvent, _sh: &mut Shared) -> Flow {
        match k.code {
            KeyCode::Left | KeyCode::Up | KeyCode::Char('h' | 'k') | KeyCode::BackTab => self.sel = 0,
            KeyCode::Right | KeyCode::Down | KeyCode::Char('l' | 'j') => self.sel = 1,
            KeyCode::Tab => self.sel = 1 - self.sel,
            KeyCode::Enter => return Flow::Next,
            KeyCode::Esc => return Flow::Back,
            _ => {}
        }
        Flow::Stay
    }

    pub fn click(&mut self, h: Hit) -> Flow {
        if h.w != W_CARD {
            return Flow::Stay;
        }
        if self.sel == h.i {
            return Flow::Next;
        }
        self.sel = h.i;
        Flow::Stay
    }

    pub fn hints(&self) -> Hints {
        hints(&[("←→", "Choose"), ("Enter", "Continue"), ("Esc", "Quit"), ("?", "Keys")])
    }

    pub fn render(&mut self, c: &mut Canvas, a: Rect, _sh: &Shared) {
        let th = *c.th;
        let side = a.width >= 100;
        let logo: Vec<&str> = LOGO.lines().collect();
        let logo_w = logo.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
        let card_w = if side { (a.width.min(104) - 3) / 2 } else { a.width.min(64) };
        let card_h = if side { 5 } else { 4 };
        let cards_h = if side { card_h } else { card_h * 2 + 1 };
        let total = logo.len() as u16 + 2 + 1 + 2 + cards_h;
        let mut y = a.height.saturating_sub(total) / 2;
        for l in &logo {
            let x = a.width.saturating_sub(logo_w) / 2;
            c.text(a, x, y, l, Style::new().fg(th.accent));
            y += 1;
        }
        y += 2;
        let x = a.width.saturating_sub(TAGLINE.width() as u16) / 2;
        c.text(a, x, y, TAGLINE, fg(th.muted));
        y += 3;
        let block_w = if side { card_w * 2 + 3 } else { card_w };
        let x0 = a.width.saturating_sub(block_w) / 2;
        for (i, (title, desc)) in CARDS.iter().enumerate() {
            let (cx, cy) = if side { (x0 + i as u16 * (card_w + 3), y) } else { (x0, y + i as u16 * (card_h + 1)) };
            let r = rect_in(a, cx, cy, card_w, card_h);
            let on = self.sel == i;
            c.fill(r, Style::new().bg(th.surface));
            for dy in 0..card_h {
                row_chrome(c, Rect { y: r.y + dy, height: 1, ..r }, on, true);
            }
            c.hit(r, W_CARD, i);
            let t = if on { Style::new().fg(th.ink).add_modifier(Modifier::BOLD) } else { fg(th.ink) };
            c.text(r, 3, 1, title, t);
            for (n, line) in wrap(desc, r.width.saturating_sub(5) as usize).iter().take(2).enumerate() {
                c.text(r, 3, 2 + n as u16, line, fg(th.muted));
            }
        }
    }
}

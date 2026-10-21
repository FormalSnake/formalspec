use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::{Flow, Hints, Shared, hints};
use crate::catalog::DISTROS;
use crate::model::{Desktop, Distro, Preset};
use crate::ui::theme::fg;
use crate::ui::widgets::controls::segmented;
use crate::ui::widgets::list::row_chrome;
use crate::ui::widgets::{Canvas, Hit, rect_in, wrap};

const W_CARD: u8 = 0;
const W_DESKTOP: u8 = 1;

pub fn blurb(p: Preset) -> (&'static str, &'static str) {
    match p {
        Preset::Minimal => ("Minimal", "A console system with networking and sudo. Add the rest in your spec."),
        Preset::Desktop => ("Desktop", "GNOME or KDE Plasma with a browser, audio, printing and Flatpak."),
        Preset::Gaming => ("Gaming", "The desktop plus Steam, Proton, GameMode and MangoHud."),
        Preset::Server => ("Server", "SSH, a firewall and automatic security updates. No desktop."),
    }
}

fn desktop_name(d: Desktop) -> &'static str {
    match d {
        Desktop::Gnome => "GNOME",
        Desktop::Kde => "KDE Plasma",
    }
}

pub struct PresetScreen {
    distro: &'static Distro,
    sel: usize,
    desk: usize,
}

impl Default for PresetScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl PresetScreen {
    pub fn new() -> Self {
        PresetScreen { distro: &DISTROS[0], sel: 0, desk: 0 }
    }

    pub fn set_distro(&mut self, d: &'static Distro) {
        self.distro = d;
        self.sel = 0;
        self.desk = 0;
    }

    fn preset(&self) -> Preset {
        self.distro.presets[self.sel.min(self.distro.presets.len() - 1)]
    }

    fn wants_desktop(&self) -> bool {
        matches!(self.preset(), Preset::Desktop | Preset::Gaming)
    }

    fn choosable(&self) -> bool {
        self.wants_desktop() && self.distro.desktops.len() > 1
    }

    pub fn current(&self) -> (Preset, Option<Desktop>) {
        let desktop = self.wants_desktop().then(|| self.distro.desktops[self.desk.min(self.distro.desktops.len() - 1)]);
        (self.preset(), desktop)
    }

    pub fn captures_text(&self) -> bool {
        false
    }

    pub fn key(&mut self, k: &KeyEvent, _sh: &mut Shared) -> Flow {
        let n = self.distro.presets.len();
        match k.code {
            KeyCode::Enter => return Flow::Next,
            KeyCode::Esc => return Flow::Back,
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.sel = (self.sel + 1).min(n - 1),
            KeyCode::Home => self.sel = 0,
            KeyCode::End => self.sel = n - 1,
            KeyCode::Left | KeyCode::Char('h') if self.choosable() => self.desk = self.desk.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') if self.choosable() => {
                self.desk = (self.desk + 1).min(self.distro.desktops.len() - 1)
            }
            _ => {}
        }
        Flow::Stay
    }

    pub fn click(&mut self, h: Hit) -> Flow {
        match h.w {
            W_CARD if self.sel == h.i => return Flow::Next,
            W_CARD => self.sel = h.i,
            W_DESKTOP => self.desk = h.i,
            _ => {}
        }
        Flow::Stay
    }

    pub fn hints(&self) -> Hints {
        if self.choosable() {
            hints(&[("↑↓", "Move"), ("←→", "Desktop"), ("Enter", "Continue"), ("Esc", "Back"), ("?", "Keys")])
        } else {
            hints(&[("↑↓", "Move"), ("Enter", "Continue"), ("Esc", "Back"), ("?", "Keys")])
        }
    }

    pub fn render(&mut self, c: &mut Canvas, a: Rect, _sh: &Shared) {
        let th = *c.th;
        c.text(
            a,
            0,
            0,
            &format!("Choose a preset for {}", self.distro.name),
            Style::new().fg(th.ink).add_modifier(Modifier::BOLD),
        );
        let w = a.width.min(76);
        let mut y = 2;
        for (i, p) in self.distro.presets.iter().enumerate() {
            let (name, desc) = blurb(*p);
            let lines = wrap(desc, w as usize - 4);
            let h = 1 + lines.len() as u16;
            let card = rect_in(a, 0, y, w, h);
            let on = self.sel == i;
            for dy in 0..h {
                row_chrome(c, Rect { y: card.y + dy, height: 1, ..card }, on, true);
            }
            c.hit(rect_in(a, 0, y, w, h + 1), W_CARD, i);
            c.text(
                card,
                2,
                0,
                name,
                if on { Style::new().fg(th.ink).add_modifier(Modifier::BOLD) } else { fg(th.ink) },
            );
            for (n, l) in lines.iter().enumerate() {
                c.text(card, 2, 1 + n as u16, l, fg(th.muted));
            }
            y += h + 1;
        }
        if self.choosable() {
            let row = rect_in(a, 0, y, a.width, 1);
            c.text(row, 2, 0, "Desktop", fg(th.muted));
            let labels: Vec<&str> = self.distro.desktops.iter().map(|d| desktop_name(*d)).collect();
            segmented(c, row, 11, &labels, self.desk, true, W_DESKTOP);
        }
    }
}

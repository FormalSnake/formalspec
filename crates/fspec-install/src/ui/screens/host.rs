use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::{Flow, Hints, Shared, hints};
use crate::model::HostSummary;
use crate::ui::theme::fg;
use crate::ui::widgets::controls::chip;
use crate::ui::widgets::list::{FilterList, row_chrome};
use crate::ui::widgets::{Canvas, Hit};

pub struct HostScreen {
    hosts: Vec<HostSummary>,
    list: FilterList,
    height: usize,
}

impl Default for HostScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl HostScreen {
    pub fn new() -> Self {
        HostScreen { hosts: Vec::new(), list: FilterList::new(0), height: 6 }
    }

    pub fn set_hosts(&mut self, hosts: Vec<HostSummary>) {
        self.hosts = hosts;
        self.list = FilterList::new(self.hosts.len());
        let h = &self.hosts;
        self.list.home(&|i| h[i].unsupported.is_none());
    }

    pub fn selected(&self) -> Option<&HostSummary> {
        self.list.selected().map(|i| &self.hosts[i]).filter(|h| h.unsupported.is_none())
    }

    pub fn captures_text(&self) -> bool {
        false
    }

    pub fn key(&mut self, k: &KeyEvent, sh: &mut Shared) -> Flow {
        match k.code {
            KeyCode::Enter => match self.selected().cloned() {
                Some(h) => {
                    sh.set_host(Some(h));
                    Flow::Next
                }
                None => Flow::Stay,
            },
            KeyCode::Esc => Flow::Back,
            _ => {
                let h = &self.hosts;
                self.list.nav(k, self.height, true, &|i| h[i].unsupported.is_none());
                Flow::Stay
            }
        }
    }

    pub fn click(&mut self, h: Hit, sh: &mut Shared) -> Flow {
        if self.hosts.get(h.i).is_none_or(|x| x.unsupported.is_some()) {
            return Flow::Stay;
        }
        if self.list.selected() == Some(h.i) {
            return self.key(&KeyEvent::from(KeyCode::Enter), sh);
        }
        self.list.select_item(h.i);
        Flow::Stay
    }

    pub fn wheel(&mut self, d: isize) {
        self.list.wheel(d, self.height);
    }

    pub fn hints(&self) -> Hints {
        hints(&[("↑↓", "Move"), ("Enter", "Continue"), ("Esc", "Back"), ("?", "Keys")])
    }

    pub fn render(&mut self, c: &mut Canvas, a: Rect, _sh: &Shared) {
        let th = *c.th;
        c.text(a, 0, 0, "Choose a host", Style::new().fg(th.ink).add_modifier(Modifier::BOLD));
        if self.hosts.is_empty() {
            c.text(a, 0, 2, "This spec declares no hosts this installer can install.", fg(th.muted));
            return;
        }
        let list_h = (a.height.saturating_sub(2) as usize).min(self.hosts.len());
        self.height = list_h;
        let top = self.list.window(list_h);
        for (n, idx) in (top..self.hosts.len()).take(list_h).enumerate() {
            let hs = &self.hosts[idx];
            let row = Rect { y: a.y + 2 + n as u16, height: 1, ..a };
            let off = hs.unsupported.is_some();
            let sel = self.list.selected() == Some(idx) && !off;
            row_chrome(c, row, sel, true);
            let name = if off {
                fg(th.faint)
            } else if sel {
                Style::new().fg(th.ink).add_modifier(Modifier::BOLD)
            } else {
                fg(th.ink)
            };
            c.text_ell(row, 2, 0, &hs.name, name);
            if let Some(reason) = &hs.unsupported {
                c.text_ell(row, 22, 0, reason, fg(th.faint));
            } else {
                c.text(row, 22, 0, &hs.platform, fg(th.muted));
                let mut x = 40;
                if !hs.distro.is_empty() {
                    x += chip(c, row, x, &hs.distro) + 2;
                }
                c.text_ell(row, x, 0, &hs.users.join(", "), fg(th.muted));
                c.hit(row, 0, idx);
            }
        }
    }
}

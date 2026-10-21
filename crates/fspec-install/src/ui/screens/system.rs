use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::{Flow, Hints, Shared, hints};
use crate::catalog::DISTROS;
use crate::model::{Delivery, Distro, Init, Release};
use crate::ui::theme::fg;
use crate::ui::widgets::controls::chip;
use crate::ui::widgets::list::{FilterList, row_chrome};
use crate::ui::widgets::{Canvas, Hit, rect_in, wrap};

const W_LIST: u8 = 0;
const LABEL_W: u16 = 17;

/// The chips every row shows, in column order.
pub fn tags(d: &Distro) -> [(&'static str, &'static str); 4] {
    [
        match d.release {
            Release::Rolling => ("rolling", "Updates arrive continuously; there are no version upgrades."),
            Release::Fixed => ("fixed", "New versions arrive in scheduled releases you upgrade to."),
        },
        match d.init {
            Init::Systemd => ("systemd", "Services start with systemd, as on most distros."),
            Init::Runit => ("runit", "Services start with runit, a small supervisor driven by plain scripts."),
        },
        if d.signed_kernel {
            ("signed kernel", "Boots with Secure Boot on, and anti-cheat that checks kernel signatures trusts it.")
        } else {
            ("unsigned kernel", "Needs Secure Boot off, or your own keys (offered on the review screen).")
        },
        match d.delivery {
            Delivery::Image => {
                ("image", "The system is a bootc image: updates apply whole, and the previous image stays bootable.")
            }
            Delivery::Atomic => {
                ("atomic", "Each update swaps the system in one transaction; earlier states stay bootable.")
            }
            Delivery::Classic => {
                ("classic", "The package manager changes the system in place; btrfs snapshots cover rollback.")
            }
        },
    ]
}

/// The detail lines below the chips: what it builds on, and whether it is ready for daily use.
fn origin(d: &Distro) -> Vec<(String, String)> {
    let mut out = vec![match d.based_on {
        Some(b) => (format!("based on {b}"), format!("Uses {b}'s packages, with its own changes on top.")),
        None => ("independent".into(), "Built from its own sources and tooling, not on top of another distro.".into()),
    }];
    if d.alpha {
        out.push(("alpha".into(), "Still in development. Expect breakage; keep it off machines you depend on.".into()));
    }
    out
}

pub struct System {
    list: FilterList,
    height: usize,
}

impl Default for System {
    fn default() -> Self {
        Self::new()
    }
}

impl System {
    pub fn new() -> Self {
        System { list: FilterList::new(DISTROS.len()), height: 6 }
    }

    pub fn current(&self) -> Option<&'static Distro> {
        self.list.selected().map(|i| &DISTROS[i])
    }

    pub fn captures_text(&self) -> bool {
        false
    }

    pub fn key(&mut self, k: &KeyEvent, _sh: &mut Shared) -> Flow {
        match k.code {
            KeyCode::Enter => Flow::Next,
            KeyCode::Esc => Flow::Back,
            _ => {
                self.list.nav(k, self.height, true, &|_| true);
                Flow::Stay
            }
        }
    }

    pub fn click(&mut self, h: Hit) -> Flow {
        if h.w != W_LIST {
            return Flow::Stay;
        }
        if self.list.selected() == Some(h.i) {
            return Flow::Next;
        }
        self.list.select_item(h.i);
        Flow::Stay
    }

    pub fn wheel(&mut self, _w: u8, d: isize) {
        self.list.wheel(d, self.height);
    }

    pub fn hints(&self) -> Hints {
        hints(&[("↑↓", "Move"), ("Enter", "Continue"), ("Esc", "Back"), ("?", "Keys")])
    }

    pub fn render(&mut self, c: &mut Canvas, a: Rect, _sh: &Shared) {
        let th = *c.th;
        let side = a.width >= 100;
        let Some(d) = self.current() else { return };
        c.text(a, 0, 0, "Choose a system", Style::new().fg(th.ink).add_modifier(Modifier::BOLD));
        let list_w = if side { 66 } else { a.width };
        let detail = if side {
            rect_in(a, list_w + 3, 2, a.width.saturating_sub(list_w + 3), a.height.saturating_sub(2))
        } else {
            rect_in(a, 0, 0, a.width, 0)
        };
        let dw = if side { detail.width } else { a.width } as usize;
        let reason = wrap(d.reason, dw);
        let explain = |e: &str| wrap(e, dw.saturating_sub(LABEL_W as usize));
        let tag_lines: Vec<(String, Vec<String>)> = tags(d)
            .iter()
            .map(|(t, e)| (t.to_string(), explain(e)))
            .chain(origin(d).into_iter().map(|(t, e)| (t, explain(&e))))
            .collect();
        let detail_h = 1 + reason.len() + 1 + tag_lines.iter().map(|(_, l)| l.len()).sum::<usize>();
        let list_h = if side {
            a.height.saturating_sub(2) as usize
        } else {
            (a.height as usize).saturating_sub(2 + 1 + detail_h).max(3)
        }
        .min(DISTROS.len());
        self.height = list_h;
        let top = self.list.window(list_h);
        for (n, idx) in (top..DISTROS.len()).take(list_h).enumerate() {
            let dd = &DISTROS[idx];
            let row = Rect { y: a.y + 2 + n as u16, height: 1, width: list_w, ..a };
            let sel = self.list.selected() == Some(idx);
            row_chrome(c, row, sel, true);
            let st = if sel { Style::new().fg(th.ink).add_modifier(Modifier::BOLD) } else { fg(th.ink) };
            let used = c.text(row, 2, 0, dd.name, st);
            if dd.alpha {
                c.text(row, 2 + used + 1, 0, " alpha ", fg(th.amber).bg(th.surface));
            }
            for ((t, _), x) in tags(dd).iter().zip([21, 30, 39, 56]) {
                chip(c, row, x, t);
            }
            c.hit(row, W_LIST, idx);
        }
        let (base, mut y) = if side { (detail, 0) } else { (a, 2 + list_h as u16 + 1) };
        c.text(base, 0, y, d.name, Style::new().fg(th.ink).add_modifier(Modifier::BOLD));
        y += 1;
        for l in &reason {
            c.text(base, 0, y, l, fg(th.muted));
            y += 1;
        }
        y += 1;
        for (t, lines) in &tag_lines {
            c.text(base, 0, y, t, Style::new().fg(th.ink).add_modifier(Modifier::BOLD));
            for l in lines {
                c.text(base, LABEL_W, y, l, fg(th.muted));
                y += 1;
            }
        }
    }
}

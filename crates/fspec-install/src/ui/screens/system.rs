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
    if d.snapper {
        out.push((
            "snapper".into(),
            "Takes btrfs snapshots around every change, and the boot menu can start one.".into(),
        ));
    }
    match d.prerelease {
        Some("alpha") => out.push((
            "alpha".into(),
            "Still in development. Expect breakage; keep it off machines you depend on.".into(),
        )),
        Some(stage) => out.push((stage.into(), "Not released as stable yet. Expect rough edges.".into())),
        None => {}
    }
    out
}

/// Room for the longest name and its prerelease chip, so the tag chips line up in columns.
fn name_col() -> u16 {
    let chip = |d: &Distro| d.prerelease.map_or(0, |p| if p == "release candidate" { 5 } else { p.len() + 3 });
    DISTROS.iter().map(|d| d.name.len() + chip(d)).max().unwrap_or(0) as u16 + 2
}

pub struct System {
    list: FilterList,
    height: usize,
    /// The machine booted with UEFI, so distros that need it can be picked.
    efi: bool,
}

impl Default for System {
    fn default() -> Self {
        Self::new()
    }
}

impl System {
    pub fn new() -> Self {
        System { list: FilterList::new(DISTROS.len()), height: 6, efi: true }
    }

    pub fn current(&self) -> Option<&'static Distro> {
        self.list.selected().map(|i| &DISTROS[i])
    }

    pub fn captures_text(&self) -> bool {
        false
    }

    fn usable(&self, i: usize) -> bool {
        self.efi || !DISTROS[i].needs_efi
    }

    pub fn key(&mut self, k: &KeyEvent, sh: &mut Shared) -> Flow {
        self.efi = sh.firmware.efi;
        match k.code {
            KeyCode::Enter if self.list.selected().is_some_and(|i| self.usable(i)) => Flow::Next,
            KeyCode::Enter => Flow::Stay,
            KeyCode::Esc => Flow::Back,
            _ => {
                let efi = self.efi;
                self.list.nav(k, self.height, true, &|i| efi || !DISTROS[i].needs_efi);
                Flow::Stay
            }
        }
    }

    pub fn click(&mut self, h: Hit) -> Flow {
        if h.w != W_LIST || !self.usable(h.i) {
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

    pub fn render(&mut self, c: &mut Canvas, a: Rect, sh: &Shared) {
        self.efi = sh.firmware.efi;
        let th = *c.th;
        let side = a.width >= 100;
        let Some(d) = self.current() else { return };
        c.text(a, 0, 0, "Choose a system", Style::new().fg(th.ink).add_modifier(Modifier::BOLD));
        let list_w = if side { 2 + name_col() + 37 } else { a.width };
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
            .chain(
                d.needs_efi
                    .then(|| {
                        let e = "Installs only on machines that boot with UEFI; this one booted with BIOS.";
                        ("needs UEFI".to_string(), explain(e))
                    })
                    .filter(|_| !self.efi),
            )
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
            let st = match (sel, self.usable(idx)) {
                (_, false) => fg(th.faint),
                (true, true) => Style::new().fg(th.ink).add_modifier(Modifier::BOLD),
                (false, true) => fg(th.ink),
            };
            let used = c.text(row, 2, 0, dd.name, st);
            if let Some(stage) = dd.prerelease {
                let short = if stage == "release candidate" { "RC" } else { stage };
                c.text(row, 2 + used + 1, 0, &format!(" {short} "), fg(th.amber).bg(th.surface));
            }
            let mut x = 2 + name_col();
            for ((t, _), w) in tags(dd).iter().zip([9, 9, 10, 9]) {
                chip(c, row, x, t.trim_end_matches(" kernel"));
                x += w;
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

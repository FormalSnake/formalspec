use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::{Flow, Hints, Shared, hints, plain};
use crate::model::{DiskChoice, DiskInfo, Secret};
use crate::ui::theme::fg;
use crate::ui::widgets::controls::{chip, stepper, toggle};
use crate::ui::widgets::input::{FIELD_ROWS, Field, TextInput, draw_field};
use crate::ui::widgets::list::{FilterList, row_chrome};
use crate::ui::widgets::{Canvas, Hit, fmt_size, rect_in};

const W_LIST: u8 = 0;
const W_TOGGLE: u8 = 1;
const W_PASS: u8 = 2;
const W_CONFIRM: u8 = 3;
const W_SWAP: u8 = 4;
const EFI_BYTES: u64 = 2_000_000_000;
const MAX_SWAP: u64 = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    List,
    Toggle,
    Pass,
    Confirm,
    Swap,
}

pub struct DiskScreen {
    disks: Vec<DiskInfo>,
    list: FilterList,
    encrypt: bool,
    pass: TextInput,
    confirm: TextInput,
    swap: u64,
    focus: Focus,
    attempted: bool,
    height: usize,
}

impl Default for DiskScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl DiskScreen {
    pub fn new() -> Self {
        DiskScreen {
            disks: Vec::new(),
            list: FilterList::new(0),
            encrypt: false,
            pass: TextInput::new(true),
            confirm: TextInput::new(true),
            swap: 4,
            focus: Focus::List,
            attempted: false,
            height: 4,
        }
    }

    pub fn reload(&mut self, sh: &Shared) {
        let keep = self.selected().map(|d| d.path.clone());
        self.disks = sh.be.disks().unwrap_or_default();
        self.list = FilterList::new(self.disks.len());
        let ok = |i: usize| !self.disks[i].live_medium;
        match keep.and_then(|p| self.disks.iter().position(|d| d.path == p)) {
            Some(i) => self.list.select_item(i),
            None => self.list.home(&ok),
        }
    }

    pub fn selected(&self) -> Option<&DiskInfo> {
        self.list.selected().map(|i| &self.disks[i]).filter(|d| !d.live_medium)
    }

    pub fn captures_text(&self) -> bool {
        matches!(self.focus, Focus::Pass | Focus::Confirm)
    }

    fn pass_error(&self) -> Option<&'static str> {
        let show = self.attempted || (!self.pass.is_empty() && self.focus != Focus::Pass);
        (self.encrypt && show && self.pass.len() < 8).then_some("Use at least 8 characters.")
    }

    fn confirm_error(&self) -> Option<&'static str> {
        let show = self.attempted || (!self.confirm.is_empty() && self.focus != Focus::Confirm);
        (self.encrypt && show && self.confirm.value != self.pass.value).then_some("The passphrases do not match.")
    }

    fn valid(&self) -> bool {
        self.selected().is_some() && (!self.encrypt || (self.pass.len() >= 8 && self.pass.value == self.confirm.value))
    }

    pub fn value(&self) -> Option<DiskChoice> {
        if !self.valid() {
            return None;
        }
        Some(DiskChoice {
            device: self.selected()?.path.clone(),
            encrypt: self.encrypt.then(|| Secret(self.pass.value.clone())),
            swap_gib: self.swap,
        })
    }

    fn stops(&self) -> Vec<Focus> {
        let mut s = vec![Focus::List, Focus::Toggle];
        if self.encrypt {
            s.extend([Focus::Pass, Focus::Confirm]);
        }
        s.push(Focus::Swap);
        s
    }

    fn shift(&mut self, d: isize) {
        let s = self.stops();
        let p = s.iter().position(|f| *f == self.focus).unwrap_or(0) as isize;
        self.focus = s[(p + d).rem_euclid(s.len() as isize) as usize];
    }

    fn set_encrypt(&mut self, on: bool) {
        self.encrypt = on;
        if !on && matches!(self.focus, Focus::Pass | Focus::Confirm) {
            self.focus = Focus::Toggle;
        }
    }

    fn proceed(&mut self) -> Flow {
        if self.valid() {
            return Flow::Next;
        }
        self.attempted = true;
        if self.selected().is_none() {
            self.focus = Focus::List;
        } else if self.pass_error().is_some() {
            self.focus = Focus::Pass;
        } else {
            self.focus = Focus::Confirm;
        }
        Flow::Stay
    }

    pub fn key(&mut self, k: &KeyEvent, sh: &mut Shared) -> Flow {
        match k.code {
            KeyCode::Tab => {
                self.shift(1);
                return Flow::Stay;
            }
            KeyCode::BackTab => {
                self.shift(-1);
                return Flow::Stay;
            }
            KeyCode::Enter => return self.proceed(),
            KeyCode::Esc => return Flow::Back,
            _ => {}
        }
        match self.focus {
            Focus::List => {
                let disks = &self.disks;
                let ok = |i: usize| !disks[i].live_medium;
                if k.code == KeyCode::Down && self.list.sel + 1 >= self.list.vis.len() {
                    self.shift(1);
                } else if !self.list.nav(k, self.height, true, &ok) {
                    match k.code {
                        KeyCode::Char('r') if plain(k) => self.reload(sh),
                        KeyCode::Char(' ') => self.set_encrypt(!self.encrypt),
                        _ => {}
                    }
                }
            }
            Focus::Toggle => match k.code {
                KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right => self.set_encrypt(!self.encrypt),
                KeyCode::Up => self.shift(-1),
                KeyCode::Down => self.shift(1),
                _ => {}
            },
            Focus::Pass | Focus::Confirm => match k.code {
                KeyCode::Up => self.shift(-1),
                KeyCode::Down => self.shift(1),
                _ => {
                    let input = if self.focus == Focus::Pass { &mut self.pass } else { &mut self.confirm };
                    input.key(k);
                }
            },
            Focus::Swap => match k.code {
                KeyCode::Left | KeyCode::Char('h') => self.swap = self.swap.saturating_sub(1),
                KeyCode::Right | KeyCode::Char('l') => self.swap = (self.swap + 1).min(MAX_SWAP),
                KeyCode::Up => self.shift(-1),
                _ => {}
            },
        }
        Flow::Stay
    }

    pub fn click(&mut self, h: Hit) -> Flow {
        match h.w {
            W_LIST => {
                if self.disks.get(h.i).is_none_or(|d| d.live_medium) {
                    return Flow::Stay;
                }
                self.focus = Focus::List;
                if self.list.selected() == Some(h.i) {
                    return self.proceed();
                }
                self.list.select_item(h.i);
            }
            W_TOGGLE => {
                self.focus = Focus::Toggle;
                self.set_encrypt(!self.encrypt);
            }
            W_PASS => self.focus = Focus::Pass,
            W_CONFIRM => self.focus = Focus::Confirm,
            W_SWAP => {
                self.focus = Focus::Swap;
                self.swap = if h.i == 0 { self.swap.saturating_sub(1) } else { (self.swap + 1).min(MAX_SWAP) };
            }
            _ => {}
        }
        Flow::Stay
    }

    pub fn wheel(&mut self, w: u8, d: isize) {
        if w == W_LIST {
            self.list.wheel(d, self.height);
        }
    }

    pub fn hints(&self) -> Hints {
        match self.focus {
            Focus::List => hints(&[
                ("↑↓", "Move"),
                ("Space", "Encrypt"),
                ("r", "Rescan"),
                ("Enter", "Continue"),
                ("Esc", "Back"),
                ("?", "Keys"),
            ]),
            Focus::Swap => hints(&[("←→", "Change size"), ("Tab", "Next"), ("Enter", "Continue"), ("Esc", "Back")]),
            Focus::Pass | Focus::Confirm => hints(&[("Tab", "Next"), ("Enter", "Continue"), ("Esc", "Back")]),
            _ => hints(&[("Tab", "Next"), ("Space", "Toggle"), ("Enter", "Continue"), ("Esc", "Back")]),
        }
    }

    pub fn render(&mut self, c: &mut Canvas, a: Rect, sh: &Shared) {
        let th = *c.th;
        let bold = Style::new().fg(th.ink).add_modifier(Modifier::BOLD);
        c.text(a, 0, 0, "Choose a disk", bold);
        if self.disks.is_empty() {
            c.text(a, 0, 2, "No disks found", bold);
            c.text(a, 0, 3, "Connect a disk of 16 GB or more, then press r to look again.", fg(th.muted));
            return;
        }
        let enc_rows = if self.encrypt { FIELD_ROWS + 1 } else { 0 };
        let fixed = 2 + 1 + 2 + 1 + 1 + 1 + 1 + enc_rows + 1;
        let list_h = (a.height.saturating_sub(fixed) as usize).clamp(1, self.disks.len());
        self.height = list_h;
        let top = self.list.window(list_h);
        let size_w = 6;
        let chip_w = self.disks.iter().map(|d| d.transport.len() as u16).max().unwrap_or(4) + 2;
        let model_w = 20.min(a.width.saturating_sub(size_w + chip_w + 24));
        for (n, idx) in self.list.vis.clone().into_iter().skip(top).take(list_h).enumerate() {
            let d = &self.disks[idx];
            let row = Rect { y: a.y + 2 + n as u16, height: 1, ..a };
            let sel = self.list.selected() == Some(idx) && !d.live_medium;
            row_chrome(c, row, sel, self.focus == Focus::List);
            let name = if sel {
                Style::new().fg(th.ink).add_modifier(Modifier::BOLD)
            } else if d.live_medium {
                fg(th.faint)
            } else {
                fg(th.ink)
            };
            c.text_ell(row, 2, 0, &d.model, name);
            let size = fmt_size(d.bytes);
            let sx = 2 + model_w + 1 + size_w.saturating_sub(size.len() as u16);
            c.text(row, sx, 0, &size, if d.live_medium { fg(th.faint) } else { fg(th.muted) });
            let cx = 2 + model_w + 1 + size_w + 2;
            if d.live_medium {
                c.text(row, cx, 0, &format!(" {} ", d.transport), fg(th.faint));
                c.text_ell(row, cx + chip_w + 1, 0, "Installer USB", fg(th.faint));
            } else {
                chip(c, rect_in(row, cx, 0, chip_w, 1), 0, &d.transport);
                let contents = if d.contents.is_empty() { "Empty".to_string() } else { d.contents.join(", ") };
                c.text_ell(row, cx + chip_w + 1, 0, &contents, fg(th.muted));
            }
            if !d.live_medium {
                c.hit(row, W_LIST, idx);
            }
        }
        let mut y = 2 + list_h as u16 + 1;
        let sel = self.selected();
        if let Some(d) = sel {
            self.draw_bar(c, rect_in(a, 0, y, a.width.min(76), 2), d.bytes, sh);
            y += 3;
            if !d.contents.is_empty() {
                let msg = format!("Everything on {} ({}) will be erased.", d.model, fmt_size(d.bytes));
                c.text_ell(a, 2, y, &msg, fg(th.amber));
            }
            y += 2;
        } else {
            y += 5;
        }
        toggle(
            c,
            rect_in(a, 0, y, a.width, 1),
            self.encrypt,
            "Encrypt the disk",
            self.focus == Focus::Toggle,
            true,
            Hit { w: W_TOGGLE, i: 0 },
        );
        y += 1;
        if self.encrypt {
            let half = (a.width.saturating_sub(3) / 2).min(36);
            let pe = self.pass_error();
            let ce = self.confirm_error();
            draw_field(
                c,
                rect_in(a, 0, y, half, FIELD_ROWS),
                Field {
                    label: "Passphrase",
                    input: &self.pass,
                    placeholder: "",
                    focused: self.focus == Focus::Pass,
                    error: pe,
                    hint: None,
                    w: W_PASS,
                    i: 0,
                },
            );
            draw_field(
                c,
                rect_in(a, half + 3, y, half, FIELD_ROWS),
                Field {
                    label: "Confirm passphrase",
                    input: &self.confirm,
                    placeholder: "",
                    focused: self.focus == Focus::Confirm,
                    error: ce,
                    hint: None,
                    w: W_CONFIRM,
                    i: 0,
                },
            );
            y += FIELD_ROWS + 1;
        }
        let row = rect_in(a, 0, y, a.width, 1);
        if self.focus == Focus::Swap {
            c.text(row, 0, 0, "▌", fg(th.accent));
        }
        c.text(row, 2, 0, "Swap size", fg(th.muted));
        stepper(c, row, 14, &format!("{} GB", self.swap), self.focus == Focus::Swap, W_SWAP);
    }

    fn draw_bar(&self, c: &mut Canvas, a: Rect, disk_bytes: u64, sh: &Shared) {
        let th = *c.th;
        let efi_w = ((a.width as u64 * EFI_BYTES / disk_bytes.max(1)) as u16).max(10);
        let rest_x = efi_w + 1;
        let rest_w = a.width.saturating_sub(rest_x);
        if self.encrypt {
            c.text(a, rest_x, 0, "LUKS2 ", fg(th.accent));
            for x in rest_x + 6..a.width {
                c.text(a, x, 0, "─", fg(th.line));
            }
        }
        let efi = Rect { y: a.y + 1, height: 1, width: efi_w, ..a };
        let rest = Rect { x: a.x + rest_x, y: a.y + 1, height: 1, width: rest_w };
        let subs = format!("{} @home @swap @snapshots @fspec", sh.subvolume());
        if th.truecolor {
            c.fill(efi, Style::new().bg(th.sel));
            c.fill(rest, Style::new().bg(th.surface));
            c.text(efi, 1, 0, "EFI 2 GB", fg(th.ink));
            c.text_ell(rest, 1, 0, &subs, fg(th.muted));
        } else {
            c.text(efi, 0, 0, "[EFI 2 GB]", fg(th.ink));
            c.text_ell(rest, 0, 0, &format!("[{subs}]"), fg(th.muted));
        }
    }
}

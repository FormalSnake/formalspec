use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::{Flow, Hints, Shared, hints};
use crate::model::{Account, Secret};
use crate::ui::theme::fg;
use crate::ui::widgets::input::{FIELD_ROWS, Field, TextInput, draw_field};
use crate::ui::widgets::{Canvas, Hit, rect_in};

const NAME: usize = 0;
const LOGIN: usize = 1;
const HOST: usize = 2;
const PASS: usize = 3;
const CONFIRM: usize = 4;

const LOGIN_RULE: &str = "Start with a letter; use lowercase letters, digits, - and _.";
const HOST_RULE: &str = "Use lowercase letters, digits and hyphens.";

pub fn derive_login(name: &str) -> String {
    let first = name.split_whitespace().next().unwrap_or("");
    let s: String = first.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect();
    s.trim_start_matches(|c: char| !c.is_ascii_lowercase()).to_string()
}

fn login_ok(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next().is_some_and(|c| c.is_ascii_lowercase())
        && s.len() <= 32
        && cs.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn host_ok(s: &str) -> bool {
    (1..=63).contains(&s.len()) && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub struct AccountScreen {
    inputs: [TextInput; 5],
    login_dirty: bool,
    host_dirty: bool,
    focus: usize,
    attempted: bool,
    restore_user: String,
}

impl Default for AccountScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountScreen {
    pub fn new() -> Self {
        AccountScreen {
            inputs: [
                TextInput::default(),
                TextInput::default(),
                TextInput::default(),
                TextInput::new(true),
                TextInput::new(true),
            ],
            login_dirty: false,
            host_dirty: false,
            focus: NAME,
            attempted: false,
            restore_user: String::new(),
        }
    }

    /// Called whenever the screen is entered, since the distro or host may have changed.
    pub fn enter(&mut self, sh: &Shared) {
        if sh.restore {
            self.restore_user =
                sh.host.as_ref().and_then(|h| h.users.first().cloned()).unwrap_or_else(|| "user".into());
            if !matches!(self.focus, PASS | CONFIRM) {
                self.focus = PASS;
            }
        } else if self.focus >= PASS && self.inputs[PASS].is_empty() {
            self.focus = NAME;
        }
        self.derive_host(sh);
    }

    fn derive_host(&mut self, sh: &Shared) {
        if !self.host_dirty && !sh.restore {
            let id = sh.distro.map_or("system", |d| d.id);
            let login = &self.inputs[LOGIN].value;
            let v = if login.is_empty() { String::new() } else { format!("{login}-{id}") };
            self.inputs[HOST].set(&v);
        }
    }

    pub fn captures_text(&self) -> bool {
        true
    }

    pub fn value(&self, sh: &Shared) -> Account {
        if sh.restore {
            return Account {
                name: self.restore_user.clone(),
                full_name: self.restore_user.clone(),
                hostname: sh.host.as_ref().map(|h| h.name.clone()).unwrap_or_default(),
                password: Secret(self.inputs[PASS].value.clone()),
            };
        }
        Account {
            name: self.inputs[LOGIN].value.clone(),
            full_name: self.inputs[NAME].value.clone(),
            hostname: self.inputs[HOST].value.clone(),
            password: Secret(self.inputs[PASS].value.clone()),
        }
    }

    fn stops(&self, restore: bool) -> &'static [usize] {
        if restore { &[PASS, CONFIRM] } else { &[NAME, LOGIN, HOST, PASS, CONFIRM] }
    }

    fn error(&self, field: usize, restore: bool) -> Option<&'static str> {
        let v = &self.inputs[field].value;
        let shown = |blurred_nonempty: bool| self.attempted || (blurred_nonempty && self.focus != field);
        match field {
            NAME if !restore && self.attempted && v.trim().is_empty() => Some("Enter your name."),
            LOGIN if !restore && shown(!v.is_empty()) && !login_ok(v) => Some(LOGIN_RULE),
            HOST if !restore && shown(!v.is_empty()) && !host_ok(v) => Some(HOST_RULE),
            PASS if shown(!v.is_empty()) && v.chars().count() < 8 => Some("Use at least 8 characters."),
            CONFIRM if shown(!v.is_empty()) && *v != self.inputs[PASS].value => Some("The passwords do not match."),
            _ => None,
        }
    }

    fn proceed(&mut self, restore: bool) -> Flow {
        self.attempted = true;
        let first = self.stops(restore).iter().copied().find(|&f| self.error(f, restore).is_some());
        match first {
            Some(f) => {
                self.focus = f;
                Flow::Stay
            }
            None => Flow::Next,
        }
    }

    pub fn key(&mut self, k: &KeyEvent, sh: &mut Shared) -> Flow {
        let stops = self.stops(sh.restore);
        let pos = stops.iter().position(|&f| f == self.focus).unwrap_or(0);
        match k.code {
            KeyCode::Tab | KeyCode::Down => self.focus = stops[(pos + 1) % stops.len()],
            KeyCode::BackTab | KeyCode::Up => self.focus = stops[(pos + stops.len() - 1) % stops.len()],
            KeyCode::Enter => return self.proceed(sh.restore),
            KeyCode::Esc => return Flow::Back,
            _ => {
                let f = self.focus;
                let before = self.inputs[f].value.clone();
                self.inputs[f].key(k);
                if self.inputs[f].value != before {
                    match f {
                        NAME if !self.login_dirty => {
                            let l = derive_login(&self.inputs[NAME].value);
                            self.inputs[LOGIN].set(&l);
                        }
                        LOGIN => self.login_dirty = !self.inputs[LOGIN].is_empty(),
                        HOST => self.host_dirty = !self.inputs[HOST].is_empty(),
                        _ => {}
                    }
                    self.derive_host(sh);
                }
            }
        }
        Flow::Stay
    }

    pub fn click(&mut self, h: Hit) -> Flow {
        if (h.w as usize) < 5 {
            self.focus = h.w as usize;
        }
        Flow::Stay
    }

    pub fn hints(&self) -> Hints {
        hints(&[("Tab", "Next field"), ("Enter", "Continue"), ("Esc", "Back")])
    }

    pub fn render(&mut self, c: &mut Canvas, a: Rect, sh: &Shared) {
        let th = *c.th;
        let bold = Style::new().fg(th.ink).add_modifier(Modifier::BOLD);
        let w = a.width.min(66);
        let mut y = 2;
        let (title, fields): (String, &[(usize, &str, &str)]) = if sh.restore {
            (
                format!("Set a password for {}", self.restore_user),
                &[(PASS, "Password", ""), (CONFIRM, "Confirm password", "")],
            )
        } else {
            (
                "Create your account".to_string(),
                &[
                    (NAME, "Your name", ""),
                    (LOGIN, "Login name", ""),
                    (HOST, "Computer name", ""),
                    (PASS, "Password", ""),
                    (CONFIRM, "Confirm password", ""),
                ],
            )
        };
        c.text(a, 0, 0, &title, bold);
        if sh.restore {
            c.text(a, 0, 1, "Passwords are never stored in your spec.", fg(th.muted));
            y = 3;
        }
        let stride = if a.height >= 22 { FIELD_ROWS + 1 } else { FIELD_ROWS };
        for (f, label, ph) in fields {
            let err = self.error(*f, sh.restore);
            draw_field(
                c,
                rect_in(a, 0, y, w, FIELD_ROWS),
                Field {
                    label,
                    input: &self.inputs[*f],
                    placeholder: ph,
                    focused: self.focus == *f,
                    error: err,
                    hint: None,
                    w: *f as u8,
                    i: 0,
                },
            );
            y += stride;
        }
    }
}

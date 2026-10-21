use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::{Flow, Hints, Shared, hints};
use crate::model::{GitAuth, Secret};
use crate::ui::anim;
use crate::ui::theme::fg;
use crate::ui::widgets::controls::{Kind, button, segmented};
use crate::ui::widgets::input::{FIELD_ROWS, Field, TextInput, draw_field};
use crate::ui::widgets::{Canvas, Hit, rect_in};

const URL: usize = 0;
const AUTH: usize = 1;
const SECRET: usize = 2;
const GO: usize = 3;
const DEFAULT_KEY: &str = "/root/.ssh/id_ed25519";

pub struct Source {
    url: TextInput,
    key: TextInput,
    token: TextInput,
    auth: usize,
    focus: usize,
    pub cloning: Option<String>,
    error: Option<(String, String)>,
    empty_url: bool,
}

impl Default for Source {
    fn default() -> Self {
        Self::new()
    }
}

impl Source {
    pub fn new() -> Self {
        Source {
            url: TextInput::default(),
            key: TextInput::default(),
            token: TextInput::new(true),
            auth: 0,
            focus: URL,
            cloning: None,
            error: None,
            empty_url: false,
        }
    }

    pub fn captures_text(&self) -> bool {
        self.focus != AUTH && self.focus != GO
    }

    pub fn request(&self) -> (String, GitAuth) {
        let auth = match self.auth {
            1 => {
                GitAuth::SshKey(PathBuf::from(if self.key.is_empty() { DEFAULT_KEY } else { self.key.value.as_str() }))
            }
            2 => GitAuth::Token(Secret(self.token.value.clone())),
            _ => GitAuth::None,
        };
        (self.url.value.trim().to_string(), auth)
    }

    pub fn fail(&mut self, err: &str) {
        self.cloning = None;
        let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        self.error = Some(("Unable to clone. Check the URL and how you signed in, then try again.".into(), last));
    }

    pub fn done(&mut self) {
        self.cloning = None;
        self.error = None;
    }

    fn stops(&self) -> Vec<usize> {
        if self.auth == 0 { vec![URL, AUTH, GO] } else { vec![URL, AUTH, SECRET, GO] }
    }

    fn shift(&mut self, d: isize) {
        let s = self.stops();
        let p = s.iter().position(|&f| f == self.focus).unwrap_or(0) as isize;
        self.focus = s[(p + d).rem_euclid(s.len() as isize) as usize];
    }

    fn start(&mut self) -> Flow {
        if self.cloning.is_some() {
            return Flow::Stay;
        }
        if self.url.value.trim().is_empty() {
            self.empty_url = true;
            self.focus = URL;
            return Flow::Stay;
        }
        self.empty_url = false;
        self.error = None;
        self.cloning = Some(self.url.value.trim().to_string());
        Flow::Clone
    }

    pub fn key(&mut self, k: &KeyEvent, _sh: &mut Shared) -> Flow {
        if self.cloning.is_some() {
            return Flow::Stay;
        }
        match k.code {
            KeyCode::Tab | KeyCode::Down => self.shift(1),
            KeyCode::BackTab | KeyCode::Up => self.shift(-1),
            KeyCode::Enter => return self.start(),
            KeyCode::Esc => return Flow::Back,
            KeyCode::Left if self.focus == AUTH => self.auth = self.auth.saturating_sub(1),
            KeyCode::Right if self.focus == AUTH => self.auth = (self.auth + 1).min(2),
            _ => match self.focus {
                URL => {
                    self.url.key(k);
                    self.empty_url = false;
                }
                SECRET if self.auth == 1 => {
                    self.key.key(k);
                }
                SECRET => {
                    self.token.key(k);
                }
                _ => {}
            },
        }
        Flow::Stay
    }

    pub fn click(&mut self, h: Hit) -> Flow {
        if self.cloning.is_some() {
            return Flow::Stay;
        }
        match h.w as usize {
            URL => self.focus = URL,
            AUTH => {
                self.focus = AUTH;
                self.auth = h.i.min(2);
            }
            SECRET => self.focus = SECRET,
            GO => {
                self.focus = GO;
                return self.start();
            }
            _ => {}
        }
        Flow::Stay
    }

    pub fn hints(&self) -> Hints {
        hints(&[("Tab", "Next field"), ("←→", "Sign-in method"), ("Enter", "Clone"), ("Esc", "Back")])
    }

    pub fn render(&mut self, c: &mut Canvas, a: Rect, _sh: &Shared) {
        let th = *c.th;
        let w = a.width.min(66);
        c.text(a, 0, 0, "Where is your spec?", Style::new().fg(th.ink).add_modifier(Modifier::BOLD));
        let err = self.empty_url.then_some("Enter the address of your spec repository.");
        draw_field(
            c,
            rect_in(a, 0, 2, w, FIELD_ROWS),
            Field {
                label: "Spec repository",
                input: &self.url,
                placeholder: "git@github.com:you/spec.git",
                focused: self.focus == URL,
                error: err,
                hint: None,
                w: URL as u8,
                i: 0,
            },
        );
        let seg = rect_in(a, 0, 5, w, 2);
        c.text(seg, 0, 0, "Sign in with", if self.focus == AUTH { fg(th.accent) } else { fg(th.muted) });
        let row = Rect { y: seg.y + 1, height: 1, ..seg };
        segmented(c, row, 0, &["Nothing", "SSH key", "Access token"], self.auth, self.focus == AUTH, AUTH as u8);
        let mut y = 8;
        if self.auth != 0 {
            let (label, input, ph) =
                if self.auth == 1 { ("Key file", &self.key, DEFAULT_KEY) } else { ("Token", &self.token, "") };
            draw_field(
                c,
                rect_in(a, 0, y, w, FIELD_ROWS),
                Field {
                    label,
                    input,
                    placeholder: ph,
                    focused: self.focus == SECRET,
                    error: None,
                    hint: None,
                    w: SECRET as u8,
                    i: 0,
                },
            );
            y += FIELD_ROWS + 1;
        } else {
            y += 1;
        }
        let r = rect_in(a, 0, y, a.width, 1);
        if let Some(url) = &self.cloning {
            let sp = anim::spinner(c.epoch, c.now);
            c.text(r, 0, 0, sp, fg(th.accent));
            c.text_ell(r, 2, 0, &format!("Cloning {url}"), fg(th.ink));
        } else {
            button(c, r, 0, "Clone", Kind::Primary, self.focus == GO, Hit { w: GO as u8, i: 0 });
            if let Some((msg, last)) = &self.error {
                let (what, fix) = msg.split_once(". ").unwrap_or((msg, ""));
                c.text(a, 0, y + 2, &format!("{what}."), fg(th.red));
                c.text_ell(a, 0, y + 3, fix, fg(th.red));
                c.text_ell(a, 0, y + 4, last, fg(th.faint));
            }
        }
    }
}

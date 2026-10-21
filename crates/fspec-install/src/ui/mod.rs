//! The full-screen front end.

pub mod anim;
pub mod app;
pub mod screens;
pub mod theme;
pub mod widgets;

use std::io::stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Instant;

use anyhow::Result;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseEventKind};
use ratatui::crossterm::execute;

pub use app::{App, Input};
pub use screens::Exit;

use crate::backend::Backend;
use theme::Theme;

pub fn reduced_motion() -> bool {
    std::env::var("FSPEC_INSTALL_REDUCED_MOTION").is_ok_and(|v| v == "1")
}

/// Runs the installer until the user quits or the install finishes.
pub fn run(be: Arc<dyn Backend>, checkout: PathBuf) -> Result<Exit> {
    let theme = Theme::detect();
    let mut term = ratatui::init();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        hook(info);
    }));
    execute!(stdout(), EnableMouseCapture)?;
    let result = event_loop(&mut term, be, checkout, theme);
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn event_loop(
    term: &mut ratatui::DefaultTerminal,
    be: Arc<dyn Backend>,
    checkout: PathBuf,
    theme: Theme,
) -> Result<Exit> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            let input = match ev {
                Event::Key(k) if k.kind != KeyEventKind::Release => Input::Key(k),
                Event::Mouse(m) if !matches!(m.kind, MouseEventKind::Moved | MouseEventKind::Drag(_)) => {
                    Input::Mouse(m)
                }
                Event::Resize(w, h) => Input::Resize(w, h),
                _ => continue,
            };
            if tx.send(input).is_err() {
                break;
            }
        }
    });
    let size = term.size()?;
    let mut app = App::new(be.clone(), theme, !reduced_motion(), checkout, (size.width, size.height), Instant::now());
    let mut drawn = false;
    loop {
        let now = Instant::now();
        term.draw(|f| app.render(f.buffer_mut(), now))?;
        if !drawn {
            drawn = true;
            be.shown();
        }
        let first = match app.next_wake(now) {
            Some(d) => match rx.recv_timeout(d) {
                Ok(i) => i,
                Err(RecvTimeoutError::Timeout) => Input::Tick,
                Err(RecvTimeoutError::Disconnected) => return Ok(Exit::Quit),
            },
            None => match rx.recv() {
                Ok(i) => i,
                Err(_) => return Ok(Exit::Quit),
            },
        };
        let now = Instant::now();
        if let Some(e) = app.handle(first, now) {
            return Ok(e);
        }
        while let Ok(i) = rx.try_recv() {
            if let Some(e) = app.handle(i, now) {
                return Ok(e);
            }
        }
    }
}

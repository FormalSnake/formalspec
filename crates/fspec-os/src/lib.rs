//! Service managers, accounts and macOS defaults: the backends for `service`, `timer`, `user`
//! and `defaults` resources.

mod cron;
mod defaults;
mod launchd;
mod systemd;
#[cfg(test)]
mod testutil;
mod users;

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use fspec_core::Resource;
use fspec_core::facts::Init;

pub use defaults::DefaultsBackend;
pub use launchd::LaunchdBackend;
pub use systemd::SystemdBackend;
pub use users::UsersBackend;

/// Fails a service that sets `systemd = {...}` on an init other than systemd. Every non-systemd
/// service backend calls it from `check`, so the plan names the field instead of dropping it.
pub fn reject_systemd_extras(init: Init, r: &Resource) -> Result<()> {
    let Resource::Service(s) = r else { return Ok(()) };
    if s.systemd.is_empty() {
        return Ok(());
    }
    let keys: Vec<&str> = s.systemd.keys().map(String::as_str).collect();
    bail!(
        "service:{} sets `systemd` ({}), which only systemd supports, and this host uses {init}",
        s.name,
        keys.join(", ")
    )
}

/// Service and timer names become file names and init labels.
fn check_name(kind: &str, name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with(['.', '-'])
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '@'));
    if !ok {
        bail!("{kind} name `{name}` may only hold letters, digits and `_ . - @`, and must not start with `.` or `-`");
    }
    Ok(())
}

fn check_line(what: &str, value: &str) -> Result<()> {
    if value.contains(['\n', '\r']) {
        bail!("{what} must be a single line");
    }
    Ok(())
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

fn read_or_empty(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

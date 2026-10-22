//! `service("gdm")` without `exec`: a unit the distro ships, which fspec only enables at boot. It
//! is not started, so enabling a display manager or sshd on a running system takes effect at the
//! next boot, and the same apply works inside a chroot. On systemd, `user = True` enables a user
//! unit, such as one an app's own installer would write.

use anyhow::{Result, bail};
use fspec_core::apply::{Cx, Op};
use fspec_core::facts::Init;
use fspec_core::journal::{Journal, Undo};
use fspec_core::resource::{Restart, Service};
use fspec_core::runner::Cmd;
use serde_json::{Map, Value};

use crate::{check_name, initsvc};

/// Runit's default runlevel. `/var/service` resolves to it once booted, but only through `/run`.
const RUNLEVEL: &str = "/etc/runit/runsvdir/default";

/// The service when it names a distro unit rather than declaring its own.
pub(crate) fn of(s: &Service) -> Option<&Service> {
    s.exec.is_none().then_some(s)
}

pub(crate) fn check(init: Init, s: &Service) -> Result<()> {
    check_name("service", &s.name)?;
    let extras = [
        ("user", s.user && init != Init::Systemd),
        ("restart", s.restart != Restart::No),
        ("after", !s.after.is_empty()),
        ("env", !s.env.is_empty()),
        ("description", s.description.is_some()),
        ("systemd", !s.systemd.is_empty()),
    ];
    let set: Vec<&str> = extras.iter().filter(|(_, on)| *on).map(|(k, _)| *k).collect();
    if !set.is_empty() {
        bail!("service:{} has no exec, so it enables the distro's own unit, which takes no {}", s.name, set.join(", "));
    }
    if matches!(init, Init::S6 | Init::Launchd) {
        bail!("service:{} has no exec, and enabling a unit the system ships is not supported on {init}", s.name);
    }
    Ok(())
}

fn systemctl(verb: &str, s: &Service) -> Cmd {
    Cmd::new("systemctl").args(s.user.then_some("--user")).args([verb, s.name.as_str()])
}

fn rc_update(verb: &str, s: &Service) -> Cmd {
    Cmd::new("rc-update").args([verb, s.name.as_str(), "default"])
}

fn dinitctl(verb: &str, s: &Service) -> Cmd {
    Cmd::new("dinitctl").args([verb, s.name.as_str()])
}

fn runit_link(s: &Service) -> String {
    format!("{RUNLEVEL}/{}", s.name)
}

pub(crate) fn drift(cx: &Cx, init: Init, s: &Service) -> Result<Vec<String>> {
    let enabled = match init {
        Init::Systemd => {
            let out = cx.runner.run(&systemctl("is-enabled", s))?.stdout;
            // A runner that only logs answers nothing, which says nothing about the unit.
            out.trim().is_empty() || matches!(out.trim(), "enabled" | "alias" | "static" | "indirect")
        }
        Init::Runit => cx.resolve(&runit_link(s))?.symlink_metadata().is_ok(),
        Init::Openrc => cx.resolve(&format!("/etc/runlevels/default/{}", s.name))?.symlink_metadata().is_ok(),
        Init::Dinit => cx.resolve(&format!("/etc/dinit.d/boot.d/{}", s.name))?.symlink_metadata().is_ok(),
        _ => true,
    };
    Ok(if enabled { vec![] } else { vec![format!("{} is not enabled", s.name)] })
}

pub(crate) fn commit(cx: &Cx, init: Init, journal: &mut Journal, op: &Op, s: &Service) -> Result<Map<String, Value>> {
    let removal = op.is_removal();
    match init {
        Init::Systemd => {
            let (verb, undo) = if removal { ("disable", "enable") } else { ("enable", "disable") };
            journal.record(Undo::Run { cmd: systemctl(undo, s) })?;
            cx.runner.check(&systemctl(verb, s))?;
        }
        Init::Openrc => {
            let (verb, undo) = if removal { ("del", "add") } else { ("add", "del") };
            journal.record(Undo::Run { cmd: rc_update(undo, s) })?;
            cx.runner.check(&rc_update(verb, s))?;
        }
        Init::Dinit => {
            let (verb, undo) = if removal { ("disable", "enable") } else { ("enable", "disable") };
            journal.record(Undo::Run { cmd: dinitctl(undo, s) })?;
            cx.runner.check(&dinitctl(verb, s))?;
        }
        Init::Runit if removal => initsvc::unlink(cx, journal, &runit_link(s))?,
        Init::Runit => {
            let dir = format!("/etc/sv/{}", s.name);
            if !cx.resolve(&dir)?.is_dir() {
                bail!("service:{} has no exec, but {dir} does not exist; install the package that ships it", s.name);
            }
            initsvc::link(cx, journal, &runit_link(s), &dir)?;
        }
        other => bail!("enabling a unit the system ships is not supported on {other}"),
    }
    Ok(Map::new())
}

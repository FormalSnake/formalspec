//! runit (Void) services.
//!
//! A system service is `/etc/sv/fspec-<name>/run` (plus `finish` when it must not restart),
//! linked into `/var/service`. A user service lives directly in `~/.local/service/fspec-<name>`,
//! scanned by a `runsvdir` that fspec runs as the system service `fspec-runsvdir-<user>`; the
//! first user service on a host creates it, which needs the privileges to write `/etc/sv` and
//! `/var/service`. runsvdir starts a newly linked service within five seconds, so an add runs no
//! `sv` command. `after = ["network"]` is ignored (runit has no such state); another service name
//! becomes an `sv check` guard in `run`.

use std::path::Path;

use anyhow::Result;
use fspec_core::Resource;
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::Init;
use fspec_core::journal::{Journal, Undo};
use fspec_core::resource::{Restart, Service};
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use serde_json::{Map, Value};

use crate::initsvc::{self, Dep, Flavor, Part};
use crate::read_or_empty;

pub struct RunitBackend;

struct Runit;

impl Backend for RunitBackend {
    fn name(&self) -> &'static str {
        "runit"
    }

    fn handles(&self, cx: &Cx, r: &Resource) -> bool {
        initsvc::handles(&Runit, cx, r)
    }

    fn phase(&self) -> Phase {
        Phase::Services
    }

    fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        initsvc::check(&Runit, cx, r)
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        initsvc::drift(&Runit, cx, entry)
    }

    fn prepare(&self, cx: &Cx, ops: &[Op], staging: &Path) -> Result<()> {
        initsvc::prepare(&Runit, cx, ops, staging)
    }

    fn commit(&self, cx: &Cx, ops: &[Op], staging: &Path, journal: &mut Journal) -> Result<Records> {
        initsvc::commit(&Runit, cx, ops, staging, journal)
    }
}

fn dir(s: &Service) -> String {
    if s.user { format!("~/.local/service/fspec-{}", s.name) } else { format!("/etc/sv/fspec-{}", s.name) }
}

/// Where runsvdir sees the service.
fn scanned(s: &Service) -> String {
    if s.user { dir(s) } else { format!("/var/service/fspec-{}", s.name) }
}

fn sv(cx: &Cx, verb: &str, spec_path: &str) -> Result<Cmd> {
    Ok(Cmd::new("sv").arg(verb).arg(cx.resolve(spec_path)?.to_string_lossy().into_owned()))
}

impl Flavor for Runit {
    fn init(&self) -> Init {
        Init::Runit
    }

    fn name(&self) -> &'static str {
        "runit"
    }

    fn parts(&self, _cx: &Cx, s: &Service) -> Result<Vec<Part>> {
        let mut guards = String::new();
        for dep in initsvc::deps(Init::Runit, s)? {
            if let Dep::Service(d) = dep {
                guards.push_str(&format!("sv check ../fspec-{d} >/dev/null || exit 1\n"));
            }
        }
        let d = dir(s);
        let mut parts = vec![Part::script(format!("{d}/run"), initsvc::run_script(s, &guards)?)];
        // Exit status 0 from `finish` lets runsv restart the service, so a stop has to ask for it.
        match s.restart {
            Restart::Always => {}
            Restart::No => parts.push(Part::script(format!("{d}/finish"), "#!/bin/sh\nsv down .\n")),
            Restart::OnFailure => {
                parts.push(Part::script(format!("{d}/finish"), "#!/bin/sh\nif [ \"$1\" = 0 ]; then sv down .; fi\n"))
            }
        }
        Ok(parts)
    }

    fn commit_service(
        &self,
        cx: &Cx,
        journal: &mut Journal,
        op: &Op,
        parts: &[Part],
        staged: &Path,
    ) -> Result<Map<String, Value>> {
        let Resource::Service(s) = op.resource() else { unreachable!("commit_service gets services") };
        let (dir, live) = (dir(s), scanned(s));

        if op.is_removal() {
            journal.record(Undo::Run { cmd: sv(cx, "up", &live)? })?;
            cx.runner.run(&sv(cx, "down", &live)?)?;
            if !s.user {
                initsvc::unlink(cx, journal, &live)?;
            }
            // runsv keeps rewriting supervise/ inside the directory until it exits.
            let exit = Cmd::new("sv").args(["-v", "exit"]).arg(cx.resolve(&dir)?.to_string_lossy().into_owned());
            cx.runner.run(&exit)?;
            initsvc::reset_dir(cx, journal, &dir)?;
            return Ok(Map::new());
        }

        if s.user {
            ensure_runsvdir(cx, journal)?;
        }
        if op.current.is_some() {
            journal.record(Undo::Run { cmd: sv(cx, "restart", &live)? })?;
        }
        initsvc::reset_dir(cx, journal, &dir)?;
        initsvc::write_parts(cx, journal, parts, staged)?;
        if !s.user {
            initsvc::link(cx, journal, &live, &dir)?;
        }
        if op.current.is_some() {
            cx.runner.check(&sv(cx, "restart", &live)?)?;
        } else {
            journal.record(Undo::Run { cmd: sv(cx, "down", &live)? })?;
        }
        Ok(initsvc::record(parts))
    }
}

/// Declares `fspec-runsvdir-<user>` as a system service the first time a user service needs it.
fn ensure_runsvdir(cx: &Cx, journal: &mut Journal) -> Result<()> {
    let user = user_name(cx)?;
    let name = format!("fspec-runsvdir-{user}");
    let run = format!("/etc/sv/{name}/run");
    let content = format!(
        "#!/bin/sh\nexec 2>&1\nexec chpst -u {user} env HOME={home} runsvdir {home}/.local/service\n",
        home = shell_words::quote(&live_home(cx)),
    );
    if read_or_empty(&cx.resolve(&run)?).as_deref() == Some(content.as_str()) {
        return Ok(());
    }
    let live = format!("/var/service/{name}");
    initsvc::reset_dir(cx, journal, &format!("/etc/sv/{name}"))?;
    let part = [Part::script(&run, &content)];
    initsvc::write_direct(cx, journal, &part)?;
    initsvc::link(cx, journal, &live, &format!("/etc/sv/{name}"))?;
    journal.record(Undo::Run { cmd: sv(cx, "down", &live)? })?;
    Ok(())
}

fn user_name(cx: &Cx) -> Result<String> {
    let out = cx.runner.check(&Cmd::new("id").arg("-un"))?;
    let name = out.stdout.trim();
    if !name.is_empty() {
        return Ok(name.to_string());
    }
    // A rooted run only logs commands, so nothing answers.
    Ok(cx.paths.home.file_name().map_or_else(|| "user".into(), |n| n.to_string_lossy().into_owned()))
}

/// The home directory as the live system sees it, without the `--root` prefix.
fn live_home(cx: &Cx) -> String {
    let home = cx.paths.home.strip_prefix(&cx.paths.root).unwrap_or(&cx.paths.home);
    Path::new("/").join(home).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests;

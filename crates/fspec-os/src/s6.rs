//! s6-rc longruns.
//!
//! Each service is a source directory `/etc/s6/sv/fspec-<name>` holding `type` (`longrun`),
//! `run`, `finish` when it must not restart, and `dependencies.d/fspec-<dep>`. All fspec services
//! are listed in the bundle `fspec` (`/etc/s6/sv/fspec`), which the host's boot bundle has to
//! include. After the files change, the whole of `/etc/s6/sv` is compiled with `s6-rc-compile`
//! into `/etc/s6/db/fspec-g<generation>-<a|r>` and switched in with `s6-rc-update`; adds then run
//! `s6-rc -u change fspec`. Old databases stay in `/etc/s6/db`. `finish` exits 125 to stop
//! s6-supervise from restarting. `after = ["network"]` is ignored (s6-rc has no such service).
//! User services are not supported: fspec sets up no per-user s6 tree.

use std::path::Path;

use anyhow::{Result, bail};
use fspec_core::Resource;
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::Init;
use fspec_core::journal::{Journal, Undo, remove_any};
use fspec_core::resource::{Restart, Service};
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use serde_json::{Map, Value};

use crate::initsvc::{self, Dep, Flavor, Part};
use crate::read_or_empty;

pub struct S6Backend;

struct S6;

const SOURCES: &str = "/etc/s6/sv";
const BUNDLE: &str = "/etc/s6/sv/fspec";

impl Backend for S6Backend {
    fn name(&self) -> &'static str {
        "s6"
    }

    fn handles(&self, cx: &Cx, r: &Resource) -> bool {
        initsvc::handles(&S6, cx, r)
    }

    fn phase(&self) -> Phase {
        Phase::Services
    }

    fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        initsvc::check(&S6, cx, r)
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        initsvc::drift(&S6, cx, entry)
    }

    fn prepare(&self, cx: &Cx, ops: &[Op], staging: &Path) -> Result<()> {
        initsvc::prepare(&S6, cx, ops, staging)
    }

    fn commit(&self, cx: &Cx, ops: &[Op], staging: &Path, journal: &mut Journal) -> Result<Records> {
        initsvc::commit(&S6, cx, ops, staging, journal)
    }
}

fn dir(s: &Service) -> String {
    format!("{SOURCES}/fspec-{}", s.name)
}

fn has_services(ops: &[Op]) -> bool {
    ops.iter().any(|o| matches!(o.resource(), Resource::Service(_)))
}

/// A fresh database path per commit call, so a retry or a rollback never meets an old one.
fn db(cx: &Cx, ops: &[Op], suffix: &str) -> String {
    let tag = if ops.iter().all(Op::is_removal) { "r" } else { "a" };
    format!("/etc/s6/db/fspec-g{}-{tag}{suffix}", cx.generation)
}

fn compile(cx: &Cx, db: &str) -> Result<Cmd> {
    Ok(Cmd::new("s6-rc-compile")
        .args([cx.resolve(db)?, cx.resolve(SOURCES)?].map(|p| p.to_string_lossy().into_owned())))
}

fn update(cx: &Cx, db: &str) -> Result<Cmd> {
    Ok(Cmd::new("s6-rc-update").arg(cx.resolve(db)?.to_string_lossy().into_owned()))
}

impl Flavor for S6 {
    fn init(&self) -> Init {
        Init::S6
    }

    fn name(&self) -> &'static str {
        "s6"
    }

    fn check_service(&self, s: &Service) -> Result<()> {
        if s.user {
            bail!("service:{} sets user = True, but fspec sets up no per-user s6 supervision tree", s.name);
        }
        Ok(())
    }

    fn parts(&self, _cx: &Cx, s: &Service) -> Result<Vec<Part>> {
        let d = dir(s);
        let mut parts = vec![
            Part::new(format!("{d}/type"), "longrun\n"),
            Part::script(format!("{d}/run"), initsvc::run_script(s, "")?),
        ];
        match s.restart {
            Restart::Always => {}
            Restart::No => parts.push(Part::script(format!("{d}/finish"), "#!/bin/sh\nexit 125\n")),
            Restart::OnFailure => parts
                .push(Part::script(format!("{d}/finish"), "#!/bin/sh\nif [ \"$1\" = 0 ]; then exit 125; fi\nexit 0\n")),
        }
        for dep in initsvc::deps(Init::S6, s)? {
            if let Dep::Service(name) = dep {
                parts.push(Part::new(format!("{d}/dependencies.d/fspec-{name}"), ""));
            }
        }
        parts.push(Part::new(format!("{BUNDLE}/contents.d/fspec-{}", s.name), ""));
        Ok(parts)
    }

    fn begin(&self, cx: &Cx, journal: &mut Journal, ops: &[Op]) -> Result<()> {
        if !has_services(ops) {
            return Ok(());
        }
        // Replayed backwards after the files are restored: recompile, then switch to it.
        let undo = db(cx, ops, "-undo");
        journal.record(Undo::Run { cmd: update(cx, &undo)? })?;
        journal.record(Undo::Run { cmd: compile(cx, &undo)? })?;
        Ok(())
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

        if op.is_removal() {
            initsvc::reset_dir(cx, journal, &dir(s))?;
            initsvc::unlink(cx, journal, &format!("{BUNDLE}/contents.d/fspec-{}", s.name))?;
            return Ok(Map::new());
        }

        initsvc::reset_dir(cx, journal, &dir(s))?;
        let bundle_type = format!("{BUNDLE}/type");
        if read_or_empty(&cx.resolve(&bundle_type)?).is_none() {
            initsvc::write_direct(cx, journal, &[Part::new(bundle_type, "bundle\n")])?;
        }
        initsvc::write_parts(cx, journal, parts, staged)?;
        Ok(initsvc::record(parts))
    }

    fn end(&self, cx: &Cx, _journal: &mut Journal, ops: &[Op]) -> Result<()> {
        if !has_services(ops) {
            return Ok(());
        }
        let new = db(cx, ops, "");
        remove_any(&cx.resolve(&new)?)?;
        cx.runner.check(&compile(cx, &new)?)?;
        cx.runner.check(&update(cx, &new)?)?;
        if !ops.iter().all(Op::is_removal) {
            cx.runner.check(&Cmd::new("s6-rc").args(["-u", "change", "fspec"]))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

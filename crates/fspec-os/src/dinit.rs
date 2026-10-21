//! dinit services: `/etc/dinit.d/fspec-<name>` or, for user services, `~/.config/dinit.d/`.
//!
//! `type = process`, `restart` maps `always`, `on_failure` and `no` to `true`, `on-failure` and
//! `false`. `after = ["network"]` is `depends-on = network` (system services only, a user dinit
//! has no such service), another name is `depends-on = fspec-<name>`. `enable` needs the `boot`
//! service to carry a `waits-for.d` directory, as the stock dinit setups do. `description` is
//! written as a comment.

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

pub struct DinitBackend;

struct Dinit;

impl Backend for DinitBackend {
    fn name(&self) -> &'static str {
        "dinit"
    }

    fn handles(&self, cx: &Cx, r: &Resource) -> bool {
        initsvc::handles(&Dinit, cx, r)
    }

    fn phase(&self) -> Phase {
        Phase::Services
    }

    fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        initsvc::check(&Dinit, cx, r)
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        initsvc::drift(&Dinit, cx, entry)
    }

    fn prepare(&self, cx: &Cx, ops: &[Op], staging: &Path) -> Result<()> {
        initsvc::prepare(&Dinit, cx, ops, staging)
    }

    fn commit(&self, cx: &Cx, ops: &[Op], staging: &Path, journal: &mut Journal) -> Result<Records> {
        initsvc::commit(&Dinit, cx, ops, staging, journal)
    }
}

fn file_path(s: &Service) -> String {
    let dir = if s.user { "~/.config/dinit.d" } else { "/etc/dinit.d" };
    format!("{dir}/fspec-{}", s.name)
}

fn ctl(s: &Service, verb: &str) -> Cmd {
    Cmd::new("dinitctl").args(s.user.then_some("--user")).args([verb.to_string(), format!("fspec-{}", s.name)])
}

impl Flavor for Dinit {
    fn init(&self) -> Init {
        Init::Dinit
    }

    fn name(&self) -> &'static str {
        "dinit"
    }

    fn parts(&self, _cx: &Cx, s: &Service) -> Result<Vec<Part>> {
        let argv = crate::argv(s.exec(), &s.env)?;
        let mut out = initsvc::HEADER.to_string();
        if let Some(d) = initsvc::describe(s)? {
            out.push_str(&format!("# {d}\n"));
        }
        out.push_str("type = process\n");
        out.push_str(&format!("command = {}\n", shell_words::join(&argv)));
        out.push_str(match s.restart {
            Restart::No => "restart = false\n",
            Restart::OnFailure => "restart = on-failure\n",
            Restart::Always => "restart = true\n",
        });
        for dep in initsvc::deps(Init::Dinit, s)? {
            match dep {
                Dep::Network if !s.user => out.push_str("depends-on = network\n"),
                Dep::Network => {}
                Dep::Service(d) => out.push_str(&format!("depends-on = fspec-{d}\n")),
            }
        }
        Ok(vec![Part::new(file_path(s), out)])
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
            journal.record(Undo::Run { cmd: ctl(s, "start") })?;
            journal.record(Undo::Run { cmd: ctl(s, "enable") })?;
            cx.runner.run(&ctl(s, "stop"))?;
            cx.runner.run(&ctl(s, "disable"))?;
            initsvc::remove_parts(cx, journal, parts)?;
            return Ok(Map::new());
        }

        // dinit keeps the description it loaded, so a changed file needs `reload` before `restart`.
        if op.current.is_some() {
            journal.record(Undo::Run { cmd: ctl(s, "restart") })?;
            journal.record(Undo::Run { cmd: ctl(s, "reload") })?;
        }
        initsvc::write_parts(cx, journal, parts, staged)?;
        if op.current.is_some() {
            cx.runner.check(&ctl(s, "reload"))?;
            cx.runner.check(&ctl(s, "restart"))?;
        } else {
            journal.record(Undo::Run { cmd: ctl(s, "disable") })?;
            cx.runner.check(&ctl(s, "enable"))?;
            journal.record(Undo::Run { cmd: ctl(s, "stop") })?;
            cx.runner.check(&ctl(s, "start"))?;
        }
        Ok(initsvc::record(parts))
    }
}

#[cfg(test)]
mod tests;

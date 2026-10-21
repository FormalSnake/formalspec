//! OpenRC services: `/etc/init.d/fspec-<name>`, added to the `default` runlevel.
//!
//! The script runs `exec` under `supervise-daemon`, which respawns on every exit, so
//! `on_failure` behaves like `always`. `restart = "no"` drops the supervisor and daemonizes with
//! `command_background`. `after = ["network"]` is `need net`, another service name is
//! `after fspec-<name>`. OpenRC has no user services, so `user = True` fails the plan.

use std::path::Path;

use anyhow::{Result, bail};
use fspec_core::Resource;
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::Init;
use fspec_core::journal::{Journal, Undo};
use fspec_core::resource::{Restart, Service};
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use serde_json::{Map, Value};

use crate::initsvc::{self, Dep, Flavor, Part};

pub struct OpenrcBackend;

struct Openrc;

impl Backend for OpenrcBackend {
    fn name(&self) -> &'static str {
        "openrc"
    }

    fn handles(&self, cx: &Cx, r: &Resource) -> bool {
        initsvc::handles(&Openrc, cx, r)
    }

    fn phase(&self) -> Phase {
        Phase::Services
    }

    fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        initsvc::check(&Openrc, cx, r)
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        initsvc::drift(&Openrc, cx, entry)
    }

    fn prepare(&self, cx: &Cx, ops: &[Op], staging: &Path) -> Result<()> {
        initsvc::prepare(&Openrc, cx, ops, staging)
    }

    fn commit(&self, cx: &Cx, ops: &[Op], staging: &Path, journal: &mut Journal) -> Result<Records> {
        initsvc::commit(&Openrc, cx, ops, staging, journal)
    }
}

fn script_path(s: &Service) -> String {
    format!("/etc/init.d/fspec-{}", s.name)
}

fn rc_service(s: &Service, verb: &str) -> Cmd {
    Cmd::new("rc-service").args([format!("fspec-{}", s.name), verb.to_string()])
}

fn rc_update(s: &Service, verb: &str) -> Cmd {
    Cmd::new("rc-update").args([verb.to_string(), format!("fspec-{}", s.name), "default".to_string()])
}

impl Flavor for Openrc {
    fn init(&self) -> Init {
        Init::Openrc
    }

    fn name(&self) -> &'static str {
        "openrc"
    }

    fn check_service(&self, s: &Service) -> Result<()> {
        if s.user {
            bail!("service:{} sets user = True, but OpenRC has no user services", s.name);
        }
        Ok(())
    }

    fn parts(&self, _cx: &Cx, s: &Service) -> Result<Vec<Part>> {
        let argv = crate::argv(s.exec(), &Default::default())?;
        let mut out = format!("#!/sbin/openrc-run\n{}\n", initsvc::HEADER);
        if let Some(d) = initsvc::describe(s)? {
            out.push_str(&format!("description={}\n", shell_words::quote(d)));
        }
        match s.restart {
            Restart::No => out.push_str("command_background=true\npidfile=\"/run/${RC_SVCNAME}.pid\"\n"),
            Restart::Always | Restart::OnFailure => out.push_str("supervisor=supervise-daemon\n"),
        }
        out.push_str(&format!("command={}\n", shell_words::quote(&argv[0])));
        if argv.len() > 1 {
            out.push_str(&format!("command_args={}\n", shell_words::quote(&shell_words::join(&argv[1..]))));
        }
        out.push_str(&initsvc::exports(s)?);

        let mut deps = String::new();
        for dep in initsvc::deps(Init::Openrc, s)? {
            match dep {
                Dep::Network => deps.push_str("\tneed net\n"),
                Dep::Service(d) => deps.push_str(&format!("\tafter fspec-{d}\n")),
            }
        }
        if !deps.is_empty() {
            out.push_str(&format!("\ndepend() {{\n{deps}}}\n"));
        }
        Ok(vec![Part::script(script_path(s), out)])
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
            journal.record(Undo::Run { cmd: rc_service(s, "start") })?;
            journal.record(Undo::Run { cmd: rc_update(s, "add") })?;
            cx.runner.run(&rc_service(s, "stop"))?;
            cx.runner.run(&rc_update(s, "del"))?;
            initsvc::remove_parts(cx, journal, parts)?;
            return Ok(Map::new());
        }

        if op.current.is_some() {
            journal.record(Undo::Run { cmd: rc_service(s, "restart") })?;
        }
        initsvc::write_parts(cx, journal, parts, staged)?;
        if op.current.is_some() {
            cx.runner.check(&rc_service(s, "restart"))?;
        } else {
            journal.record(Undo::Run { cmd: rc_update(s, "del") })?;
            cx.runner.check(&rc_update(s, "add"))?;
            journal.record(Undo::Run { cmd: rc_service(s, "stop") })?;
            cx.runner.check(&rc_service(s, "start"))?;
        }
        Ok(initsvc::record(parts))
    }
}

#[cfg(test)]
mod tests;

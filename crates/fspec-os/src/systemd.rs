//! systemd units for `service` and `timer` resources.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::Init;
use fspec_core::journal::{Journal, Undo, remove_any};
use fspec_core::resource::{Restart, Service, Timer};
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use fspec_core::{Resource, Scope};
use serde_json::{Map, Value, json};

use crate::cron::Schedule;
use crate::{check_line, check_name, read_or_empty, write_file};

pub struct SystemdBackend;

/// A rendered unit file. `path` is a spec path, resolved against the rooted paths when used.
struct Unit {
    path: String,
    file: String,
    content: String,
}

impl Backend for SystemdBackend {
    fn name(&self) -> &'static str {
        "systemd"
    }

    fn handles(&self, cx: &Cx, r: &Resource) -> bool {
        cx.facts.init == Init::Systemd && matches!(r, Resource::Service(_) | Resource::Timer(_))
    }

    fn phase(&self) -> Phase {
        Phase::Services
    }

    fn check(&self, _cx: &Cx, r: &Resource) -> Result<()> {
        units(r).map(drop)
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        let mut notes = Vec::new();
        for u in units(&entry.resource)? {
            match read_or_empty(&cx.resolve(&u.path)?) {
                None => notes.push(format!("{} is missing", u.path)),
                Some(on_disk) if on_disk != u.content => notes.push(format!("{} was edited", u.path)),
                Some(_) => {}
            }
        }
        Ok(notes)
    }

    fn prepare(&self, _cx: &Cx, ops: &[Op], staging: &Path) -> Result<()> {
        for op in ops.iter().filter(|o| !o.is_removal()) {
            for u in units(op.resource())? {
                write_file(&staged(staging, op, &u), &u.content)?;
            }
        }
        Ok(())
    }

    fn commit(&self, cx: &Cx, ops: &[Op], staging: &Path, journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        for op in ops {
            let user = is_user(op.resource());
            let main = main_unit(op.resource());
            let ctl = |args: &[&str]| ctl(user, args);
            let us = units(op.resource())?;

            // Undo steps replay backwards: files are restored first, then the reload, then the
            // unit is brought back to its old state, so that step is recorded first.
            // systemctl refuses `--now` in a chroot but ignores start and stop there, so a root
            // built by `switch --distro` still applies.
            if op.is_removal() {
                journal.record(Undo::Run { cmd: ctl(&["start", &main]) })?;
                journal.record(Undo::Run { cmd: ctl(&["enable", &main]) })?;
            } else if op.current.is_some() {
                journal.record(Undo::Run { cmd: ctl(&["restart", &main]) })?;
            }
            journal.record(Undo::Run { cmd: ctl(&["daemon-reload"]) })?;
            if op.is_removal() {
                for u in &us {
                    journal.backup(&cx.resolve(&u.path)?)?;
                }
                cx.runner.run(&ctl(&["stop", &main]))?;
                cx.runner.run(&ctl(&["disable", &main]))?;
                for u in &us {
                    remove_any(&cx.resolve(&u.path)?)?;
                }
                cx.runner.check(&ctl(&["daemon-reload"]))?;
                continue;
            }

            for u in &us {
                let dest = cx.resolve(&u.path)?;
                journal.backup(&dest)?;
                write_file(&dest, &fs::read_to_string(staged(staging, op, u))?)?;
            }
            cx.runner.check(&ctl(&["daemon-reload"]))?;
            if op.current.is_some() {
                cx.runner.check(&ctl(&["restart", &main]))?;
            } else {
                journal.record(Undo::Run { cmd: ctl(&["disable", &main]) })?;
                journal.record(Undo::Run { cmd: ctl(&["stop", &main]) })?;
                cx.runner.check(&ctl(&["enable", &main]))?;
                cx.runner.check(&ctl(&["start", &main]))?;
            }
            let mut rec = Map::new();
            rec.insert("units".into(), json!(us.iter().map(|u| u.path.clone()).collect::<Vec<_>>()));
            records.insert(op.key.clone(), rec);
        }
        Ok(records)
    }
}

fn staged(staging: &Path, op: &Op, u: &Unit) -> std::path::PathBuf {
    staging.join("systemd").join(op.key.to_string().replace(':', "-")).join(&u.file)
}

fn ctl(user: bool, args: &[&str]) -> Cmd {
    Cmd::new("systemctl").args(user.then_some("--user")).args(args.iter().copied())
}

fn is_user(r: &Resource) -> bool {
    r.scope() == Scope::User
}

fn main_unit(r: &Resource) -> String {
    match r {
        Resource::Timer(t) => format!("fspec-{}.timer", t.name),
        Resource::Service(s) => format!("fspec-{}.service", s.name),
        _ => unreachable!("systemd claims services and timers"),
    }
}

fn units(r: &Resource) -> Result<Vec<Unit>> {
    match r {
        Resource::Service(s) => {
            check_name("service", &s.name)?;
            Ok(vec![unit(s.user, &format!("fspec-{}.service", s.name), render_service(s)?)])
        }
        Resource::Timer(t) => {
            check_name("timer", &t.name)?;
            Ok(vec![
                unit(t.user, &format!("fspec-{}.service", t.name), render_timer_service(t)?),
                unit(t.user, &format!("fspec-{}.timer", t.name), render_timer(t)?),
            ])
        }
        _ => unreachable!("systemd claims services and timers"),
    }
}

fn unit(user: bool, file: &str, content: String) -> Unit {
    let dir = if user { "~/.config/systemd/user" } else { "/etc/systemd/system" };
    Unit { path: format!("{dir}/{file}"), file: file.into(), content }
}

const HEADER: &str = "# Managed by fspec. Edits are overwritten on the next apply.\n";

/// Directives in file order. `set` replaces every earlier line for the same name.
#[derive(Default)]
struct Section(Vec<(String, String)>);

impl Section {
    fn push(&mut self, key: &str, value: impl Into<String>) {
        self.0.push((key.into(), value.into()));
    }

    fn set(&mut self, key: &str, values: Vec<String>) {
        self.0.retain(|(k, _)| k != key);
        self.0.extend(values.into_iter().map(|v| (key.to_string(), v)));
    }

    fn render(&self, title: &str, out: &mut String) {
        if self.0.is_empty() {
            return;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("[{title}]\n"));
        for (k, v) in &self.0 {
            out.push_str(&format!("{k}={v}\n"));
        }
    }
}

fn assemble(sections: [(&str, &Section); 3]) -> String {
    let mut out = HEADER.to_string();
    let mut body = String::new();
    for (title, s) in sections {
        s.render(title, &mut body);
    }
    if !body.is_empty() {
        out.push('\n');
    }
    out.push_str(&body);
    out
}

/// systemd does not search `PATH` for `ExecStart=`, so a bare command name goes through `env`.
/// `%` starts a specifier in unit files and is doubled; `$VAR` expansion is left to systemd.
fn exec_start(exec: &str) -> Result<String> {
    check_line("exec", exec)?;
    let exec = exec.trim();
    let Some(first) = exec.split_whitespace().next() else { bail!("exec is empty") };
    let exec = exec.replace('%', "%%");
    Ok(if first.starts_with('/') { exec } else { format!("/usr/bin/env {exec}") })
}

fn description(desc: Option<&str>, fallback: String) -> Result<String> {
    let d = desc.map(str::to_owned).unwrap_or(fallback);
    check_line("description", &d)?;
    Ok(d)
}

fn render_service(s: &Service) -> Result<String> {
    let mut unit = Section::default();
    let mut service = Section::default();
    let mut install = Section::default();

    unit.push("Description", description(s.description.as_deref(), s.name.clone())?);
    for dep in &s.after {
        check_line("after", dep)?;
        match dep.as_str() {
            "network" => {
                unit.push("After", "network-online.target");
                // User managers have no network-online.target to pull in.
                if !s.user {
                    unit.push("Wants", "network-online.target");
                }
            }
            d if d.ends_with(".target") => unit.push("After", d),
            d => {
                check_name("after", d)?;
                unit.push("After", format!("fspec-{d}.service"));
            }
        }
    }

    service.push("ExecStart", exec_start(&s.exec)?);
    match s.restart {
        Restart::No => {}
        Restart::OnFailure => service.push("Restart", "on-failure"),
        Restart::Always => service.push("Restart", "always"),
    }
    for (k, v) in &s.env {
        if k.is_empty() || k.contains(['=', '\n', ' ']) || v.contains('\n') {
            bail!("env entry `{k}` of service {} cannot be written to a unit", s.name);
        }
        let v = v.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%");
        service.push("Environment", format!("\"{k}={v}\""));
    }
    install.push("WantedBy", if s.user { "default.target" } else { "multi-user.target" });

    apply_extras(&s.systemd, &mut unit, &mut service, &mut install)
        .with_context(|| format!("service {}: systemd = {{...}}", s.name))?;
    Ok(assemble([("Unit", &unit), ("Service", &service), ("Install", &install)]))
}

fn apply_extras(
    extras: &BTreeMap<String, Value>,
    unit: &mut Section,
    service: &mut Section,
    install: &mut Section,
) -> Result<()> {
    for (key, value) in extras {
        let target = match key.as_str() {
            "unit" => Some(&mut *unit),
            "service" => Some(&mut *service),
            "install" => Some(&mut *install),
            _ => None,
        };
        match target {
            Some(section) => {
                let Value::Object(map) = value else { bail!("`{key}` must be a dict of directives") };
                for (k, v) in map {
                    section.set(&directive(k)?, values(k, v)?);
                }
            }
            None => service.set(&directive(key)?, values(key, value)?),
        }
    }
    Ok(())
}

fn directive(name: &str) -> Result<String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("`{name}` is not a systemd directive name");
    }
    Ok(name.into())
}

/// A scalar is one line, a list repeats the directive. Booleans become `yes` and `no`.
fn values(key: &str, v: &Value) -> Result<Vec<String>> {
    let one = |v: &Value| -> Result<String> {
        let s = match v {
            Value::String(s) => s.clone(),
            Value::Bool(b) => if *b { "yes" } else { "no" }.into(),
            Value::Number(n) => n.to_string(),
            _ => bail!("`{key}` holds a value systemd cannot take (use a string, number, bool or list of those)"),
        };
        check_line(key, &s)?;
        Ok(s)
    };
    match v {
        Value::Array(items) => items.iter().map(one).collect(),
        v => Ok(vec![one(v)?]),
    }
}

fn render_timer_service(t: &Timer) -> Result<String> {
    let mut unit = Section::default();
    let mut service = Section::default();
    unit.push("Description", format!("{} (run by fspec-{}.timer)", t.name, t.name));
    service.push("Type", "oneshot");
    service.push("ExecStart", exec_start(&t.exec)?);
    Ok(assemble([("Unit", &unit), ("Service", &service), ("Install", &Section::default())]))
}

fn render_timer(t: &Timer) -> Result<String> {
    let schedule = Schedule::parse(&t.schedule).with_context(|| format!("timer {}", t.name))?;
    let mut unit = Section::default();
    let mut timer = Section::default();
    let mut install = Section::default();
    unit.push("Description", format!("{} schedule", t.name));
    timer.push("OnCalendar", schedule.on_calendar());
    timer.push("Persistent", "true");
    install.push("WantedBy", "timers.target");
    Ok(assemble([("Unit", &unit), ("Timer", &timer), ("Install", &install)]))
}

#[cfg(test)]
mod tests;

//! What runit, OpenRC, dinit and s6 share: claiming `service` and `timer` resources, rendering
//! service files, drift, staging and the commit loop. Each init supplies a [`Flavor`]; timers on
//! all of them go to `crond`.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use fspec_core::Resource;
use fspec_core::apply::{Cx, Op, Records};
use fspec_core::facts::Init;
use fspec_core::journal::{Journal, remove_any};
use fspec_core::resource::Service;
use fspec_core::state::Entry;
use serde_json::{Map, Value, json};

use crate::{check_line, check_name, distro_unit};
use crate::{crond, read_or_empty, reject_systemd_extras, write_file};

/// One file a service is rendered to. `path` is a spec path, resolved against the rooted paths.
pub(crate) struct Part {
    pub path: String,
    pub content: String,
    pub exec: bool,
}

impl Part {
    pub fn new(path: impl Into<String>, content: impl Into<String>) -> Part {
        Part { path: path.into(), content: content.into(), exec: false }
    }

    pub fn script(path: impl Into<String>, content: impl Into<String>) -> Part {
        Part { exec: true, ..Part::new(path, content) }
    }
}

pub(crate) trait Flavor: Sync {
    fn init(&self) -> Init;

    fn name(&self) -> &'static str;

    /// Fail a service this init cannot run (OpenRC and s6 have no user services).
    fn check_service(&self, _s: &Service) -> Result<()> {
        Ok(())
    }

    fn parts(&self, cx: &Cx, s: &Service) -> Result<Vec<Part>>;

    /// Runs once per commit call, before any file changes, for undo steps that span services.
    fn begin(&self, _cx: &Cx, _journal: &mut Journal, _ops: &[Op]) -> Result<()> {
        Ok(())
    }

    /// Make one service live. `staged` holds the rendered `parts` by index. Journal before each
    /// mutation, undo steps replay backwards.
    fn commit_service(
        &self,
        cx: &Cx,
        journal: &mut Journal,
        op: &Op,
        parts: &[Part],
        staged: &Path,
    ) -> Result<Map<String, Value>>;

    /// Runs once per commit call after every service file is in place.
    fn end(&self, _cx: &Cx, _journal: &mut Journal, _ops: &[Op]) -> Result<()> {
        Ok(())
    }
}

pub(crate) fn handles(f: &dyn Flavor, cx: &Cx, r: &Resource) -> bool {
    cx.facts.init == f.init() && matches!(r, Resource::Service(_) | Resource::Timer(_))
}

pub(crate) fn check(f: &dyn Flavor, cx: &Cx, r: &Resource) -> Result<()> {
    reject_systemd_extras(cx.facts.init, r)?;
    match r {
        Resource::Service(s) if distro_unit::of(s).is_some() => distro_unit::check(f.init(), s),
        Resource::Service(s) => {
            check_name("service", &s.name)?;
            f.check_service(s)?;
            f.parts(cx, s).map(drop)
        }
        Resource::Timer(t) => crond::check(t),
        _ => unreachable!("{} claims services and timers", f.name()),
    }
}

pub(crate) fn drift(f: &dyn Flavor, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
    match &entry.resource {
        Resource::Service(s) if distro_unit::of(s).is_some() => distro_unit::drift(cx, f.init(), s),
        Resource::Service(s) => {
            let mut notes = Vec::new();
            for p in f.parts(cx, s)? {
                match read_or_empty(&cx.resolve(&p.path)?) {
                    None => notes.push(format!("{} is missing", p.path)),
                    Some(on_disk) if on_disk != p.content => notes.push(format!("{} was edited", p.path)),
                    Some(_) => {}
                }
            }
            Ok(notes)
        }
        Resource::Timer(t) => crond::drift(cx, t),
        _ => unreachable!("{} claims services and timers", f.name()),
    }
}

pub(crate) fn prepare(f: &dyn Flavor, cx: &Cx, ops: &[Op], staging: &Path) -> Result<()> {
    for op in ops.iter().filter(|o| !o.is_removal()) {
        if let Resource::Service(s) = op.resource()
            && distro_unit::of(s).is_none()
        {
            let dir = staged_dir(f, staging, op);
            for (i, p) in f.parts(cx, s)?.iter().enumerate() {
                write_file(&dir.join(i.to_string()), &p.content)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn commit(f: &dyn Flavor, cx: &Cx, ops: &[Op], staging: &Path, journal: &mut Journal) -> Result<Records> {
    let mut records = Records::new();
    f.begin(cx, journal, ops)?;
    for op in ops {
        let record = match op.resource() {
            Resource::Service(s) if distro_unit::of(s).is_some() => distro_unit::commit(cx, f.init(), journal, op, s)?,
            Resource::Service(s) => {
                let parts = f.parts(cx, s)?;
                f.commit_service(cx, journal, op, &parts, &staged_dir(f, staging, op))?
            }
            Resource::Timer(t) => crond::commit(cx, journal, op, t)?,
            _ => unreachable!("{} claims services and timers", f.name()),
        };
        if !op.is_removal() {
            records.insert(op.key.clone(), record);
        }
    }
    f.end(cx, journal, ops)?;
    Ok(records)
}

fn staged_dir(f: &dyn Flavor, staging: &Path, op: &Op) -> PathBuf {
    staging.join(f.name()).join(op.key.to_string().replace(':', "-"))
}

/// Back up and write every part from its staged copy.
pub(crate) fn write_parts(cx: &Cx, journal: &mut Journal, parts: &[Part], staged: &Path) -> Result<()> {
    for (i, p) in parts.iter().enumerate() {
        write_part(cx, journal, p, &fs::read_to_string(staged.join(i.to_string()))?)?;
    }
    Ok(())
}

/// Back up and write parts that were never staged.
pub(crate) fn write_direct(cx: &Cx, journal: &mut Journal, parts: &[Part]) -> Result<()> {
    for p in parts {
        write_part(cx, journal, p, &p.content)?;
    }
    Ok(())
}

fn write_part(cx: &Cx, journal: &mut Journal, p: &Part, content: &str) -> Result<()> {
    let dest = cx.resolve(&p.path)?;
    journal.backup(&dest)?;
    write_file(&dest, content)?;
    if p.exec {
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

pub(crate) fn remove_parts(cx: &Cx, journal: &mut Journal, parts: &[Part]) -> Result<()> {
    for p in parts {
        let path = cx.resolve(&p.path)?;
        journal.backup(&path)?;
        remove_any(&path)?;
    }
    Ok(())
}

/// Back up `dir` as a whole and empty it, so files a new render no longer has do not linger.
pub(crate) fn reset_dir(cx: &Cx, journal: &mut Journal, dir: &str) -> Result<()> {
    let path = cx.resolve(dir)?;
    journal.backup(&path)?;
    remove_any(&path)
}

/// Point the symlink at `link` to `target`, a path as the live system sees it.
pub(crate) fn link(cx: &Cx, journal: &mut Journal, link: &str, target: &str) -> Result<()> {
    let path = cx.resolve(link)?;
    journal.backup(&path)?;
    remove_any(&path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    symlink(target, &path)?;
    Ok(())
}

pub(crate) fn unlink(cx: &Cx, journal: &mut Journal, link: &str) -> Result<()> {
    let path = cx.resolve(link)?;
    journal.backup(&path)?;
    remove_any(&path)
}

pub(crate) fn record(parts: &[Part]) -> Map<String, Value> {
    let mut rec = Map::new();
    rec.insert("files".into(), json!(parts.iter().map(|p| p.path.clone()).collect::<Vec<_>>()));
    rec
}

/// What an `after` entry means on an init without targets.
pub(crate) enum Dep<'a> {
    Network,
    Service(&'a str),
}

pub(crate) fn deps<'a>(init: Init, s: &'a Service) -> Result<Vec<Dep<'a>>> {
    let mut out = Vec::new();
    for d in &s.after {
        check_line("after", d)?;
        if d == "network" {
            out.push(Dep::Network);
        } else if d.ends_with(".target") {
            bail!("service:{} orders itself after `{d}`, a systemd target, and this host uses {init}", s.name);
        } else {
            check_name("after", d)?;
            out.push(Dep::Service(d));
        }
    }
    Ok(out)
}

pub(crate) fn describe(s: &Service) -> Result<Option<&str>> {
    if let Some(d) = &s.description {
        check_line("description", d)?;
    }
    Ok(s.description.as_deref())
}

/// Header line for generated files that take `#` comments.
pub(crate) const HEADER: &str = "# Managed by fspec. Edits are overwritten on the next apply.\n";

/// `export K=V` lines for a shell script. Values are quoted.
pub(crate) fn exports(s: &Service) -> Result<String> {
    let mut out = String::new();
    for (k, v) in &s.env {
        crate::check_env_name(&s.name, k)?;
        if v.contains(['\n', '\r']) {
            bail!("env entry `{k}` of service {} must be a single line", s.name);
        }
        out.push_str(&format!("export {k}={}\n", shell_words::quote(v)));
    }
    Ok(out)
}

/// The `run` script runit and s6 share: stderr to stdout, environment, then `exec`.
/// `pre` lines go before the command.
pub(crate) fn run_script(s: &Service, pre: &str) -> Result<String> {
    check_line("exec", s.exec())?;
    let exec = s.exec().trim();
    if exec.is_empty() {
        bail!("exec is empty");
    }
    Ok(format!("#!/bin/sh\nexec 2>&1\n{}{pre}exec {exec}\n", exports(s)?))
}

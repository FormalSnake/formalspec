use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::Os;
use fspec_core::journal::{Journal, Undo, remove_any};
use fspec_core::resource::{Pkg, PkgMode, Resource, Source};
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::{locked, pkg, text};

/// Packages from the distro's repos on a classic Fedora. The RPM the lock names is cached under
/// `/var/cache/fspec/pkgs`, so a pin stays installable after Fedora drops the version.
pub struct DnfBackend;

/// A locked RPM: where it comes from and the file name it is cached under.
struct Rpm {
    url: String,
    hash: String,
    file: String,
}

struct Want {
    names: Vec<String>,
    /// The cached RPM to install instead of the names. Only for a single-name package, since
    /// the lock holds one artifact per package.
    rpm: Option<Rpm>,
}

fn names(p: &Pkg) -> Result<Vec<String>> {
    let Source::Dnf(d) = &p.source else { bail!("pkg:{} is not a dnf package", p.name) };
    Ok(if d.names.is_empty() { vec![p.name.clone()] } else { d.names.clone() })
}

fn want(cx: &Cx, p: &Pkg) -> Result<Want> {
    let names = names(p)?;
    let artifact = locked(cx, p)?.and_then(|l| l.artifacts.get(&cx.facts.platform()));
    let rpm = match (p.mode, artifact, names.len()) {
        (PkgMode::Pinned, Some(a), 1) => match (&a.url, &a.hash) {
            (Some(url), Some(hash)) => {
                let file = url.split(['?', '#']).next().and_then(|u| u.rsplit('/').next()).unwrap_or_default();
                if file.is_empty() || file.starts_with('.') {
                    bail!("pkg:{}: cannot derive an RPM file name from {url}", p.name);
                }
                Some(Rpm { url: url.clone(), hash: hash.clone(), file: file.to_string() })
            }
            _ => None,
        },
        _ => None,
    };
    Ok(Want { names, rpm })
}

fn cache_dir(cx: &Cx) -> PathBuf {
    cx.paths.system("/var/cache/fspec/pkgs")
}

fn dnf(verb: &str) -> Cmd {
    Cmd::new("dnf").args([verb, "-y"])
}

fn install_cmd(targets: impl IntoIterator<Item = String>) -> Cmd {
    dnf("install").args(targets)
}

fn remove_cmd(names: &[String]) -> Cmd {
    dnf("remove").args(names)
}

fn sri(bytes: &[u8]) -> String {
    format!("sha256-{}", STANDARD.encode(Sha256::digest(bytes)))
}

fn version(cx: &Cx, name: &str) -> Result<fspec_core::runner::Output> {
    cx.runner.run(&Cmd::new("rpm").args(["-q", "--qf", "%{VERSION}-%{RELEASE}"]).arg(name))
}

fn record(cx: &Cx, w: &Want) -> Result<Map<String, Value>> {
    let mut versions = Map::new();
    for n in &w.names {
        let out = version(cx, n)?;
        let v = out.stdout.trim();
        if out.status == 0 && !v.is_empty() {
            versions.insert(n.clone(), v.into());
        }
    }
    let mut rec = Map::new();
    rec.insert("names".into(), w.names.clone().into());
    if !versions.is_empty() {
        rec.insert("versions".into(), versions.into());
    }
    if let Some(r) = &w.rpm {
        rec.insert("rpm".into(), r.file.clone().into());
    }
    Ok(rec)
}

/// The RPM the last apply installed, while it is still in the cache.
fn cached_rpm(cx: &Cx, rec: &Map<String, Value>) -> Option<PathBuf> {
    text(rec, "rpm").map(|f| cache_dir(cx).join(f)).filter(|p| p.exists())
}

fn targets(cx: &Cx, w: &Want) -> Vec<String> {
    match &w.rpm {
        Some(r) => vec![cache_dir(cx).join(&r.file).display().to_string()],
        None => w.names.clone(),
    }
}

fn install(cx: &Cx, journal: &mut Journal, w: &Want) -> Result<()> {
    journal.record(Undo::Run { cmd: remove_cmd(&w.names) })?;
    cx.runner.check(&install_cmd(targets(cx, w)))?;
    Ok(())
}

/// Installing over an existing package can only be reversed when the previous RPM is cached.
fn upgrade(cx: &Cx, journal: &mut Journal, w: &Want, rec: &Map<String, Value>) -> Result<()> {
    if let Some(prev) = cached_rpm(cx, rec) {
        journal.record(Undo::Run { cmd: install_cmd([prev.display().to_string()]) })?;
    }
    cx.runner.check(&install_cmd(targets(cx, w)))?;
    Ok(())
}

fn remove(cx: &Cx, journal: &mut Journal, names: &[String], rec: &Map<String, Value>) -> Result<()> {
    let back = match cached_rpm(cx, rec) {
        Some(path) => install_cmd([path.display().to_string()]),
        None => install_cmd(names.iter().cloned()),
    };
    journal.record(Undo::Run { cmd: back })?;
    cx.runner.check(&remove_cmd(names))?;
    Ok(())
}

fn recorded_names(e: &Entry) -> Result<Vec<String>> {
    match e.record.get("names").and_then(Value::as_array) {
        Some(list) => Ok(list.iter().filter_map(Value::as_str).map(str::to_string).collect()),
        None => names(pkg(&e.resource)?),
    }
}

impl Backend for DnfBackend {
    fn name(&self) -> &'static str {
        "dnf"
    }

    /// On an image host dnf packages belong to `image(packages = ...)`.
    fn handles(&self, cx: &Cx, r: &Resource) -> bool {
        cx.facts.os == Os::Linux
            && cx.facts.distro == "fedora"
            && !cx.facts.image
            && matches!(r, Resource::Pkg(Pkg { source: Source::Dnf(_), .. }))
    }

    fn phase(&self) -> Phase {
        Phase::Packages
    }

    fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        want(cx, pkg(r)?).map(drop)
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        let p = pkg(&entry.resource)?;
        let mut notes = Vec::new();
        let versions = entry.record.get("versions").and_then(Value::as_object);
        for n in recorded_names(entry)? {
            let out = version(cx, &n)?;
            let live = out.stdout.trim();
            if out.status != 0 {
                notes.push(format!("{n} is not installed"));
                continue;
            }
            match versions.and_then(|v| v.get(&n)).and_then(Value::as_str) {
                Some(rec) if !live.is_empty() && live != rec => notes.push(format!("{n} is {live}, applied {rec}")),
                _ => {}
            }
        }
        if let Some(r) = want(cx, p)?.rpm
            && text(&entry.record, "rpm").as_deref() != Some(&r.file)
        {
            notes.push(format!("lock moved to {}", r.file));
        }
        Ok(notes)
    }

    fn prepare(&self, cx: &Cx, ops: &[Op], _staging: &Path) -> Result<()> {
        for op in ops {
            let Some(d) = &op.desired else { continue };
            let Some(r) = want(cx, pkg(&d.resource)?)?.rpm else { continue };
            let dest = cache_dir(cx).join(&r.file);
            if fs::read(&dest).is_ok_and(|b| sri(&b) == r.hash) {
                continue;
            }
            let bytes = cx.fetch.get(&r.url)?;
            let got = sri(&bytes);
            if got != r.hash {
                bail!("{}: downloaded {} hashes to {got}, the lock says {}", d.resource.key(), r.url, r.hash);
            }
            fs::create_dir_all(cache_dir(cx)).with_context(|| format!("creating {}", cache_dir(cx).display()))?;
            let tmp = dest.with_extension("rpm.tmp");
            fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
            if let Err(e) = fs::rename(&tmp, &dest) {
                remove_any(&tmp)?;
                return Err(e).with_context(|| format!("caching {}", dest.display()));
            }
        }
        Ok(())
    }

    fn commit(&self, cx: &Cx, ops: &[Op], _staging: &Path, journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        for op in ops {
            let had = match &op.current {
                Some(e) => Some((recorded_names(e)?, e.record.clone())),
                None => None,
            };
            let Some(d) = &op.desired else {
                if let Some((n, rec)) = had {
                    remove(cx, journal, &n, &rec)?;
                }
                continue;
            };
            let w = want(cx, pkg(&d.resource)?)?;
            match had {
                None => install(cx, journal, &w)?,
                Some((n, rec)) if n == w.names => upgrade(cx, journal, &w, &rec)?,
                Some((n, rec)) => {
                    remove(cx, journal, &n, &rec)?;
                    install(cx, journal, &w)?;
                }
            }
            records.insert(op.key.clone(), record(cx, &w)?);
        }
        Ok(records)
    }
}

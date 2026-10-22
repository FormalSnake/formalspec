use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::Os;
use fspec_core::journal::Journal;
use fspec_core::resource::{Distro, Repo, Resource};
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use serde_json::Value;

use crate::family::Family;
use crate::pkgs::{drift_names, install, record_names, recorded_names, remove};
use crate::{text, zypper};

/// The `packages` list of `fedora(...)`, `arch(...)`, `tumbleweed(...)` and the other classic
/// distros, installed with that distro's package manager, and the `repo(...)` resources it
/// installs from. A name dropped from the list is removed. On Tumbleweed the distro repos
/// point at the snapshot `fspec.lock` pins.
pub struct DistroBackend;

fn distro(r: &Resource) -> Result<&Distro> {
    match r {
        Resource::Distro(d) => Ok(d),
        other => bail!("{} is not a distro", other.key()),
    }
}

fn family(cx: &Cx) -> Result<Family> {
    Family::of_distro(&cx.facts.distro)
        .with_context(|| format!("{} has no package manager FormalSpec drives", cx.facts.distro))
}

fn recorded(e: &Entry) -> Vec<String> {
    recorded_names(&e.record).unwrap_or_else(|| distro(&e.resource).map(|d| d.packages.clone()).unwrap_or_default())
}

fn snapshot_path(cx: &Cx) -> String {
    cx.paths.system(format!("/.snapshots/fspec-{}", cx.generation)).display().to_string()
}

/// The record key for the Tumbleweed snapshot the last apply moved the repos to. `snapshot`
/// already names the btrfs one.
const REPO_SNAPSHOT: &str = "repo_snapshot";

fn repo(op: &Op) -> Option<&Repo> {
    match op.resource() {
        Resource::Repo(r) => Some(r),
        _ => None,
    }
}

impl DistroBackend {
    /// Returns why the repo was deferred, when it was.
    fn commit_repo(&self, cx: &Cx, op: &Op, journal: &mut Journal) -> Result<Option<String>> {
        match (&op.desired, &op.current) {
            (None, Some(e)) => {
                let Resource::Repo(r) = &e.resource else { return Ok(None) };
                zypper::remove_repo(cx, journal, r).map(|()| None)
            }
            (Some(d), current) => {
                let Resource::Repo(r) = &d.resource else { return Ok(None) };
                if let Some(Entry { resource: Resource::Repo(old), .. }) = current
                    && old.name != r.name
                {
                    zypper::remove_repo(cx, journal, old)?;
                }
                zypper::add_repo(cx, journal, r)
            }
            (None, None) => Ok(None),
        }
    }
}

impl Backend for DistroBackend {
    fn name(&self) -> &'static str {
        "distro"
    }

    /// Image hosts take their base packages and repos through `image(...)`.
    fn handles(&self, cx: &Cx, r: &Resource) -> bool {
        cx.facts.os == Os::Linux && !cx.facts.image && matches!(r, Resource::Distro(_) | Resource::Repo(_))
    }

    /// Before `pkg` packages, which may come from the repos and need the snapshot in place.
    fn phase(&self) -> Phase {
        Phase::Distro
    }

    fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        let fam = family(cx)?;
        match r {
            Resource::Repo(repo) if fam != Family::Zypper => {
                bail!("repo:{}: repo(...) is for zypper distros, {} is not one", repo.name, cx.facts.distro)
            }
            Resource::Repo(repo) => zypper::check_repo(repo),
            Resource::Distro(d) if fam != Family::Zypper && d.packages.iter().any(|n| n.starts_with('!')) => {
                bail!("distro: `!name` packages are zypper syntax, {} is not a zypper distro", cx.facts.distro)
            }
            _ if fam == Family::Zypper => zypper::pin(cx).map(drop),
            _ => Ok(()),
        }
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        if let Resource::Repo(r) = &entry.resource {
            return Ok(zypper::repo_drift(cx, r));
        }
        let fam = family(cx)?;
        let mut notes = Vec::new();
        if fam == Family::Zypper {
            notes.extend(zypper::snapshot_drift(cx, zypper::pin(cx)?));
        }
        notes.extend(drift_names(cx, fam, &recorded(entry), &entry.record)?);
        Ok(notes)
    }

    /// Takes the btrfs snapshot here because every backend prepares before any commits, so the
    /// snapshot holds the system from before the `pkg` packages and the new kernel. Repo keys
    /// are fetched and checked here too, before anything changes.
    fn prepare(&self, cx: &Cx, ops: &[Op], _staging: &Path) -> Result<()> {
        for op in ops.iter().filter(|o| o.desired.is_some()) {
            if let Some(r) = repo(op) {
                zypper::fetch_key(cx, r)?;
            }
        }
        if ops.iter().all(Op::is_removal) {
            return Ok(());
        }
        let fstype = cx
            .runner
            .run(&Cmd::new("findmnt").args(["-no", "FSTYPE"]).arg(cx.paths.system("/").display().to_string()))?;
        if fstype.stdout.trim() != "btrfs" {
            return Ok(());
        }
        let dest = snapshot_path(cx);
        if Path::new(&dest).exists() {
            return Ok(());
        }
        if let Some(dir) = Path::new(&dest).parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let root = cx.paths.system("/").display().to_string();
        cx.runner.check(&Cmd::new("btrfs").args(["subvolume", "snapshot", "-r"]).arg(root).arg(&dest))?;
        Ok(())
    }

    /// Repos first, then the snapshot, then the base packages, which may come from either.
    fn commit(&self, cx: &Cx, ops: &[Op], _staging: &Path, journal: &mut Journal) -> Result<Records> {
        let fam = family(cx)?;
        let mut records = Records::new();
        for op in ops.iter().filter(|o| repo(o).is_some()) {
            if let Some(why) = self.commit_repo(cx, op, journal)? {
                records.insert(op.key.clone(), fspec_core::apply::deferred(why));
            }
        }
        for op in ops.iter().filter(|o| repo(o).is_none()) {
            let Some(d) = &op.desired else { continue };
            let want = &distro(&d.resource)?.packages;
            let had = op.current.as_ref().map(recorded).unwrap_or_default();
            let rec = op.record();
            let pinned = match fam {
                Family::Zypper => Some(zypper::pin(cx)?),
                _ => None,
            };
            if let Some(s) = pinned
                && (text(&rec, REPO_SNAPSHOT).as_deref() != Some(s) || !zypper::snapshot_drift(cx, s).is_empty())
            {
                zypper::move_to(cx, journal, s)?;
            }
            let dropped: Vec<String> =
                had.iter().filter(|n| !want.contains(n) && !n.starts_with('!')).cloned().collect();
            let added: Vec<String> = want.iter().filter(|n| !had.contains(n)).cloned().collect();
            if !dropped.is_empty() {
                remove(cx, journal, fam, &dropped, &rec)?;
            }
            if !added.is_empty() {
                install(cx, journal, fam, &added, None)?;
            }
            let mut next = record_names(cx, fam, want)?;
            let snap = snapshot_path(cx);
            if Path::new(&snap).exists() {
                next.insert("snapshot".into(), Value::from(snap));
            }
            if let Some(s) = pinned {
                next.insert(REPO_SNAPSHOT.into(), Value::from(s));
            }
            records.insert(op.key.clone(), next);
        }
        for op in ops.iter().filter(|o| o.desired.is_some() && repo(o).is_some()) {
            records.entry(op.key.clone()).or_default();
        }
        Ok(records)
    }
}

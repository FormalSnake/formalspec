use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use fspec_core::apply::{Backend, Cx, Op, Phase, Records};
use fspec_core::facts::Os;
use fspec_core::journal::Journal;
use fspec_core::resource::{Distro, Resource};
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use serde_json::Value;

use crate::family::Family;
use crate::pkgs::{drift_names, install, record_names, recorded_names, remove};

/// The `packages` list of `fedora(...)`, `arch(...)`, `debian(...)` and `void(...)` on a classic
/// distro, installed with that distro's package manager. A name dropped from the list is removed.
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

impl Backend for DistroBackend {
    fn name(&self) -> &'static str {
        "distro"
    }

    /// Image hosts take their base packages through `image(...)`.
    fn handles(&self, cx: &Cx, r: &Resource) -> bool {
        cx.facts.os == Os::Linux && !cx.facts.image && matches!(r, Resource::Distro(_))
    }

    /// The packages are live once installed, so services and files can use what they ship.
    fn phase(&self) -> Phase {
        Phase::Packages
    }

    fn check(&self, cx: &Cx, _r: &Resource) -> Result<()> {
        family(cx).map(drop)
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        drift_names(cx, family(cx)?, &recorded(entry), &entry.record)
    }

    /// Takes the btrfs snapshot here because every backend prepares before any commits, so the
    /// snapshot holds the system from before the `pkg` packages and the new kernel.
    fn prepare(&self, cx: &Cx, ops: &[Op], _staging: &Path) -> Result<()> {
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

    fn commit(&self, cx: &Cx, ops: &[Op], _staging: &Path, journal: &mut Journal) -> Result<Records> {
        let fam = family(cx)?;
        let mut records = Records::new();
        for op in ops {
            let Some(d) = &op.desired else { continue };
            let want = &distro(&d.resource)?.packages;
            let had = op.current.as_ref().map(recorded).unwrap_or_default();
            let rec = op.record();
            let dropped: Vec<String> = had.iter().filter(|n| !want.contains(n)).cloned().collect();
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
            records.insert(op.key.clone(), next);
        }
        Ok(records)
    }
}

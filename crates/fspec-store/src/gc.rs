//! `fspec gc`: delete what no kept generation needs any more. A generation is kept when it is
//! the current one or newer than the cutoff; its snapshot, store paths and trash go with it
//! when it is not.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use fspec_core::apply::state_dir;
use fspec_core::journal::remove_any;
use fspec_core::{Paths, Scope, State};

use crate::Installed;
use crate::generation::current_dir;

#[derive(Debug, Default)]
pub struct Report {
    /// One line per deleted path with its size.
    pub removed: Vec<String>,
    pub freed: u64,
}

/// `14d`, `12h`, `30m`, `90s`, `2w`.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let Ok(n) = n.parse::<u64>() else { bail!("`{s}` is not <number><unit>, e.g. 14d or 12h") };
    let unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => bail!("`{s}` needs a unit of s, m, h, d or w"),
    };
    Ok(Duration::from_secs(n * unit))
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::symlink_metadata(path).and_then(|m| m.modified()).ok()
}

/// Bytes under `path`, symlinks not followed.
pub fn size_of(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(path).into_iter().flatten().flatten().map(|e| size_of(&e.path())).sum::<u64>() + meta.len()
}

fn remove(report: &mut Report, paths: &Paths, path: &Path) -> Result<()> {
    let size = size_of(path);
    remove_any(path)?;
    report.removed.push(format!("{} ({})", paths.to_spec(path), human(size)));
    report.freed += size;
    Ok(())
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

/// Delete everything in `scope` that is not kept and was last touched at or before `cutoff`.
pub fn run(paths: &Paths, scope: Scope, cutoff: SystemTime) -> Result<Report> {
    let dir = state_dir(paths, scope);
    let state = State::load(&dir.join("state.json"))?;
    let mut report = Report::default();

    let snapshots = dir.join("generations");
    let mut kept: BTreeSet<u64> = BTreeSet::from([state.generation]);
    let mut states = vec![state.clone()];
    let mut stale = Vec::new();
    for entry in fs::read_dir(&snapshots).into_iter().flatten().flatten() {
        let path = entry.path();
        let Some(n) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u64>().ok()) else { continue };
        if n == state.generation || mtime(&path).is_none_or(|t| t > cutoff) {
            kept.insert(n);
            states.push(State::load(&path).with_context(|| format!("reading {}", path.display()))?);
        } else {
            stale.push(path);
        }
    }
    let old = |path: &Path| mtime(path).is_some_and(|t| t <= cutoff);
    stale.sort();
    for path in &stale {
        remove(&mut report, paths, path)?;
    }

    for (root, only_system_prefixed) in trash_roots(paths, scope) {
        for entry in fs::read_dir(&root).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if only_system_prefixed != name.starts_with("system-") {
                continue;
            }
            let number = name.strip_prefix("system-").unwrap_or(&name).parse::<u64>().ok();
            if !number.is_some_and(|n| kept.contains(&n)) && old(&entry.path()) {
                remove(&mut report, paths, &entry.path())?;
            }
        }
    }

    if scope == Scope::System {
        store(paths, &kept, &states, &old, &mut report)?;
    }
    report.removed.sort();
    Ok(report)
}

/// The trash directories a scope owns: `(root, holds system- prefixed names)`. The user trash
/// has two kinds, the user scope's own and those the system scope moved into the home.
fn trash_roots(paths: &Paths, scope: Scope) -> Vec<(PathBuf, bool)> {
    let system = state_dir(paths, Scope::System).join("trash");
    let user = state_dir(paths, Scope::User).join("trash");
    match scope {
        Scope::System => vec![(system, false), (user, true)],
        Scope::User => vec![(user, false)],
    }
}

fn store(
    paths: &Paths,
    kept: &BTreeSet<u64>,
    states: &[State],
    old: &dyn Fn(&Path) -> bool,
    report: &mut Report,
) -> Result<()> {
    let root = paths.fspec_root();
    let mut used: BTreeSet<String> = BTreeSet::new();
    for entry in states.iter().flat_map(|s| s.entries.values()) {
        if let Some(inst) = Installed::from_record(&entry.record)
            && let Some(name) = inst.store.strip_prefix("store/")
        {
            used.insert(name.to_string());
        }
    }
    // Whatever `current` links to is live even if no snapshot says so.
    let current = current_dir(&root);
    if let Some(cur) = &current {
        for link in fs::read_dir(cur.join("bin")).into_iter().flatten().flatten() {
            let Ok(target) = fs::read_link(link.path()) else { continue };
            let mut parts = target.components().map(|c| c.as_os_str().to_string_lossy().into_owned());
            if parts.any(|p| p == "store")
                && let Some(name) = parts.next()
            {
                used.insert(name);
            }
        }
    }

    for entry in fs::read_dir(root.join("store")).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let owner = name.strip_suffix(".json").unwrap_or(&name);
        if !used.contains(owner) && old(&entry.path()) {
            remove(report, paths, &entry.path())?;
        }
    }
    for entry in fs::read_dir(root.join("generations")).into_iter().flatten().flatten() {
        let path = entry.path();
        let is_current = current.as_deref() == Some(path.as_path());
        let number = entry.file_name().to_string_lossy().parse::<u64>().ok();
        if !is_current && !number.is_some_and(|n| kept.contains(&n)) && old(&path) {
            remove(report, paths, &path)?;
        }
    }
    Ok(())
}

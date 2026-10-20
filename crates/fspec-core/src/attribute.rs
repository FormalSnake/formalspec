//! Which app data belongs to which package. During a user-scope apply, entries that appeared
//! under the XDG directories since the last one and are named like an installed package become
//! that package's data. What nothing claims is an orphan.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::apply::Cx;
use crate::paths::Paths;
use crate::resource::{Desired, Pkg, Resource, Source};
use crate::state::State;
use crate::trash::known_data;

const XDG_DIRS: [&str; 4] = [".config", ".local/share", ".local/state", ".cache"];

/// fspec's own state sits under `~/.local/state`.
const OWN: &str = "fspec";

/// Every direct child of the XDG directories, as spec paths.
pub fn listing(paths: &Paths) -> BTreeSet<String> {
    let mut all = BTreeSet::new();
    for dir in XDG_DIRS {
        let Ok(read) = fs::read_dir(paths.home.join(dir)) else { continue };
        for entry in read.flatten() {
            if entry.file_name() != OWN {
                all.insert(format!("~/{dir}/{}", entry.file_name().to_string_lossy()));
            }
        }
    }
    all
}

/// The names an entry may carry to belong to `pkg`, lowercase: the package name, a Flatpak id
/// and its last segment, an npm scope and the name after it.
fn names(pkg: &Pkg) -> Vec<String> {
    let mut names = vec![pkg.name.clone()];
    match &pkg.source {
        Source::Flatpak { id, .. } => {
            let id = id.clone().unwrap_or_else(|| pkg.name.clone());
            names.extend(id.rsplit('.').next().map(Into::into));
            names.push(id);
        }
        Source::Npm { name } => {
            let bare = name.trim_start_matches('@');
            names.extend(bare.split('/').map(Into::into));
        }
        _ => {}
    }
    names.iter().map(|n| n.to_lowercase()).collect()
}

fn leaf(spec: &str) -> String {
    spec.rsplit('/').next().unwrap_or(spec).to_lowercase()
}

/// Attribute new entries to packages and drop claims that no longer hold. Returns whether the
/// state changed. The first call only records the baseline.
pub fn refresh(cx: &Cx, state: &mut State, desired: &[Desired]) -> Result<bool> {
    let pkgs: Vec<&Pkg> = desired
        .iter()
        .filter_map(|d| match &d.resource {
            Resource::Pkg(p) => Some(p),
            _ => None,
        })
        .collect();
    let now = listing(cx.paths);
    let before = state.clone();

    if let Some(seen) = &before.seen {
        for entry in now.difference(seen) {
            let name = leaf(entry);
            if let Some(p) = pkgs.iter().find(|p| names(p).contains(&name)) {
                let claimed = state.data.entry(p.name.clone()).or_default();
                if !claimed.contains(entry) {
                    claimed.push(entry.clone());
                }
            }
        }
    }
    state.data.retain(|pkg, paths| {
        paths.retain(|p| cx.resolve(p).is_ok_and(|abs| fs::symlink_metadata(abs).is_ok()));
        pkgs.iter().any(|p| &p.name == pkg) && !paths.is_empty()
    });
    state.seen = Some(now);
    Ok(*state != before)
}

/// Entries under the XDG directories that no package claims and no managed file covers.
pub fn orphans(paths: &Paths, system: &State, user: &State) -> Vec<String> {
    let mut claimed: Vec<PathBuf> = Vec::new();
    let mut add = |spec: &str| {
        if let Ok(p) = paths.resolve(spec) {
            claimed.push(p);
        }
    };
    for state in [system, user] {
        for entry in state.entries.values() {
            match &entry.resource {
                Resource::Pkg(p) => {
                    p.data.iter().for_each(|d| add(d));
                    known_data(p).iter().for_each(|d| add(d));
                }
                Resource::File(f) => add(&f.path),
                Resource::Secret(s) => add(&s.path),
                _ => {}
            }
        }
    }
    user.data.values().flatten().for_each(|d| add(d));

    listing(paths)
        .into_iter()
        .filter(|spec| {
            let Ok(abs) = paths.resolve(spec) else { return false };
            !claimed.iter().any(|c| c.starts_with(&abs) || abs.starts_with(c))
        })
        .collect()
}

/// Record `path` as `pkg`'s data in the user state. It must be a direct child of an XDG directory.
pub fn claim(paths: &Paths, user: &mut State, path: &str, pkg: &str) -> Result<String> {
    let spec = orphan_spec(paths, path)?;
    let claimed = user.data.entry(pkg.into()).or_default();
    if !claimed.contains(&spec) {
        claimed.push(spec.clone());
    }
    Ok(spec)
}

/// The spec form of `path` when it is a direct child of an XDG directory.
pub fn orphan_spec(paths: &Paths, path: &str) -> Result<String> {
    let abs = paths.resolve(path)?;
    let spec = paths.to_spec(&abs);
    let inside = XDG_DIRS.iter().any(|d| spec.strip_prefix(&format!("~/{d}/")).is_some_and(|rest| !rest.contains('/')));
    if !inside {
        bail!("{path} is not a direct child of ~/.config, ~/.local/share, ~/.local/state or ~/.cache");
    }
    if fs::symlink_metadata(&abs).is_err() {
        bail!("{} does not exist", abs.display());
    }
    Ok(spec)
}

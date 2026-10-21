//! `fspec gc` for project environments: link directories not used within the cutoff, or whose
//! project is gone, and the user-store packages no remaining one points into.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use fspec_core::journal::remove_any;

use crate::{FILE, cache_dir, user_store};

/// The paths deleted.
pub fn run(home: &Path, cutoff: SystemTime) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    let mut used = BTreeSet::new();
    let store = user_store(home);
    for e in fs::read_dir(cache_dir(home).join("farms")).into_iter().flatten().flatten() {
        let farm = e.path();
        let project = fs::read_to_string(farm.join("project")).unwrap_or_default();
        let recent = fs::metadata(farm.join("project")).and_then(|m| m.modified()).is_ok_and(|t| t > cutoff);
        if recent && Path::new(project.trim()).join(FILE).is_file() {
            // Packages without commands (fonts) are only named in `trees`.
            let trees = fs::read_to_string(farm.join("trees")).unwrap_or_default();
            let links = fs::read_dir(farm.join("bin"))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|l| fs::read_link(l.path()).ok());
            for target in links.chain(trees.lines().map(PathBuf::from)) {
                if let Ok(rest) = target.strip_prefix(&store)
                    && let Some(top) = rest.components().next()
                {
                    used.insert(top.as_os_str().to_owned());
                }
            }
        } else {
            remove_any(&farm)?;
            removed.push(farm);
        }
    }
    for e in fs::read_dir(&store).into_iter().flatten().flatten() {
        let name = e.file_name();
        if !used.contains(&name) && name != ".partial" {
            remove_any(&e.path())?;
            removed.push(e.path());
        }
    }
    Ok(removed)
}

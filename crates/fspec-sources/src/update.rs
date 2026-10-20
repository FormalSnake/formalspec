//! `fspec update`: resolve every declared package and rewrite the lock.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use fspec_core::lock::{Lock, LockedPkg};
use fspec_core::resource::{Channel, Host, Pkg, Resource};

use crate::{Cx, parse_age, resolve};

pub struct Update {
    pub lock: Lock,
    /// One line per change, e.g. `ripgrep 14.1.0 -> 15.1.0`.
    pub changes: Vec<String>,
    /// Packages that failed to resolve; their old entries stay in `lock`.
    pub errors: Vec<String>,
}

struct Decl {
    pkg: Pkg,
    platforms: BTreeSet<String>,
    age: Option<i64>,
}

/// Re-resolve `only` (every package when empty), keep the other entries, and drop entries no
/// host declares any more.
pub fn update(hosts: &[Host], old: &Lock, only: &[String], cx: &Cx) -> Result<Update> {
    let mut decls: BTreeMap<String, Decl> = BTreeMap::new();
    let mut errors = Vec::new();
    for host in hosts {
        let age = host
            .release_age
            .as_deref()
            .map(parse_age)
            .transpose()
            .map_err(|e| anyhow::anyhow!("host {}: {e}", host.name))?;
        for r in &host.resources {
            let Resource::Pkg(p) = r else { continue };
            let d = decls.entry(p.name.clone()).or_insert_with(|| Decl {
                pkg: p.clone(),
                platforms: BTreeSet::new(),
                age: None,
            });
            if d.pkg.source != p.source {
                errors.push(format!(
                    "{}: host {} declares {} but another host declares {}",
                    p.name, host.name, p.source, d.pkg.source
                ));
            }
            if p.channel == Channel::Bleeding {
                d.pkg.channel = Channel::Bleeding;
            }
            d.platforms.insert(host.platform.clone());
            d.age = d.age.max(age);
        }
    }
    for name in only {
        if !decls.contains_key(name) {
            bail!("no host declares a package named `{name}`");
        }
    }

    let mut lock = Lock { pkg: vec![], system: old.system.clone() };
    let mut changes = Vec::new();
    for gone in old.pkg.iter().filter(|p| !decls.contains_key(&p.name)) {
        changes.push(format!("- {} {}", gone.name, label(gone)));
    }
    for (name, d) in &decls {
        let before = old.pkg(name);
        if !only.is_empty() && !only.contains(name) {
            if let Some(b) = before {
                lock.upsert(b.clone());
            }
            continue;
        }
        let platforms: Vec<String> = d.platforms.iter().cloned().collect();
        match resolve(&d.pkg, &platforms, d.age, cx) {
            Ok(mut new) => {
                keep_cache(before, &mut new);
                if let Some(line) = describe(before, &new) {
                    changes.push(line);
                }
                lock.upsert(new);
            }
            Err(e) => {
                errors.push(format!("{e:#}"));
                if let Some(b) = before {
                    lock.upsert(b.clone());
                }
            }
        }
    }
    Ok(Update { lock, changes, errors })
}

/// CI-built artifacts stay valid while the commit and the patches are the same, so a plain
/// `fspec update` does not throw away what `fspec update --cache` recorded.
fn keep_cache(before: Option<&LockedPkg>, new: &mut LockedPkg) {
    let Some(old) = before else { return };
    if old.rev.is_some() && old.rev == new.rev && old.patches == new.patches && old.source == new.source {
        for (platform, artifact) in &old.artifacts {
            if artifact.oci.is_some() {
                new.artifacts.entry(platform.clone()).or_insert_with(|| artifact.clone());
            }
        }
    }
}

fn label(p: &LockedPkg) -> String {
    let short = |s: &str| s.chars().take(12).collect::<String>();
    p.version
        .clone()
        .or_else(|| p.rev.as_deref().map(short))
        .or_else(|| p.artifacts.values().find_map(|a| a.commit.as_deref().map(short)))
        .unwrap_or_else(|| "(unpinned)".into())
}

fn describe(before: Option<&LockedPkg>, new: &LockedPkg) -> Option<String> {
    let Some(b) = before else { return Some(format!("+ {} {}", new.name, label(new))) };
    if b == new {
        return None;
    }
    if b.source != new.source {
        return Some(format!("{} {} -> {}", new.name, b.source, new.source));
    }
    if label(b) != label(new) {
        return Some(format!("{} {} -> {}", new.name, label(b), label(new)));
    }
    Some(format!("{} {} (artifacts changed)", new.name, label(new)))
}

//! Packages for a project shell (`dev.star`): unpacked into a store, linked into no generation.
//! The system store is reused when it already holds the exact tree; anything else goes into the
//! user's own store, so a project never needs root.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fspec_core::Cx;
use fspec_core::journal::remove_any;
use fspec_core::resource::Pkg;

use crate::{Kind, bins_of, nix_bins, nix_build, target, unpack_into};

/// A package ready to run: its tree and its executables, by command name, relative to it.
#[derive(Debug, Clone)]
pub struct Unpacked {
    pub dir: PathBuf,
    pub bins: BTreeMap<String, String>,
}

/// Make the locked version of `p` available, looking in `shared` (the system store) first and
/// unpacking into `own` otherwise.
pub fn unpack_for_project(cx: &Cx, p: &Pkg, shared: &Path, own: &Path) -> Result<Unpacked> {
    let t = target(cx, p)?;
    let (dir, artifact) = match t.kind {
        Kind::Nix { attr } => {
            let out = PathBuf::from(nix_build(cx, t.locked, attr).with_context(|| format!("building {}", p.name))?);
            let bins = nix_bins(&out)?;
            return Ok(Unpacked { dir: out, bins });
        }
        Kind::Archive { origin, dirname, artifact } => {
            let reuse = shared.join(&dirname);
            if reuse.is_dir() {
                (reuse, artifact)
            } else {
                let dir = own.join(&dirname);
                if !dir.is_dir() {
                    let work = own.join(".partial").join(&dirname);
                    if work.exists() {
                        remove_any(&work)?;
                    }
                    let tree = work.join("tree");
                    unpack_into(cx, p, &origin, &dirname, &tree, &work)
                        .with_context(|| format!("unpacking {} {}", p.name, t.version))?;
                    fs::rename(&tree, &dir).with_context(|| format!("moving {} into {}", p.name, own.display()))?;
                    remove_any(&work)?;
                }
                (dir, artifact)
            }
        }
    };
    let bins = bins_of(p, artifact, &dir).with_context(|| format!("pkg {}", p.name))?;
    Ok(Unpacked { dir, bins })
}

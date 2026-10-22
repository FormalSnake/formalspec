//! A generation is `<fspec root>/generations/<n>/bin/` full of symlinks into the store, plus
//! `pkgs/<name>` linking each package's whole tree, and `<fspec root>/current` is a symlink to
//! the active one. `current/pkgs/<name>/...` is the stable path to a library or data file that
//! other config has to name, like a PAM module.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fspec_core::journal::{Journal, Undo, remove_any};

pub struct Generation {
    root: PathBuf,
    dir: PathBuf,
    /// `current` already points here: an earlier commit of this apply switched to it.
    live: bool,
}

/// The directory `current` points at, if any.
pub fn current_dir(root: &Path) -> Option<PathBuf> {
    fs::read_link(root.join("current")).ok().map(|t| root.join(t))
}

/// The directories of a generation that hold links into the store.
const DIRS: [&str; 2] = ["bin", "pkgs"];

/// Where a link in `generations/<n>/bin/` (or `pkgs/`) points for `rel` inside `store`: relative for
/// our own store (so a rooted tree can be moved), absolute for an outside path such as `/nix/store`.
pub fn link_target(store: &str, rel: &str) -> PathBuf {
    if Path::new(store).is_absolute() {
        Path::new(store).join(rel)
    } else {
        Path::new("../../..").join(store).join(rel)
    }
}

impl Generation {
    /// Start generation `n` from the links of the one `current` points at. The commit that
    /// handles removals and the one that handles adds share it, the second reopening the first.
    pub fn open(root: &Path, n: u64, journal: &mut Journal) -> Result<Generation> {
        let dir = root.join("generations").join(n.to_string());
        let previous = current_dir(root);
        if previous.as_deref() == Some(dir.as_path()) {
            return Ok(Generation { root: root.into(), dir, live: true });
        }
        journal.backup(&dir)?;
        remove_any(&dir)?;
        for sub in DIRS {
            let to = dir.join(sub);
            fs::create_dir_all(&to).with_context(|| format!("creating {}", to.display()))?;
            let Some(Ok(entries)) = previous.as_ref().map(|p| fs::read_dir(p.join(sub))) else { continue };
            for entry in entries {
                let entry = entry?;
                symlink(fs::read_link(entry.path())?, to.join(entry.file_name()))?;
            }
        }
        Ok(Generation { root: root.into(), dir, live: false })
    }

    pub fn unlink(&self, name: &str) -> Result<()> {
        remove_any(&self.dir.join("bin").join(name))
    }

    pub fn link(&self, name: &str, target: &Path) -> Result<()> {
        self.link_in("bin", name, target)
    }

    /// `pkgs/<name>`, the package's whole tree.
    pub fn unlink_pkg(&self, name: &str) -> Result<()> {
        remove_any(&self.dir.join("pkgs").join(name))
    }

    pub fn link_pkg(&self, name: &str, target: &Path) -> Result<()> {
        self.link_in("pkgs", name, target)
    }

    fn link_in(&self, sub: &str, name: &str, target: &Path) -> Result<()> {
        let at = self.dir.join(sub).join(name);
        remove_any(&at)?;
        symlink(target, &at).with_context(|| format!("linking {}", at.display()))
    }

    /// Point `current` at this generation: a temp symlink renamed over it, so readers see the
    /// old generation or the new one, never neither.
    pub fn switch(self, journal: &mut Journal) -> Result<()> {
        if self.live {
            return Ok(());
        }
        let current = self.root.join("current");
        if fs::symlink_metadata(&current).is_ok_and(|m| !m.file_type().is_symlink()) {
            bail!("{} exists and is not a symlink", current.display());
        }
        journal.backup(&current)?;
        let tmp = self.root.join("current.new");
        remove_any(&tmp)?;
        let rel = self.dir.strip_prefix(&self.root).unwrap_or(&self.dir);
        symlink(rel, &tmp).with_context(|| format!("creating {}", tmp.display()))?;
        fs::rename(&tmp, &current).with_context(|| format!("switching {}", current.display()))
    }
}

/// Undo for a path created by this apply.
pub fn undo_create(journal: &mut Journal, path: &Path) -> Result<()> {
    journal.record(Undo::RestorePath { path: path.into(), backup: None })
}

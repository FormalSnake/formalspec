use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::runner::{Cmd, Runner};

/// How to reverse one live mutation. Written to disk before the mutation happens, so a crash
/// mid-apply can be reverted on the next run.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "undo", rename_all = "snake_case")]
pub enum Undo {
    /// Put `backup` back at `path`, or remove `path` when nothing was there before.
    RestorePath { path: PathBuf, backup: Option<PathBuf> },
    /// Reverse a package manager, init or bootc operation.
    Run { cmd: Cmd },
}

/// The undo log for one apply in one scope, at `<state dir>/journal.json`.
#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    backups: PathBuf,
    generation: u64,
    steps: Vec<Undo>,
}

#[derive(Serialize, Deserialize)]
struct OnDisk {
    generation: u64,
    steps: Vec<Undo>,
}

impl Journal {
    fn paths(state_dir: &Path) -> (PathBuf, PathBuf) {
        (state_dir.join("journal.json"), state_dir.join("journal-backups"))
    }

    /// Start a journal. Fails if a previous apply left one behind; call [`Journal::recover`] first.
    pub fn begin(state_dir: &Path, generation: u64) -> Result<Journal> {
        let (path, backups) = Journal::paths(state_dir);
        if path.exists() {
            bail!("{} exists: a previous apply did not finish", path.display());
        }
        fs::create_dir_all(&backups).with_context(|| format!("creating {}", backups.display()))?;
        let j = Journal { path, backups, generation, steps: vec![] };
        j.persist()?;
        Ok(j)
    }

    /// Revert whatever an interrupted apply left behind. Returns whether there was anything.
    pub fn recover(state_dir: &Path, runner: &dyn Runner) -> Result<bool> {
        let (path, backups) = Journal::paths(state_dir);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let on_disk: OnDisk = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Journal { path, backups, generation: on_disk.generation, steps: on_disk.steps }.rollback(runner)?;
        Ok(true)
    }

    fn persist(&self) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let on_disk = OnDisk { generation: self.generation, steps: self.steps.clone() };
        fs::write(&tmp, serde_json::to_vec_pretty(&on_disk)?)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Record an undo step. Call before making the change it reverses.
    pub fn record(&mut self, undo: Undo) -> Result<()> {
        self.steps.push(undo);
        self.persist()
    }

    /// Save whatever is at `path` (file, symlink or directory) so a rollback restores it, and
    /// record the step. Call before writing, replacing or removing `path`.
    pub fn backup(&mut self, path: &Path) -> Result<()> {
        let backup = match fs::symlink_metadata(path) {
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
            Ok(_) => {
                let dest = self.backups.join(self.steps.len().to_string());
                copy_any(path, &dest).with_context(|| format!("backing up {}", path.display()))?;
                Some(dest)
            }
        };
        self.record(Undo::RestorePath { path: path.into(), backup })
    }

    /// Replay the steps backwards. Keeps going past a failing step and reports all of them.
    pub fn rollback(self, runner: &dyn Runner) -> Result<()> {
        let mut errors = Vec::new();
        for step in self.steps.iter().rev() {
            if let Err(e) = undo(step, runner) {
                errors.push(format!("{e:#}"));
            }
        }
        self.finish()?;
        if !errors.is_empty() {
            bail!("rollback left {} step(s) undone:\n  {}", errors.len(), errors.join("\n  "));
        }
        Ok(())
    }

    /// The apply succeeded: drop the journal and its backups.
    pub fn finish(self) -> Result<()> {
        remove_any(&self.backups)?;
        remove_any(&self.path)
    }
}

fn undo(step: &Undo, runner: &dyn Runner) -> Result<()> {
    match step {
        Undo::RestorePath { path, backup } => {
            remove_any(path)?;
            if let Some(b) = backup {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                copy_any(b, path).with_context(|| format!("restoring {}", path.display()))?;
            }
            Ok(())
        }
        Undo::Run { cmd } => runner.check(cmd).map(drop),
    }
}

/// Copy a file, symlink (as a symlink) or directory tree, preserving permissions.
pub fn copy_any(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)?;
    } else if meta.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_any(&entry.path(), &to.join(entry.file_name()))?;
        }
        fs::set_permissions(to, meta.permissions())?;
    } else {
        fs::copy(from, to)?;
    }
    Ok(())
}

/// Remove a file, symlink or directory tree; a missing path is fine.
pub fn remove_any(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(m) if m.is_dir() => fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display())),
        Ok(_) => fs::remove_file(path).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RecordingRunner;

    #[test]
    fn rollback_restores_files_and_runs_commands() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let existing = dir.path().join("etc/existing");
        let created = dir.path().join("etc/created");
        fs::create_dir_all(existing.parent().unwrap()).unwrap();
        fs::write(&existing, "old").unwrap();

        let mut j = Journal::begin(&state, 1).unwrap();
        j.backup(&existing).unwrap();
        fs::write(&existing, "new").unwrap();
        j.backup(&created).unwrap();
        fs::write(&created, "new").unwrap();
        j.record(Undo::Run { cmd: Cmd::new("flatpak").args(["uninstall", "-y", "x"]) }).unwrap();

        let runner = RecordingRunner::default();
        // A crash here leaves the journal on disk; the next run recovers from it.
        drop(j);
        assert!(Journal::begin(&state, 2).is_err());
        assert!(Journal::recover(&state, &runner).unwrap());
        assert_eq!(fs::read_to_string(&existing).unwrap(), "old");
        assert!(!created.exists());
        assert_eq!(runner.lines(), ["flatpak uninstall -y x"]);
        assert!(!state.join("journal.json").exists());
        assert!(!Journal::recover(&state, &runner).unwrap());
    }
}

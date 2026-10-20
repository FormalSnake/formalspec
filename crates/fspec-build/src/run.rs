use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fspec_core::patches::patch_path;
use fspec_core::resource::Pkg;
use fspec_core::runner::{Cmd, Runner};

use crate::pack;
use crate::recipe::{self, Dirs};

/// One package built for one platform at its locked commit.
pub struct Job<'a> {
    pub pkg: &'a Pkg,
    pub url: &'a str,
    pub rev: &'a str,
    pub platform: &'a str,
    pub spec_dir: &'a Path,
    pub out: &'a Path,
}

#[derive(Debug)]
pub struct Built {
    /// `<out>/<name>-<platform>.tar.zst`
    pub archive: PathBuf,
    /// The registry tag the workflow pushes it under, `<rev>-<platform>`; also written to
    /// `<out>/tag`.
    pub tag: String,
}

/// Fetch the locked commit, apply the patches, run the recipe and pack the executables.
/// Everything external goes through `runner`.
pub fn build(runner: &dyn Runner, job: &Job) -> Result<Built> {
    let work = tempfile::tempdir().context("creating a build directory")?;
    let dirs = Dirs { src: work.path().join("src"), build: work.path().join("build"), out: work.path().join("out") };
    let src = dirs.src.display().to_string();
    for cmd in [
        Cmd::new("git").args(["init", "-q", &src]),
        Cmd::new("git").args(["-C", &src, "remote", "add", "origin", job.url]),
        Cmd::new("git").args(["-C", &src, "fetch", "-q", "--depth", "1", "origin", job.rev]),
        Cmd::new("git").args(["-C", &src, "checkout", "-q", "FETCH_HEAD"]),
    ] {
        runner.check(&cmd)?;
    }
    for patch in &job.pkg.patches {
        let file = patch_path(job.spec_dir, patch)?;
        runner
            .check(&Cmd::new("git").args(["-C", &src, "apply"]).arg(file.display().to_string()))
            .with_context(|| format!("applying {patch}"))?;
    }

    fs::create_dir_all(&dirs.out)?;
    let recipe = recipe::select(&dirs.src, &job.pkg.build)?;
    for cmd in recipe.commands(&dirs, &job.pkg.build_flags, &job.pkg.build) {
        runner.check(&cmd).with_context(|| format!("building {}", job.pkg.name))?;
    }

    let files = pack::executables(&recipe.bin_dirs(&dirs), &job.pkg.bins)?;
    let archive = job.out.join(format!("{}-{}.tar.zst", job.pkg.name, job.platform));
    fs::create_dir_all(job.out)?;
    fs::write(&archive, pack::tar_zst(&files)?).with_context(|| format!("writing {}", archive.display()))?;
    let tag = format!("{}-{}", job.rev, job.platform);
    fs::write(job.out.join("tag"), &tag)?;
    Ok(Built { archive, tag })
}

#[cfg(test)]
mod tests;

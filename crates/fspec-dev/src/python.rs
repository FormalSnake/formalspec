//! `pypi("yt-dlp")`: `fspec update` has uv compile the distribution and its dependencies into
//! hashed requirements for every platform, kept in the lock; activation installs exactly those
//! into a virtualenv over the project's `pkg("python")`, in the user's store.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use fspec_core::dev::Dev;
use fspec_core::journal::remove_any;
use fspec_core::lock::{Artifact, LockedPkg};
use fspec_core::resource::{Pkg, Source};
use fspec_core::runner::{Cmd, Runner};
use fspec_store::Unpacked;

use crate::short_hash;

/// The uv every pypi package is compiled and installed with, unless `dev.star` names its own.
pub fn implicit_uv() -> Pkg {
    serde_json::from_value(serde_json::json!({"name": "uv", "source": {"kind": "aqua", "name": "uv"}}))
        .expect("a pkg from a name and a source")
}

pub fn is_pypi(p: &Pkg) -> bool {
    matches!(p.source, Source::Pypi { .. })
}

/// The `python` and `uv` packages a project with pypi packages needs.
pub fn tools(dev: &Dev) -> Result<Option<(&Pkg, &Pkg)>> {
    if !dev.packages.iter().any(is_pypi) {
        return Ok(None);
    }
    let python = dev
        .packages
        .iter()
        .find(|p| p.source == Source::Python)
        .context("pypi(...) packages install over a pkg(\"python\", version = ...), and dev.star declares none")?;
    let uv = dev.packages.iter().find(|p| p.name == "uv").context("uv is added for pypi(...) packages")?;
    Ok(Some((python, uv)))
}

/// `uv pip compile` output: `name==version` lines, each followed by `\`-continued hash lines.
fn requirement_lines(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut joining = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (body, more) = match line.strip_suffix('\\') {
            Some(b) => (b.trim(), true),
            None => (line, false),
        };
        match out.last_mut() {
            Some(last) if joining => {
                last.push(' ');
                last.push_str(body);
            }
            _ => out.push(body.to_string()),
        }
        joining = more;
    }
    out
}

/// The version uv picked for `name`, from its own line.
fn version_of(name: &str, lines: &[String]) -> Option<String> {
    let norm = |s: &str| s.to_ascii_lowercase().replace(['_', '.'], "-");
    lines.iter().find_map(|l| {
        let (n, rest) = l.split_once("==")?;
        (norm(n.trim()) == norm(name)).then(|| rest.split([' ', ';']).next().unwrap_or("").to_string())
    })
}

/// Resolve `p` for every platform at once, for the CPython `python_version` the project runs.
pub fn compile(runner: &dyn Runner, uv: &Path, p: &Pkg, python_version: &str) -> Result<LockedPkg> {
    let Source::Pypi { name } = &p.source else { bail!("{} is not a pypi package", p.name) };
    let spec = match &p.version {
        Some(v) => format!("{name}=={v}\n"),
        None => format!("{name}\n"),
    };
    let out = runner.check(
        &Cmd::new(uv.display().to_string())
            .args(["pip", "compile", "-", "--universal", "--generate-hashes", "--no-header", "--no-annotate"])
            .args(["--quiet", "--python-version", python_version])
            .stdin(spec),
    )?;
    let requirements = requirement_lines(&out.stdout);
    let version = version_of(name, &requirements).with_context(|| format!("uv did not resolve {name}"))?;
    Ok(LockedPkg {
        name: p.name.clone(),
        source: p.source.to_string(),
        version: Some(version),
        rev: None,
        patches: None,
        parts: Default::default(),
        artifacts: BTreeMap::from([("any".to_string(), Artifact { requirements, ..Default::default() })]),
    })
}

/// The virtualenv for `locked` over `python` (a python-build-standalone tree), built once into
/// `own`. Its commands are the console scripts the distributions installed.
pub fn venv(runner: &dyn Runner, uv: &Path, python: &Path, locked: &LockedPkg, own: &Path) -> Result<Unpacked> {
    let reqs =
        &locked.artifacts.get("any").context("pypi package without requirements, run fspec update")?.requirements;
    let version = locked.version.as_deref().unwrap_or("unversioned");
    let key = short_hash(&format!("{}\n{}", python.display(), reqs.join("\n")));
    let dir = own.join(format!("{key}-{}-{version}", locked.name));
    if !dir.is_dir() {
        let work = own.join(".partial").join(dir.file_name().context("venv has a name")?);
        if work.exists() {
            remove_any(&work)?;
        }
        fs::create_dir_all(&work)?;
        let file = work.join("requirements.txt");
        fs::write(&file, reqs.join("\n") + "\n")?;
        let tree = work.join("tree");
        let interpreter = python.join("python/bin/python3");
        // Relocatable, so the scripts still run once the tree moves to its final name.
        runner.check(
            &Cmd::new(uv.display().to_string())
                .args(["venv", "--relocatable", "--no-project", "--quiet", "--python"])
                .args([interpreter.display().to_string(), tree.display().to_string()]),
        )?;
        runner
            .check(
                &Cmd::new(uv.display().to_string())
                    .args(["pip", "install", "--quiet", "--require-hashes", "--no-deps", "--python"])
                    .args([tree.join("bin/python").display().to_string(), "-r".into(), file.display().to_string()]),
            )
            .with_context(|| format!("installing {} {version}", locked.name))?;
        fs::rename(&tree, &dir)?;
        remove_any(&work)?;
    }
    let mut bins = BTreeMap::new();
    for e in fs::read_dir(dir.join("bin"))? {
        let name = e?.file_name().to_string_lossy().into_owned();
        let venv_own = ["python", "activate", "deactivate", "pip", "pydoc"].iter().any(|p| name.starts_with(p));
        if !venv_own {
            bins.insert(name.clone(), format!("bin/{name}"));
        }
    }
    Ok(Unpacked { dir, bins })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_requirements_join_their_hash_lines() {
        let out = "yt-dlp==2025.10.22 \\\n    --hash=sha256:aa \\\n    --hash=sha256:bb\ncolorama==0.4.6 ; sys_platform == 'win32' \\\n    --hash=sha256:cc\n";
        let lines = requirement_lines(out);
        assert_eq!(
            lines,
            [
                "yt-dlp==2025.10.22 --hash=sha256:aa --hash=sha256:bb",
                "colorama==0.4.6 ; sys_platform == 'win32' --hash=sha256:cc"
            ]
        );
        assert_eq!(version_of("yt_dlp", &lines).as_deref(), Some("2025.10.22"));
    }
}

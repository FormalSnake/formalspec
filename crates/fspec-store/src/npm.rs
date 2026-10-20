//! npm packages: the registry tarball is the artifact. One without runtime dependencies is
//! unpacked and its `bin` entries linked directly; one with dependencies needs `npm install`,
//! so the host must have npm on PATH.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result};
use fspec_core::journal::remove_any;
use fspec_core::runner::{Cmd, Runner};
use serde::Deserialize;

#[derive(Deserialize)]
struct Manifest {
    #[serde(default)]
    bin: Option<Bin>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Bin {
    One(String),
    Many(BTreeMap<String, String>),
}

fn manifest(path: &Path) -> Result<Manifest> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Turn the freshly unpacked tarball in `dir` into an installed tree. With runtime
/// dependencies, `npm install` replaces it with a `node_modules` tree.
pub fn finish(runner: &dyn Runner, dir: &Path, tarball: &Path, npm_name: &str) -> Result<()> {
    let m = manifest(&dir.join("package/package.json"))?;
    if !m.dependencies.is_empty() {
        remove_any(dir)?;
        fs::create_dir_all(dir)?;
        let cmd = Cmd::new("npm").args(["install", "--omit=dev", "--prefix"]).arg(dir.display().to_string());
        runner.check(&cmd.arg(tarball.display().to_string())).with_context(|| format!("installing {npm_name}"))?;
        return Ok(());
    }
    for rel in bins(dir, npm_name)?.values() {
        let path = dir.join(rel);
        if path.is_file() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
}

/// Executable name to path inside the installed tree.
pub fn bins(dir: &Path, npm_name: &str) -> Result<BTreeMap<String, String>> {
    let short = npm_name.rsplit('/').next().unwrap_or(npm_name);
    let installed = dir.join("node_modules").join(npm_name).join("package.json");
    let (m, prefix) = if installed.exists() {
        (manifest(&installed)?, None)
    } else {
        (manifest(&dir.join("package/package.json"))?, Some("package"))
    };
    let named: BTreeMap<String, String> = match m.bin {
        None => BTreeMap::new(),
        Some(Bin::One(path)) => BTreeMap::from([(short.to_string(), path)]),
        Some(Bin::Many(map)) => map,
    };
    Ok(named
        .into_iter()
        .map(|(name, path)| {
            let rel = match prefix {
                Some(p) => format!("{p}/{}", path.trim_start_matches("./")),
                None => format!("node_modules/.bin/{name}"),
            };
            (name, rel)
        })
        .collect())
}

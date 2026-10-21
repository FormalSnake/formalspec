//! A project's environment: its locked packages on PATH through a directory of links, its
//! system dependencies checked, `env` set and `shell_hook` run. Nothing is installed globally.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fspec_core::dev::Dev;
use fspec_core::facts::{Facts, Init, Os};
use fspec_core::fetch::Fetch;
use fspec_core::journal::remove_any;
use fspec_core::runner::Runner;
use fspec_core::{Cx, Lock, Paths, Scope};

use crate::container::{self, Run};
use crate::{Project, cache_dir, libs, short_hash, user_store};

/// The machine a project is activated on.
pub struct Machine<'a> {
    pub paths: &'a Paths,
    pub runner: &'a dyn Runner,
    pub fetch: &'a dyn Fetch,
    /// `aarch64-darwin`.
    pub platform: String,
    /// The package manager `lib(...)` and `bin(...)` are checked against; none on NixOS.
    pub manager: Option<&'static str>,
    /// The calling environment.
    pub base: BTreeMap<String, String>,
    /// `docker` or `podman`, when one is installed.
    pub runtime: Option<String>,
}

pub struct Activated {
    pub project: Project,
    pub dev: Dev,
    pub lock: Lock,
    /// The whole environment of the shell, or of the shell inside the container.
    pub env: BTreeMap<String, String>,
    /// Set when `dev.star` names a container.
    pub run: Option<Run>,
}

pub fn activate(project: &Project, m: &Machine) -> Result<Activated> {
    let lock_path = project.lock_path();
    if !lock_path.is_file() {
        bail!("{} has no fspec.lock beside it, run fspec update there", project.file().display());
    }
    let lock = Lock::load(&lock_path)?;
    let here = project.evaluate(&m.platform)?;
    if here.container.is_some() {
        let arch = m.platform.split('-').next().unwrap_or("x86_64");
        let platform = format!("{arch}-linux");
        let dev = if platform == m.platform { here } else { project.evaluate(&platform)? };
        let farm = link_farm(project, &dev, &lock, m, &platform)?;
        let (run, env) = container::prepare(project, &dev, &lock, &farm, m)?;
        return Ok(Activated { project: project.clone(), dev, lock, env, run: Some(run) });
    }
    let dev = here;
    let farm = link_farm(project, &dev, &lock, m, &m.platform)?;
    let mut env = m.base.clone();
    let mut path = vec![farm.join("bin").display().to_string()];
    let mut search: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for prefix in check_natives(&dev, &lock, m)? {
        path.push(prefix.join("bin").display().to_string());
        let pc = [prefix.join("lib/pkgconfig"), prefix.join("share/pkgconfig")];
        search.entry("PKG_CONFIG_PATH").or_default().extend(pc.iter().map(|p| p.display().to_string()));
        search.entry("LIBRARY_PATH").or_default().push(prefix.join("lib").display().to_string());
        search.entry("CPATH").or_default().push(prefix.join("include").display().to_string());
    }
    if let Some(old) = m.base.get("PATH") {
        path.push(old.clone());
    }
    env.insert("PATH".into(), path.join(":"));
    for (k, dirs) in search {
        let mut v = dirs.join(":");
        if let Some(old) = m.base.get(k).filter(|o| !o.is_empty()) {
            v = format!("{v}:{old}");
        }
        env.insert(k.into(), v);
    }
    env.extend(dev.env.clone());
    env.insert("FSPEC_DEV".into(), project.dir.display().to_string());
    if let Some(hook) = &dev.shell_hook {
        env = run_hook(hook, &env, &project.dir)?;
    }
    Ok(Activated { project: project.clone(), dev, lock, env, run: None })
}

/// Check every `lib(...)` and `bin(...)` is installed, and return the Homebrew prefixes of the
/// ones brew provides, which are not on the default search paths.
fn check_natives(dev: &Dev, lock: &Lock, m: &Machine) -> Result<Vec<PathBuf>> {
    let natives = libs::natives(dev);
    if natives.is_empty() {
        return Ok(vec![]);
    }
    let Some(manager) = m.manager else {
        bail!(
            "this system has no package manager fspec checks lib(...) and bin(...) against; set container = \"debian:13\" in dev.star to get them from an image"
        );
    };
    let (mut problems, mut missing, mut prefixes) = (Vec::new(), Vec::new(), Vec::new());
    let brew = libs::brew_prefix();
    for ((is_lib, name), (_, answer)) in natives.iter().zip(libs::locked_for(dev, lock, manager)) {
        match answer {
            Err(why) => problems.push(why),
            Ok(pkg) if manager == "brew" => {
                let prefix = brew.join("opt").join(&pkg);
                if prefix.is_dir() {
                    prefixes.push(prefix);
                } else {
                    missing.push(pkg);
                }
            }
            Ok(pkg) => {
                if !libs::present_on_linux(Path::new("/"), *is_lib, name) {
                    missing.push(pkg);
                }
            }
        }
    }
    prefixes.dedup();
    missing.dedup();
    if !missing.is_empty() {
        problems.push(format!("not installed, install with: {}", libs::install_hint(manager, &missing)));
    }
    if !problems.is_empty() {
        bail!("dev.star needs system packages:\n  {}", problems.join("\n  "));
    }
    Ok(prefixes)
}

/// A directory of links to every locked package's executables for `platform`, named by what it
/// holds, so it is built once and shared by every shell of the project.
pub(crate) fn link_farm(project: &Project, dev: &Dev, lock: &Lock, m: &Machine, platform: &str) -> Result<PathBuf> {
    let (arch, os) = platform.split_once('-').context("platform is <arch>-<os>")?;
    let facts = Facts {
        name: "dev".into(),
        os: if os == "darwin" { Os::Darwin } else { Os::Linux },
        arch: arch.into(),
        distro: "dev".into(),
        init: Init::Systemd,
        image: false,
    };
    let cx = Cx {
        paths: m.paths,
        facts: &facts,
        spec_dir: &project.dir,
        lock,
        runner: m.runner,
        fetch: m.fetch,
        generation: 0,
        scope: Scope::User,
        protect: &[],
    };
    let shared = m.paths.fspec_root().join("store");
    let own = user_store(&m.paths.home);
    let mut links: BTreeMap<String, (String, PathBuf)> = BTreeMap::new();
    for p in &dev.packages {
        let u = fspec_store::unpack_for_project(&cx, p, &shared, &own)?;
        for (cmd, rel) in &u.bins {
            if let Some((other, _)) = links.get(cmd) {
                bail!("pkg {other} and pkg {} both provide `{cmd}`", p.name);
            }
            links.insert(cmd.clone(), (p.name.clone(), u.dir.join(rel)));
        }
    }
    let identity: Vec<String> = links.iter().map(|(c, (_, t))| format!("{c}={}", t.display())).collect();
    let key = short_hash(&format!("{}\n{platform}\n{}", project.dir.display(), identity.join("\n")));
    let farm = cache_dir(&m.paths.home).join("farms").join(&key);
    if !farm.is_dir() {
        let tmp = farm.with_extension(format!("tmp{}", std::process::id()));
        if tmp.exists() {
            remove_any(&tmp)?;
        }
        fs::create_dir_all(tmp.join("bin"))?;
        for (cmd, (_, target)) in &links {
            symlink(target, tmp.join("bin").join(cmd))?;
        }
        fs::write(tmp.join("project"), project.dir.display().to_string())?;
        if fs::rename(&tmp, &farm).is_err() {
            remove_any(&tmp)?;
        }
    }
    // gc keeps farms that were used recently.
    fs::write(farm.join("project"), project.dir.display().to_string())?;
    Ok(farm)
}

/// Run `hook` in sh with `env` and return the environment it leaves.
fn run_hook(hook: &str, env: &BTreeMap<String, String>, dir: &Path) -> Result<BTreeMap<String, String>> {
    let script = format!("{{\n{hook}\n}} >&2 && exec env -0");
    let out = std::process::Command::new("sh")
        .args(["-c", &script])
        .env_clear()
        .envs(env)
        .current_dir(dir)
        .stderr(std::process::Stdio::inherit())
        .output()
        .context("running shell_hook")?;
    if !out.status.success() {
        bail!("shell_hook in dev.star failed ({})", out.status);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.split('\0').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect())
}

/// Shell variables a subshell changes on its own, which an export must leave alone.
const VOLATILE: [&str; 4] = ["_", "SHLVL", "PWD", "OLDPWD"];

/// bash code that turns `base` into `env`, for direnv.
pub fn export(base: &BTreeMap<String, String>, env: &BTreeMap<String, String>) -> String {
    let quote = |v: &str| format!("'{}'", v.replace('\'', r"'\''"));
    let mut out = String::new();
    for (k, v) in env {
        if !VOLATILE.contains(&k.as_str()) && base.get(k) != Some(v) {
            out.push_str(&format!("export {k}={}\n", quote(v)));
        }
    }
    for k in base.keys() {
        if !VOLATILE.contains(&k.as_str()) && !env.contains_key(k) {
            out.push_str(&format!("unset {k}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_sets_changes_and_unsets_removals() {
        let base: BTreeMap<String, String> =
            [("PATH", "/usr/bin"), ("RUSTC", "x"), ("SHLVL", "1")].map(|(k, v)| (k.into(), v.into())).into();
        let env: BTreeMap<String, String> =
            [("PATH", "/farm/bin:/usr/bin"), ("A", "it's"), ("SHLVL", "2")].map(|(k, v)| (k.into(), v.into())).into();
        assert_eq!(export(&base, &env), "export A='it'\\''s'\nexport PATH='/farm/bin:/usr/bin'\nunset RUSTC\n");
    }

    #[test]
    fn the_hook_sees_the_environment_and_its_exports_and_unsets_stick() {
        let env: BTreeMap<String, String> =
            [("A", "1"), ("RUSTC", "x"), ("PATH", "/usr/bin:/bin")].map(|(k, v)| (k.into(), v.into())).into();
        let out = run_hook("export B=\"$A-2\"\nunset RUSTC\necho noise", &env, Path::new("/")).unwrap();
        assert_eq!(out.get("B").map(String::as_str), Some("1-2"));
        assert!(!out.contains_key("RUSTC"));
        assert!(run_hook("false", &env, Path::new("/")).is_err());
    }
}

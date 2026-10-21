//! `container = "debian:13"`: the shell runs in the pinned image, with the project's `lib(...)`
//! and `bin(...)` installed into a derived image from that distro's repositories, and the locked
//! packages mounted from the store at the paths they have outside.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use fspec_core::dev::Dev;
use fspec_core::runner::Cmd;
use fspec_core::{Lock, Paths};

use crate::activate::{Farm, Machine};
use crate::{Project, cache_dir, libs, short_hash, user_store};

const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// How to start a command in the project's container.
#[derive(Debug, Clone)]
pub struct Run {
    pub runtime: String,
    /// Everything between `run` and the image.
    pub args: Vec<String>,
    pub image: String,
    pub hook: Option<String>,
}

impl Run {
    /// `cmd` in the container, or an interactive shell when it is empty.
    pub fn command(&self, cmd: &[String], tty: bool) -> Command {
        let mut c = Command::new(&self.runtime);
        c.args(["run", "--rm", "-i"]);
        if tty {
            c.arg("-t");
        }
        c.args(&self.args).arg(&self.image);
        let shell = ["sh".to_string(), "-c".into(), "command -v bash >/dev/null && exec bash || exec sh".into()];
        let cmd: Vec<String> = if cmd.is_empty() { shell.to_vec() } else { cmd.to_vec() };
        let hook = self.hook.as_deref().unwrap_or(":");
        c.args(["sh", "-c", &format!("{{\n{hook}\n}} >&2 && exec \"$@\""), "sh"]).args(cmd);
        c
    }
}

pub(crate) fn prepare(
    project: &Project,
    dev: &Dev,
    reference: &str,
    lock: &Lock,
    farm: &Farm,
    m: &Machine,
) -> Result<(Run, BTreeMap<String, String>)> {
    let pinned = lock
        .image
        .get(reference)
        .with_context(|| format!("container {reference} is not pinned in fspec.lock, run fspec update"))?;
    let manager = libs::manager_of_image(reference).with_context(|| {
        format!("cannot tell which package manager {reference} uses (debian, ubuntu, fedora, archlinux, opensuse or void images)")
    })?;
    let mut pkgs = Vec::new();
    let mut problems = Vec::new();
    for (_, answer) in libs::locked_for(dev, lock, manager) {
        match answer {
            Ok(p) => pkgs.push(p),
            Err(why) => problems.push(why),
        }
    }
    if !problems.is_empty() {
        bail!("dev.star's container needs system packages:\n  {}", problems.join("\n  "));
    }
    // Slim bases ship no CA bundle, so nothing in the shell could fetch over https. Every
    // supported distro names the package the same.
    pkgs.push("ca-certificates".into());
    pkgs.sort();
    pkgs.dedup();
    let containerfile = format!("FROM {pinned}\nRUN {}\n", libs::image_install(manager, &pkgs)?);
    let runtime = m.runtime.clone().context("container = ... needs docker or podman")?;
    let tag = format!("fspec-dev:{}", short_hash(&containerfile));
    let have = m.runner.run(&Cmd::new(runtime.as_str()).args(["image", "inspect", &tag]))?;
    if have.status != 0 {
        eprintln!("fspec: building {tag} from {reference} with {} system packages", pkgs.len());
        m.runner
            .check(&Cmd::new(runtime.as_str()).args(["build", "-t", &tag, "-"]).stdin(containerfile))
            .with_context(|| format!("building the dev image for {reference}"))?;
    }

    let home = cache_dir(&m.paths.home).join("homes").join(format!("{}-{}", project.id(), m.platform));
    fs::create_dir_all(&home)?;
    let mut env = BTreeMap::new();
    env.insert("PATH".to_string(), format!("{}:{PATH}", farm.dir.join("bin").display()));
    env.insert("HOME".into(), home.display().to_string());
    if let Some(t) = m.base.get("TERM") {
        env.insert("TERM".into(), t.clone());
    }
    env.extend(farm.pkg_env());
    env.extend(dev.env.clone());
    env.insert("FSPEC_DEV".into(), project.dir.display().to_string());

    let cwd = std::env::current_dir().ok().filter(|d| d.starts_with(&project.dir)).unwrap_or(project.dir.clone());
    let mut args = vec!["--network".to_string(), "host".into()];
    // SAFETY: getuid and getgid cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    args.extend(["--user".into(), format!("{uid}:{gid}")]);
    for (dir, rw) in mounts(project, &farm.dir, &home, m.paths) {
        args.extend(["-v".into(), format!("{0}:{0}{1}", dir.display(), if rw { "" } else { ":ro" })]);
    }
    args.extend(["-w".into(), cwd.display().to_string()]);
    for (k, v) in &env {
        args.extend(["-e".into(), format!("{k}={v}")]);
    }
    Ok((Run { runtime, args, image: tag, hook: dev.shell_hook.clone() }, env))
}

/// The project and the container's home writable; the links and the stores they point into
/// read-only, at the same paths as outside.
fn mounts(project: &Project, farm: &Path, home: &Path, paths: &Paths) -> Vec<(PathBuf, bool)> {
    let mut out = vec![(project.dir.clone(), true), (home.to_path_buf(), true), (farm.to_path_buf(), false)];
    for store in [user_store(&paths.home), paths.fspec_root().join("store")] {
        if store.is_dir() {
            out.push((store, false));
        }
    }
    out
}

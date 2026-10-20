//! The store backend: aqua, github, npm, cargo and nix packages unpack into
//! `<fspec root>/store/<hash8>-<name>-<version>/`, and each apply builds a generation of
//! symlinks into the store and switches `current` to it.

mod archive;
mod generation;
mod npm;
mod oci;
mod patch;
mod pins;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fspec_core::apply::Records;
use fspec_core::journal::{Journal, copy_any, remove_any};
use fspec_core::lock::{Artifact, LockedPkg};
use fspec_core::resource::{Pkg, PkgMode, Source};
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use fspec_core::{Backend, Cx, Lock, Op, Phase, Resource, State};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use generation::{Generation, link_target, undo_create};

pub use archive::sri_sha256;
pub use pins::{Shell, pin_dirs, shell_hook};

pub struct StoreBackend;

/// What the state file keeps per package. `locked` is the lock entry as applied, which lets a
/// rollback rebuild the lock of an old generation.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Installed {
    /// Relative to the fspec root for our own store, absolute for `/nix/store` paths.
    store: String,
    version: String,
    /// The artifact hash, or the nixpkgs rev: what changed when the lock moved.
    id: String,
    /// Executable name to path inside `store`.
    bins: BTreeMap<String, String>,
    locked: LockedPkg,
}

impl Installed {
    fn from_record(record: &Map<String, Value>) -> Option<Installed> {
        serde_json::from_value(Value::Object(record.clone())).ok()
    }

    fn to_record(&self) -> Map<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        }
    }

    fn exists(&self, root: &Path) -> bool {
        root.join(&self.store).exists()
    }
}

/// The lock of the generation whose state is `state`, rebuilt from what each apply recorded.
pub fn lock_from_state(state: &State) -> Lock {
    let mut lock = Lock::default();
    for entry in state.entries.values() {
        if let Some(inst) = Installed::from_record(&entry.record) {
            lock.upsert(inst.locked);
        }
    }
    lock
}

/// What the lock says to install for a package on this host.
struct Target<'a> {
    locked: &'a LockedPkg,
    version: String,
    id: String,
    kind: Kind<'a>,
}

enum Kind<'a> {
    Archive { origin: Origin<'a>, dirname: String, artifact: &'a Artifact },
    Nix { attr: &'a str },
}

/// Where an archive comes from. A CI build is already patched; a download is patched here.
enum Origin<'a> {
    Url { url: &'a str, hash: &'a str },
    Oci { reference: &'a str },
}

fn target<'a>(cx: &'a Cx, p: &'a Pkg) -> Result<Target<'a>> {
    let missing = || format!("pkg {} is not in fspec.lock, run fspec update", p.name);
    let locked = cx.lock.pkg(&p.name).filter(|l| l.source == p.source.to_string()).with_context(missing)?;
    let version = locked.version.clone().or_else(|| locked.rev.clone()).unwrap_or_else(|| "unversioned".into());
    if let Source::Nix { attr } = &p.source {
        let id = locked.rev.clone().unwrap_or_else(|| version.clone());
        return Ok(Target { locked, version, id, kind: Kind::Nix { attr } });
    }
    fspec_core::patches::check(cx.spec_dir, &p.patches, locked.patches.as_deref())
        .with_context(|| format!("pkg {}", p.name))?;
    let platform = cx.facts.platform();
    let artifact =
        locked.artifacts.get(&platform).or_else(|| locked.artifacts.get("any")).with_context(|| {
            format!("pkg {} has no artifact for {platform} in fspec.lock, run fspec update", p.name)
        })?;
    if let Some(reference) = artifact.oci.as_deref() {
        let version = version.chars().take(12).collect::<String>();
        let dirname = format!("{}-{}-{version}", archive::hash8(reference), p.name);
        return Ok(Target {
            locked,
            version,
            id: reference.into(),
            kind: Kind::Archive { origin: Origin::Oci { reference }, dirname, artifact },
        });
    }
    let (Some(url), Some(hash)) = (artifact.url.as_deref(), artifact.hash.as_deref()) else {
        bail!(
            "pkg {} needs a url and a hash, or an oci reference, for {platform} in fspec.lock, run fspec update",
            p.name
        );
    };
    let identity = match &locked.patches {
        Some(patches) => format!("{hash}+{patches}"),
        None => hash.to_string(),
    };
    let dirname = format!("{}-{}-{version}", archive::hash8(&identity), p.name);
    Ok(Target {
        locked,
        version,
        id: identity,
        kind: Kind::Archive { origin: Origin::Url { url, hash }, dirname, artifact },
    })
}

/// A self_managed package keeps whatever it has once installed.
fn keeps_installed(cx: &Cx, op: &Op, p: &Pkg) -> Option<Installed> {
    if p.mode != PkgMode::SelfManaged {
        return None;
    }
    let Entry { resource: Resource::Pkg(old), record, .. } = op.current.as_ref()? else { return None };
    let inst = Installed::from_record(record)?;
    (old.source == p.source && inst.exists(&cx.paths.fspec_root())).then_some(inst)
}

fn pkg_of(op: &Op) -> Result<&Pkg> {
    match op.resource() {
        Resource::Pkg(p) => Ok(p),
        other => bail!("the store backend got {}", other.key()),
    }
}

impl Backend for StoreBackend {
    fn name(&self) -> &'static str {
        "store"
    }

    fn handles(&self, _cx: &Cx, r: &Resource) -> bool {
        let Resource::Pkg(p) = r else { return false };
        matches!(
            p.source,
            Source::Aqua { .. }
                | Source::Github { .. }
                | Source::Npm { .. }
                | Source::Cargo { .. }
                | Source::Git { .. }
                | Source::Nix { .. }
        )
    }

    fn phase(&self) -> Phase {
        Phase::Store
    }

    /// The lock moved past the installed version (pinned only), or a link went missing.
    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        let Resource::Pkg(p) = &entry.resource else { return Ok(vec![]) };
        let root = cx.paths.fspec_root();
        let Some(inst) = Installed::from_record(&entry.record) else { return Ok(vec!["no install recorded".into()]) };
        let mut notes = Vec::new();
        if p.mode == PkgMode::Pinned {
            let locked = cx.lock.pkg(&p.name).and_then(|l| l.patches.as_deref());
            if let Err(e) = fspec_core::patches::check(cx.spec_dir, &p.patches, locked) {
                notes.push(e.to_string());
            } else if let Ok(t) = target(cx, p)
                && t.id != inst.id
            {
                notes.push(format!("locked {} (installed {})", t.version, inst.version));
            }
        }
        if !inst.exists(&root) {
            notes.push(format!("{} is missing from the store", inst.store));
        }
        let broken: Vec<_> =
            inst.bins.keys().filter(|b| fs::metadata(root.join("current/bin").join(b)).is_err()).cloned().collect();
        if !broken.is_empty() {
            notes.push(format!("not linked in current: {}", broken.join(", ")));
        }
        Ok(notes)
    }

    fn prepare(&self, cx: &Cx, ops: &[Op], staging: &Path) -> Result<()> {
        let root = cx.paths.fspec_root();
        for op in ops.iter().filter(|o| !o.is_removal()) {
            let p = pkg_of(op)?;
            if keeps_installed(cx, op, p).is_some() {
                continue;
            }
            let t = target(cx, p)?;
            match t.kind {
                Kind::Nix { attr } => {
                    let out = nix_build(cx, t.locked, attr).with_context(|| format!("building {}", p.name))?;
                    let marker = staging.join("nix").join(&p.name);
                    fs::create_dir_all(marker.parent().context("marker has a parent")?)?;
                    fs::write(&marker, out)?;
                }
                Kind::Archive { origin, dirname, .. } => {
                    if root.join("store").join(&dirname).exists() {
                        continue;
                    }
                    let dest = staging.join("store").join(&dirname);
                    match origin {
                        Origin::Oci { reference } => {
                            let body = oci::pull(cx.runner, reference, &staging.join("oci").join(&dirname))
                                .with_context(|| format!("pulling {reference}"))?;
                            archive::unpack(&body, &dest, &p.name).with_context(|| format!("unpacking {reference}"))?;
                        }
                        Origin::Url { url, hash } => {
                            let body = cx.fetch.get(url)?;
                            archive::verify(&body, hash).with_context(|| format!("verifying {url}"))?;
                            archive::unpack(&body, &dest, &p.name).with_context(|| format!("unpacking {url}"))?;
                            if let Source::Npm { name } = &p.source {
                                let tarball = staging.join("dl").join(format!("{dirname}.tgz"));
                                fs::create_dir_all(tarball.parent().context("tarball has a parent")?)?;
                                fs::write(&tarball, &body)?;
                                npm::finish(cx.runner, &dest, &tarball, name)?;
                            }
                            if !p.patches.is_empty() {
                                let tree = patch_root(&dest, &p.source);
                                patch::apply_all(&tree, cx.spec_dir, &p.patches)
                                    .with_context(|| format!("patching {}", p.name))?;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn commit(&self, cx: &Cx, ops: &[Op], staging: &Path, journal: &mut Journal) -> Result<Records> {
        let root = cx.paths.fspec_root();
        fs::create_dir_all(root.join("store")).with_context(|| format!("creating {}", root.display()))?;
        let generation = Generation::open(&root, cx.generation, journal)?;
        let mut records = Records::new();
        for op in ops {
            let p = pkg_of(op)?;
            let before = op.current.as_ref().and_then(|e| Installed::from_record(&e.record));
            if let Some(before) = &before {
                for name in before.bins.keys() {
                    generation.unlink(name)?;
                }
            }
            if op.is_removal() {
                continue;
            }
            let inst = match keeps_installed(cx, op, p) {
                Some(kept) => kept,
                None => install(cx, p, staging, journal).with_context(|| format!("installing {}", p.name))?,
            };
            for (name, rel) in &inst.bins {
                generation.link(name, &link_target(&inst.store, rel))?;
            }
            records.insert(op.key.clone(), inst.to_record());
        }
        generation.switch(journal)?;
        Ok(records)
    }
}

/// Where patch paths are relative to: the package directory inside an npm install or the single
/// top-level directory of an archive, else the unpacked root.
fn patch_root(dest: &Path, source: &Source) -> PathBuf {
    if let Source::Npm { name } = source {
        let installed = dest.join("node_modules").join(name);
        return if installed.is_dir() { installed } else { dest.join("package") };
    }
    let mut entries = fs::read_dir(dest).into_iter().flatten().flatten();
    match (entries.next(), entries.next()) {
        (Some(only), None) if only.path().is_dir() => only.path(),
        _ => dest.to_path_buf(),
    }
}

#[derive(Serialize, Deserialize)]
struct Sidecar {
    name: String,
    version: String,
    bins: BTreeMap<String, String>,
}

/// Put the package's tree in the store (unless it is already there) and work out its executables.
fn install(cx: &Cx, p: &Pkg, staging: &Path, journal: &mut Journal) -> Result<Installed> {
    let root = cx.paths.fspec_root();
    let t = target(cx, p)?;
    let (store, bins) = match t.kind {
        Kind::Nix { .. } => {
            let out = fs::read_to_string(staging.join("nix").join(&p.name)).context("nix output was not prepared")?;
            let bins = nix_bins(Path::new(&out))?;
            (out, bins)
        }
        Kind::Archive { dirname, artifact, .. } => {
            let live = root.join("store").join(&dirname);
            if !live.exists() {
                let sidecar = root.join("store").join(format!("{dirname}.json"));
                undo_create(journal, &live)?;
                undo_create(journal, &sidecar)?;
                let from = staging.join("store").join(&dirname);
                fs::rename(&from, &live).or_else(|_| copy_any(&from, &live).and_then(|()| remove_any(&from)))?;
                let meta =
                    Sidecar { name: p.name.clone(), version: t.version.clone(), bins: bins_of(p, artifact, &live)? };
                fs::write(&sidecar, serde_json::to_vec_pretty(&meta)?)?;
            }
            let bins = bins_of(p, artifact, &live)?;
            (format!("store/{dirname}"), bins)
        }
    };
    if bins.is_empty() {
        bail!("it has no executables to link");
    }
    Ok(Installed { store, version: t.version, id: t.id, bins, locked: t.locked.clone() })
}

fn bins_of(p: &Pkg, artifact: &Artifact, dir: &Path) -> Result<BTreeMap<String, String>> {
    if let Source::Npm { name } = &p.source {
        return npm::bins(dir, name);
    }
    if artifact.bins.is_empty() {
        return Ok(discover(dir));
    }
    let mut bins = BTreeMap::new();
    for rel in &artifact.bins {
        if !dir.join(rel).is_file() {
            bail!("bin `{rel}` from fspec.lock is not in the artifact");
        }
        let name = rel.rsplit('/').next().unwrap_or(rel);
        bins.insert(name.to_string(), rel.clone());
    }
    Ok(bins)
}

/// Every executable regular file in the top two levels, shallower ones winning name clashes.
fn discover(dir: &Path) -> BTreeMap<String, String> {
    let mut bins = BTreeMap::new();
    let mut level = vec![(dir.to_path_buf(), String::new())];
    for depth in 0..2 {
        let mut subdirs = Vec::new();
        for (path, prefix) in &level {
            let mut entries: Vec<_> = fs::read_dir(path).into_iter().flatten().flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Ok(meta) = fs::symlink_metadata(entry.path()) else { continue };
                if meta.is_dir() && depth == 0 {
                    subdirs.push((entry.path(), format!("{prefix}{name}/")));
                } else if meta.is_file() && is_executable(&meta) && !name.starts_with('.') {
                    bins.entry(name.clone()).or_insert(format!("{prefix}{name}"));
                }
            }
        }
        level = subdirs;
    }
    bins
}

fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

fn nix_build(cx: &Cx, locked: &LockedPkg, attr: &str) -> Result<String> {
    let flake = match &locked.rev {
        Some(rev) => format!("github:NixOS/nixpkgs/{rev}#{attr}"),
        None => format!("nixpkgs#{attr}"),
    };
    let out = cx.runner.check(&Cmd::new("nix").args(["build", "--no-link", "--print-out-paths", &flake]))?;
    let path = out.stdout.lines().next().unwrap_or("").trim();
    if !path.starts_with('/') {
        bail!("`nix build {flake}` printed no store path");
    }
    Ok(path.into())
}

fn nix_bins(out: &Path) -> Result<BTreeMap<String, String>> {
    let mut bins = BTreeMap::new();
    let dir = out.join("bin");
    for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        bins.insert(name.clone(), format!("bin/{name}"));
    }
    Ok(bins)
}

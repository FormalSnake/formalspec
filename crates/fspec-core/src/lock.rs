use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// `fspec.lock`: every version FormalSpec installs, written by `fspec update` and committed.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Lock {
    #[serde(default)]
    pub pkg: Vec<LockedPkg>,
    /// Per host: the system layer's pinned digest.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub system: BTreeMap<String, LockedSystem>,
    /// Per Tumbleweed host: the snapshot its distro repos point at.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub zypper: BTreeMap<String, LockedZypper>,
    /// For a project's `dev.star`: its `lib(...)` and `bin(...)` dependencies, per package manager.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native: Vec<LockedNative>,
    /// For a project's `dev.star`: each OCI image it names (`debian:13`) pinned by digest.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub image: BTreeMap<String, String>,
}

/// What provides one `lib(...)` or `bin(...)`: the package (or `pkgconfig(...)`, `/usr/bin/...`
/// capability) to install, keyed by package manager, and why a lookup found nothing.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LockedNative {
    /// `lib` or `bin`.
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub miss: BTreeMap<String, String>,
    #[serde(flatten)]
    pub provides: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LockedPkg {
    pub name: String,
    /// `Source`'s display form, e.g. `aqua:BurntSushi/ripgrep`. A spec whose source no longer
    /// matches needs `fspec update`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// Hash over the package's patch files, for patched source builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patches: Option<String>,
    /// Keyed by platform (`x86_64-linux`, `aarch64-darwin`), or `any` for platform-independent
    /// artifacts such as npm tarballs.
    #[serde(flatten)]
    pub artifacts: BTreeMap<String, Artifact>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Artifact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `sha256-<base64>`, SRI style.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// `registry/repo@sha256:…` for CI-built artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oci: Option<String>,
    /// Flatpak commit, git rev, or another source-specific exact id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Executables inside the unpacked artifact, relative to its root (`rg`,
    /// `ripgrep-15.1.0-x86_64-unknown-linux-musl/rg`). Empty: link what a prefix tree's `bin/`
    /// holds, else every executable regular file in the top two levels. `<command>=<path>` names
    /// the command differently.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bins: Vec<String>,
    /// For pypi packages: every distribution to install, one `name==version --hash=sha256:...`
    /// line each, as uv compiles them for all platforms.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requirements: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LockedSystem {
    pub image: String,
}

/// A dated openSUSE Tumbleweed snapshot (`20261020`), served under
/// `https://download.opensuse.org/history/<snapshot>/tumbleweed/`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LockedZypper {
    pub snapshot: String,
}

impl Lock {
    /// A missing lock is empty; `plan` then reports packages that need `fspec update`.
    pub fn load(path: &Path) -> Result<Lock> {
        match fs::read_to_string(path) {
            Ok(t) => toml::from_str(&t).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Lock::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("lock.tmp");
        fs::write(&tmp, toml::to_string(self)?).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("renaming {} into place", tmp.display()))
    }

    pub fn native(&self, kind: &str, name: &str) -> Option<&LockedNative> {
        self.native.iter().find(|n| n.kind == kind && n.name == name)
    }

    pub fn pkg(&self, name: &str) -> Option<&LockedPkg> {
        self.pkg.iter().find(|p| p.name == name)
    }

    /// Insert or replace by name, keeping the file sorted so diffs stay small.
    pub fn upsert(&mut self, locked: LockedPkg) {
        self.pkg.retain(|p| p.name != locked.name);
        self.pkg.push(locked);
        self.pkg.sort_by(|a, b| a.name.cmp(&b.name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn design_example_parses() {
        let text = r#"
[[pkg]]
name = "ripgrep"
source = "aqua:BurntSushi/ripgrep"
version = "15.1.0"
x86_64-linux = { url = "https://example/rg-x86.tar.gz", hash = "sha256-AAAA" }
aarch64-darwin = { url = "https://example/rg-arm.tar.gz", hash = "sha256-BBBB" }

[[pkg]]
name = "librepods"
source = "git:https://example/librepods@main"
rev = "4f1c2a9"
patches = "sha256-CCCC"
x86_64-linux = { oci = "ghcr.io/formalstudio/fspec-cache/librepods@sha256:dd" }

[system.g815]
image = "ghcr.io/ublue-os/bazzite-nvidia@sha256:ee"

[zypper.e1504g]
snapshot = "20261020"
"#;
        let lock: Lock = toml::from_str(text).unwrap();
        assert_eq!(lock.pkg("ripgrep").unwrap().artifacts["aarch64-darwin"].hash.as_deref(), Some("sha256-BBBB"));
        assert!(lock.pkg("librepods").unwrap().artifacts["x86_64-linux"].oci.is_some());
        assert_eq!(lock.system["g815"].image, "ghcr.io/ublue-os/bazzite-nvidia@sha256:ee");
        assert_eq!(lock.zypper["e1504g"].snapshot, "20261020");
        let again: Lock = toml::from_str(&toml::to_string(&lock).unwrap()).unwrap();
        assert_eq!(again, lock);
    }
}

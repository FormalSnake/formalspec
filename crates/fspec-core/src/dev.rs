//! `dev.star`: a project's development environment, the counterpart of a flake's devShell.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::resource::Pkg;

/// The platforms a project locks for when `dev()` names none.
pub const PLATFORMS: [&str; 4] = ["x86_64-linux", "aarch64-linux", "x86_64-darwin", "aarch64-darwin"];

/// What one evaluation of `dev.star` declares, for one platform.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Dev {
    #[serde(default)]
    pub packages: Vec<Pkg>,
    #[serde(default)]
    pub libs: Vec<Native>,
    #[serde(default)]
    pub bins: Vec<Native>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub services: Vec<DevService>,
    /// sh code run after the environment is set, whose exports become part of it.
    #[serde(default)]
    pub shell_hook: Option<String>,
    /// An OCI image (`debian:13`) the shell runs in, with `libs` and `bins` installed from its
    /// own repos.
    #[serde(default)]
    pub container: Option<String>,
    /// The image the shell runs in instead on a machine whose package manager has no package
    /// for some `lib(...)` or `bin(...)`, or that has no package manager fspec knows.
    #[serde(default)]
    pub fallback_container: Option<String>,
    #[serde(default = "default_platforms")]
    pub platforms: Vec<String>,
}

fn default_platforms() -> Vec<String> {
    PLATFORMS.iter().map(|p| p.to_string()).collect()
}

/// A native dependency the system's package manager provides: `lib("gtk4")` is the pkg-config
/// module `gtk4`, `bin("cmake")` an executable on PATH. `fspec update` asks each package manager
/// which package provides it and locks the answer; a field named after a manager (`apt`, `dnf`,
/// `zypper`, `pacman`, `xbps`, `moss`, `brew`) names the package by hand where that lookup
/// finds nothing.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Native {
    pub name: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty", flatten)]
    pub overrides: BTreeMap<String, String>,
}

/// The package managers a [`Native`] dependency resolves for.
pub const MANAGERS: [&str; 7] = ["apt", "dnf", "zypper", "pacman", "xbps", "moss", "brew"];

/// A process that runs while a project shell is open. With `exec` it is a command from the
/// shell's PATH; without, the OCI image `image` (default `<name>:<version>`) run by docker or
/// podman.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DevService {
    pub name: String,
    #[serde(default)]
    pub exec: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    /// Ports published on 127.0.0.1, the same number inside and out.
    #[serde(default)]
    pub ports: Vec<u16>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl DevService {
    /// The image reference to pin, for a service without `exec`.
    pub fn image_ref(&self) -> Option<String> {
        if self.exec.is_some() {
            return None;
        }
        Some(self.image.clone().unwrap_or_else(|| match &self.version {
            Some(v) => format!("{}:{v}", self.name),
            None => format!("{}:latest", self.name),
        }))
    }
}

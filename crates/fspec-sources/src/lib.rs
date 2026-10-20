//! Turns a package declaration into a [`LockedPkg`]: exact version, URL and hash per platform.
//! Resolvers are the only code that talks to registries; apply installs from the lock alone.

mod age;
mod aqua;
mod cargo;
mod distro;
mod git;
mod github;
mod hash;
mod npm;
mod service;
pub mod update;

use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use fspec_core::facts::Facts;
use fspec_core::fetch::Fetch;
use fspec_core::lock::LockedPkg;
use fspec_core::resource::{Channel, Pkg, Source};
use fspec_core::runner::Runner;

pub use age::parse_age;

/// What resolvers read the world through.
pub struct Cx<'a> {
    pub fetch: &'a dyn Fetch,
    pub runner: &'a dyn Runner,
    /// Platform and distro of the machine running the update; only it can ask a package manager.
    pub local: (String, String),
    /// Unix seconds, the reference point for release-age cutoffs.
    pub now: i64,
    pub(crate) registry: OnceLock<aqua::Registry>,
}

impl<'a> Cx<'a> {
    pub fn new(fetch: &'a dyn Fetch, runner: &'a dyn Runner) -> Cx<'a> {
        Cx { fetch, runner, local: Facts::probe_local(), now: age::now(), registry: OnceLock::new() }
    }
}

/// Resolve `pkg` for every platform in `platforms` (`x86_64-linux`, ...). `min_age` is the
/// longest release-age delay among the hosts declaring the package; a bleeding package ignores it.
pub fn resolve(pkg: &Pkg, platforms: &[String], min_age: Option<i64>, cx: &Cx) -> Result<LockedPkg> {
    let min_age = if pkg.channel == Channel::Bleeding { None } else { min_age };
    let cutoff = min_age.map(|a| cx.now - a);
    let mut locked = LockedPkg {
        name: pkg.name.clone(),
        source: pkg.source.to_string(),
        version: None,
        rev: None,
        patches: None,
        artifacts: Default::default(),
    };
    let ctx = || format!("resolving {} ({})", pkg.name, pkg.source);
    let result = match &pkg.source {
        Source::Aqua { name } => aqua::resolve(name, platforms, cutoff, cx, &mut locked),
        Source::Github { repo, asset } => github::resolve(repo, asset.as_deref(), platforms, cutoff, cx, &mut locked),
        Source::Npm { name } => npm::resolve(name, cutoff, cx, &mut locked),
        Source::Cargo { name } => cargo::resolve(name, platforms, cutoff, cx, &mut locked),
        Source::Flatpak { id, remote } => service::flatpak(id.as_deref().unwrap_or(&pkg.name), remote, cx, &mut locked),
        Source::Brew { name, cask } => service::brew(name.as_deref().unwrap_or(&pkg.name), *cask, cx, &mut locked),
        Source::Git { url, rev } => git::resolve(url, rev, cx, &mut locked),
        Source::Nix { attr } => service::nix(attr, cx, &mut locked),
        Source::Dnf(_) | Source::Pacman(_) | Source::Apt(_) | Source::Xbps(_) => {
            distro::resolve(&pkg.source, &pkg.name, platforms, cx, &mut locked)
        }
    };
    result.map_err(|e| anyhow!("{}: {e:#}", ctx()))?;
    Ok(locked)
}

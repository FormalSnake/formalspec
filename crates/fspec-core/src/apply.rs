//! The apply transaction. Backends claim resources; the engine plans, prepares every backend
//! (fetch, verify, render: nothing live changes), then commits them in phase order under a
//! journal, and writes the new state only when every commit succeeded.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::attribute;
use crate::facts::Facts;
use crate::fetch::Fetch;
use crate::journal::{Journal, remove_any};
use crate::lock::Lock;
use crate::paths::Paths;
use crate::plan::{Action, Change, Plan, plan};
use crate::resource::{Desired, Key, Resource, Scope};
use crate::runner::Runner;
use crate::state::{Entry, State};
use crate::trash::{data_paths, restore_pkg, trash_paths};

/// What a backend may read while planning and applying.
pub struct Cx<'a> {
    pub paths: &'a Paths,
    pub facts: &'a Facts,
    /// The directory holding `spec.star`; `src = "files/x"` resolves against it.
    pub spec_dir: &'a Path,
    pub lock: &'a Lock,
    pub runner: &'a dyn Runner,
    pub fetch: &'a dyn Fetch,
    /// The generation this apply creates (the state's current one plus one).
    pub generation: u64,
    pub scope: Scope,
    /// The host's `protect` list, as written in the spec.
    pub protect: &'a [String],
}

impl Cx<'_> {
    /// Resolve a spec path (`~/…` or `/…`) under the rooted paths.
    pub fn resolve(&self, spec_path: &str) -> Result<PathBuf> {
        self.paths.resolve(spec_path)
    }

    /// Refuse a path under one of the host's `protect` entries. When a protected entry is an
    /// ancestor of the applying user's home (`/home`), that home stays usable, since the spec
    /// has to manage it; other users' homes do not.
    pub fn check_path(&self, path: &Path) -> Result<()> {
        let path = lexical(path);
        let home = lexical(&self.paths.home);
        for entry in self.protect {
            let guard = lexical(&self.paths.resolve(entry)?);
            let own_home = guard != home && home.starts_with(&guard) && path.starts_with(&home);
            if path.starts_with(&guard) && !own_home {
                bail!("{} is under protected path {entry}", path.display());
            }
        }
        Ok(())
    }

    /// The state directory for this scope.
    pub fn state_dir(&self) -> PathBuf {
        state_dir(self.paths, self.scope)
    }
}

fn lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The resources a past generation had applied, for `rollback`. Generation 0 is the empty state.
pub fn desired_from_generation(state_dir: &Path, n: u64) -> Result<Vec<Desired>> {
    if n == 0 {
        return Ok(vec![]);
    }
    let path = state_dir.join("generations").join(format!("{n}.json"));
    if !path.exists() {
        bail!("generation {n} was not found in {}", state_dir.display());
    }
    let state = State::load(&path)?;
    Ok(state.entries.into_values().map(|e| Desired { resource: e.resource, owner: e.owner }).collect())
}

pub fn state_dir(paths: &Paths, scope: Scope) -> PathBuf {
    match scope {
        Scope::System => paths.system_state_dir(),
        Scope::User => paths.user_state_dir(),
    }
}

/// Commit order. Removals run in reverse, so services stop before their packages go.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    Users,
    Files,
    /// The distro's repos and base packages, before any `pkg` installs from them.
    Distro,
    Packages,
    /// After the packages, so a store build can use the npm or oras the same apply installs.
    Store,
    Services,
    Defaults,
    System,
}

/// One resource to change. `desired` is `None` for a removal, `current` is `None` for an add.
#[derive(Clone, Debug)]
pub struct Op {
    pub key: Key,
    pub desired: Option<Desired>,
    pub current: Option<Entry>,
    /// Set when the config is unchanged but the live system drifted from the record.
    pub drift: Vec<String>,
}

impl Op {
    pub fn resource(&self) -> &Resource {
        match (&self.desired, &self.current) {
            (Some(d), _) => &d.resource,
            (None, Some(e)) => &e.resource,
            (None, None) => unreachable!("an op has a desired or a current resource"),
        }
    }

    pub fn is_removal(&self) -> bool {
        self.desired.is_none()
    }

    pub fn owner(&self) -> Option<&str> {
        match (&self.desired, &self.current) {
            (Some(d), _) => d.owner.as_deref(),
            (None, Some(e)) => e.owner.as_deref(),
            _ => None,
        }
    }

    /// The record the last apply left, empty for an add.
    pub fn record(&self) -> Map<String, Value> {
        self.current.as_ref().map(|e| e.record.clone()).unwrap_or_default()
    }
}

/// What each backend's commit returns: the new record per applied (not removed) key.
pub type Records = BTreeMap<Key, Map<String, Value>>;

pub trait Backend {
    fn name(&self) -> &'static str;

    /// Whether this backend applies `r` on this host. Exactly one backend must claim each resource.
    fn handles(&self, cx: &Cx, r: &Resource) -> bool;

    fn phase(&self) -> Phase;

    /// How the live system diverged from what the last apply recorded, one line per finding.
    /// Called only for resources whose config did not change.
    fn drift(&self, _cx: &Cx, _entry: &Entry) -> Result<Vec<String>> {
        Ok(vec![])
    }

    /// Fail the plan early when the host cannot apply this resource (a systemd-only field on runit).
    fn check(&self, _cx: &Cx, _r: &Resource) -> Result<()> {
        Ok(())
    }

    /// Fetch, verify and render into `staging`. Must not change the live system.
    fn prepare(&self, _cx: &Cx, _ops: &[Op], _staging: &Path) -> Result<()> {
        Ok(())
    }

    /// Make `ops` live. Journal an undo step before each live mutation. Called once with the
    /// removals and once with the adds and updates.
    fn commit(&self, cx: &Cx, ops: &[Op], staging: &Path, journal: &mut Journal) -> Result<Records>;
}

/// Plan for one scope: the config diff plus drift that backends report on unchanged resources.
pub fn plan_scope(cx: &Cx, backends: &[&dyn Backend], desired: &[Desired], state: &State) -> Result<Plan> {
    let desired: Vec<Desired> = desired.iter().filter(|d| d.resource.scope() == cx.scope).cloned().collect();
    let empty = State::default();
    let (system, user) = match cx.scope {
        Scope::System => (state, &empty),
        Scope::User => (&empty, state),
    };
    let mut p = plan(&cx.facts.name, &desired, system, user)?;
    let claims = user_claims(cx)?;
    for d in &desired {
        if let Resource::Pkg(pkg) = &d.resource {
            for spec in &pkg.data {
                cx.check_path(&cx.resolve(spec)?).with_context(|| format!("data of {}", d.resource.key()))?;
            }
        }
        match backend_for(cx, backends, &d.resource) {
            Ok(b) => {
                if let Err(e) = b.check(cx, &d.resource) {
                    p.unsupported.push(format!("{}: {e:#}", d.resource.key()));
                }
            }
            Err(e) => p.unsupported.push(e.to_string()),
        }
    }
    let changed: Vec<Key> = p.changes.iter().map(|c| c.key.clone()).collect();
    for (key, entry) in &state.entries {
        if changed.contains(key) {
            continue;
        }
        let Ok(b) = backend_for(cx, backends, &entry.resource) else { continue };
        let notes = b.drift(cx, entry)?;
        if !notes.is_empty() {
            let detail = String::new();
            p.changes.push(Change { key: key.clone(), scope: cx.scope, action: Action::Drift { notes }, detail });
        }
    }
    for change in p.changes.iter_mut().filter(|c| c.action == Action::Remove) {
        let Some(Entry { resource: Resource::Pkg(pkg), .. }) = state.entries.get(&change.key) else { continue };
        if pkg.keep_data {
            continue;
        }
        let paths = data_paths(cx, pkg, &claims)?;
        for path in &paths {
            cx.check_path(path).with_context(|| format!("data of {}", change.key))?;
        }
        if !paths.is_empty() {
            let list: Vec<String> = paths.iter().map(|p| cx.paths.to_spec(p)).collect();
            change.detail = format!("{} (data to trash: {})", change.detail, list.join(", "));
        }
    }
    p.changes.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(p)
}

/// The data attributed to packages, kept in the user state whichever scope is applying.
fn user_claims(cx: &Cx) -> Result<BTreeMap<String, Vec<String>>> {
    Ok(State::load(&state_dir(cx.paths, Scope::User).join("state.json"))?.data)
}

fn backend_for<'b>(cx: &Cx, backends: &[&'b dyn Backend], r: &Resource) -> Result<&'b dyn Backend> {
    let claimed: Vec<_> = backends.iter().filter(|b| b.handles(cx, r)).collect();
    match claimed.as_slice() {
        [b] => Ok(**b),
        [] => bail!("no backend applies {} on {} ({}, {})", r.key(), cx.facts.name, cx.facts.distro, cx.facts.init),
        many => {
            let names: Vec<_> = many.iter().map(|b| b.name()).collect();
            bail!("{} is claimed by several backends: {}", r.key(), names.join(", "))
        }
    }
}

/// Converge one scope. Recovers an interrupted apply first. On failure the journal is replayed
/// and the state file is left as it was.
pub fn apply_scope(cx: &Cx, backends: &[&dyn Backend], desired: &[Desired]) -> Result<Plan> {
    let dir = cx.state_dir();
    let state_path = dir.join("state.json");
    Journal::recover(&dir, cx.runner)?;
    let mut state = State::load(&state_path)?;
    if cx.scope == Scope::User && attribute::refresh(cx, &mut state, desired)? {
        state.save(&state_path)?;
    }
    if cx.generation != state.generation + 1 {
        bail!("apply for generation {} but the state is at {}", cx.generation, state.generation);
    }
    let p = plan_scope(cx, backends, desired, &state)?;
    if !p.unsupported.is_empty() {
        bail!("cannot apply:\n  {}", p.unsupported.join("\n  "));
    }
    if p.changes.is_empty() {
        return Ok(p);
    }

    let wanted: BTreeMap<Key, &Desired> = desired.iter().map(|d| (d.resource.key(), d)).collect();
    let ops: Vec<Op> = p
        .changes
        .iter()
        .map(|c| Op {
            key: c.key.clone(),
            desired: wanted.get(&c.key).map(|d| (*d).clone()),
            current: state.entries.get(&c.key).cloned(),
            drift: match &c.action {
                Action::Drift { notes } => notes.clone(),
                _ => vec![],
            },
        })
        .collect();

    let mut by_backend: Vec<(&dyn Backend, Vec<Op>)> = Vec::new();
    for op in ops {
        let b = backend_for(cx, backends, op.resource())?;
        match by_backend.iter_mut().find(|(x, _)| x.name() == b.name()) {
            Some((_, list)) => list.push(op),
            None => by_backend.push((b, vec![op])),
        }
    }
    by_backend.sort_by_key(|(b, _)| b.phase());

    let staging = dir.join("staging").join(cx.generation.to_string());
    remove_any(&staging)?;
    fs::create_dir_all(&staging).with_context(|| format!("creating {}", staging.display()))?;
    for (b, ops) in &by_backend {
        b.prepare(cx, ops, &staging).with_context(|| format!("preparing {}", b.name()))?;
    }

    let mut journal = Journal::begin(&dir, cx.generation)?;
    let result = commit_all(cx, &by_backend, &staging, &mut journal);
    let records = match result {
        Ok(r) => r,
        Err(e) => {
            let rollback = journal.rollback(cx.runner);
            remove_any(&staging)?;
            return match rollback {
                Ok(()) => Err(e.context("apply failed and was rolled back")),
                Err(r) => Err(e.context(format!("apply failed and rollback was incomplete: {r:#}"))),
            };
        }
    };

    let mut next = State { generation: cx.generation, ..state.clone() };
    for (_, ops) in &by_backend {
        for op in ops {
            match &op.desired {
                None => {
                    next.entries.remove(&op.key);
                    if op.key.kind == "pkg" {
                        next.data.remove(&op.key.id);
                    }
                }
                Some(d) => {
                    let record = records.get(&op.key).cloned().unwrap_or_default();
                    next.entries
                        .insert(op.key.clone(), Entry { resource: d.resource.clone(), owner: d.owner.clone(), record });
                }
            }
        }
    }
    next.save(&state_path)?;
    next.save(&dir.join("generations").join(format!("{}.json", cx.generation)))?;
    journal.finish()?;
    remove_any(&staging)?;
    Ok(p)
}

fn commit_all(
    cx: &Cx,
    by_backend: &[(&dyn Backend, Vec<Op>)],
    staging: &Path,
    journal: &mut Journal,
) -> Result<Records> {
    let mut records = Records::new();
    for (b, ops) in by_backend.iter().rev() {
        let removals: Vec<Op> = ops.iter().filter(|o| o.is_removal()).cloned().collect();
        if !removals.is_empty() {
            b.commit(cx, &removals, staging, journal).with_context(|| format!("{}: removing", b.name()))?;
        }
    }
    let ops = || by_backend.iter().flat_map(|(_, ops)| ops);
    trash_removed(cx, ops().filter(|o| o.is_removal()), journal)?;
    for (b, ops) in by_backend {
        let rest: Vec<Op> = ops.iter().filter(|o| !o.is_removal()).cloned().collect();
        if !rest.is_empty() {
            records.extend(b.commit(cx, &rest, staging, journal).with_context(|| format!("{}: applying", b.name()))?);
        }
    }
    restore_returned(cx, ops().filter(|o| o.current.is_none() && !o.is_removal()), journal)?;
    Ok(records)
}

/// Removed packages lose their app data to the trash once every backend has removed what it
/// owned. This is the engine's job, not a backend's, so every package source behaves the same
/// and the moves share the apply's journal.
fn trash_removed<'o>(cx: &Cx, removals: impl Iterator<Item = &'o Op>, journal: &mut Journal) -> Result<()> {
    let claims = user_claims(cx)?;
    for op in removals {
        if let Resource::Pkg(pkg) = op.resource()
            && !pkg.keep_data
        {
            trash_paths(cx, journal, Some(&pkg.name), &data_paths(cx, pkg, &claims)?)
                .with_context(|| format!("moving the data of {} to the trash", pkg.name))?;
        }
    }
    Ok(())
}

/// A package that comes back (a rollback re-adds it) gets the data its removal trashed.
fn restore_returned<'o>(cx: &Cx, adds: impl Iterator<Item = &'o Op>, journal: &mut Journal) -> Result<()> {
    for op in adds {
        if let Resource::Pkg(pkg) = op.resource() {
            restore_pkg(cx, journal, &pkg.name).with_context(|| format!("restoring the data of {}", pkg.name))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

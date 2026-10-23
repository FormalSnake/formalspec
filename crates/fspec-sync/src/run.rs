//! Which items sync with which peers, in what order, and one item between two sides.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use fspec_core::Host;
use fspec_core::resource::{SyncItem, SyncMode};
use fspec_core::runner::Runner;
use fspec_remote::Reach;

use crate::{Apply, Local, Peer, Side, Way, plan};

#[derive(Default)]
pub struct Opts {
    /// Hand a `seed` item (or replace a `live` one) over from this host.
    pub from: Option<String>,
    pub dry_run: bool,
    /// Only these peers.
    pub peer: Option<String>,
}

pub enum Outcome {
    Done { conflicts: Vec<String> },
    Skipped,
}

/// Syncs `item` between `local` and `peer`, printing what moved.
pub fn pair(local: &Local, peer: &dyn Side, item: &SyncItem, opts: &Opts) -> Result<Outcome> {
    let (name, there) = (&item.name, peer.host());
    let base = local.base(name, there)?;
    let mine = local.scan(item, &base)?;
    let theirs = peer.scan(item, &base)?;
    for (side, m) in [(local.host(), &mine), (there, &theirs)] {
        if let Some(proc) = &m.busy {
            println!("{name}: {proc} is running on {side}, skipped");
            return Ok(Outcome::Skipped);
        }
    }
    let way = match (&opts.from, item.mode) {
        (Some(from), _) if from == local.host() => Way::Copy { from_local: true },
        (Some(from), _) if from == there => Way::Copy { from_local: false },
        (Some(from), _) => bail!("{name}: --from {from} is neither {} nor {there}", local.host()),
        (None, SyncMode::Live) => Way::Merge(item.conflict),
        (None, SyncMode::Seed) => match (mine.is_empty(), theirs.is_empty()) {
            (false, true) => Way::Copy { from_local: true },
            (true, false) => Way::Copy { from_local: false },
            (true, true) => {
                println!("{name}: neither host has it");
                return Ok(Outcome::Done { conflicts: vec![] });
            }
            (false, false) => {
                println!(
                    "{name}: seeded on both hosts, each keeps its own; hand it over with `fspec sync {name} --from <host>`"
                );
                return Ok(Outcome::Done { conflicts: vec![] });
            }
        },
    };
    let p = plan(&mine, &theirs, &base, way);
    let mut said = false;
    for (c, dir) in [(&p.to_peer, format!("to {there}")), (&p.to_local, format!("from {there}"))] {
        if !c.is_empty() {
            println!("{name}: {} {dir}", c.describe());
            said = true;
        }
    }
    for k in &p.conflicts {
        println!("{name}: {k} changed on both hosts; left as it is on each");
    }
    if !said && p.conflicts.is_empty() {
        println!("{name}: in sync with {there}");
    }
    if opts.dry_run {
        return Ok(Outcome::Done { conflicts: p.conflicts });
    }

    let tmp = tempfile::tempdir()?;
    let tar = tmp.path().join("files.tar");
    if !p.to_peer.files.is_empty() {
        local.pack(item, &p.to_peer.files, &tar)?;
        peer.unpack(name, &tar)?;
    }
    if !p.to_local.files.is_empty() {
        peer.pack(item, &p.to_local.files, &tar)?;
        local.unpack(name, &tar)?;
    }
    let secrets = |from: &dyn Side, keys: &[String]| -> Result<Vec<_>> {
        if keys.is_empty() { Ok(vec![]) } else { from.secrets(item, keys) }
    };
    let to_peer = Apply {
        item: item.clone(),
        peer: local.host().into(),
        base: p.base.clone(),
        secrets: secrets(local, &p.to_peer.keyring)?,
        changes: p.to_peer,
    };
    let to_local = Apply {
        item: item.clone(),
        peer: there.into(),
        base: p.base,
        secrets: secrets(peer, &p.to_local.keyring)?,
        changes: p.to_local,
    };
    peer.apply(&to_peer)?;
    local.apply(&to_local)?;
    Ok(Outcome::Done { conflicts: p.conflicts })
}

/// `items` in an order where each comes after the ones its `after` names.
fn ordered(items: &[SyncItem]) -> Result<Vec<&SyncItem>> {
    let names: BTreeSet<&str> = items.iter().map(|i| i.name.as_str()).collect();
    let mut done: Vec<&SyncItem> = Vec::new();
    let mut left: Vec<&SyncItem> = items.iter().collect();
    while !left.is_empty() {
        let ready = left
            .iter()
            .position(|i| i.after.iter().all(|a| !names.contains(a.as_str()) || done.iter().any(|d| d.name == *a)));
        let Some(at) = ready else {
            let stuck: Vec<&str> = left.iter().map(|i| i.name.as_str()).collect();
            bail!("sync items wait on each other in a cycle: {}", stuck.join(", "));
        };
        done.push(left.remove(at));
    }
    Ok(done)
}

/// Syncs this host's items (all of them, or those named) with their peers.
pub fn run(
    runner: &dyn Runner,
    local: &Local,
    hosts: &[Host],
    this: &Host,
    names: &[String],
    opts: &Opts,
) -> Result<()> {
    for n in names {
        if !this.sync.iter().any(|i| i.name == *n) {
            bail!("host {} has no sync item {n}", this.name);
        }
    }
    let chosen: Vec<SyncItem> =
        this.sync.iter().filter(|i| names.is_empty() || names.contains(&i.name)).cloned().collect();
    let mut routes: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut done: BTreeSet<String> = BTreeSet::new();
    let (mut conflicts, mut failed) = (0, Vec::new());
    for item in ordered(&chosen)? {
        if let Some(dep) = item.after.iter().find(|a| chosen.iter().any(|c| c.name == **a) && !done.contains(*a)) {
            println!("{}: waits for {dep}, skipped", item.name);
            continue;
        }
        if !item.peers.contains(&this.name) {
            bail!("sync({}) lists peers {} without this host, {}", item.name, item.peers.join(", "), this.name);
        }
        let mut ok = true;
        for peer_name in item.peers.iter().filter(|p| **p != this.name) {
            if opts.peer.as_ref().is_some_and(|p| p != peer_name) {
                continue;
            }
            let result = (|| -> Result<bool> {
                let peer = hosts.iter().find(|h| h.name == *peer_name).with_context(|| {
                    format!("sync({}) names peer {peer_name}, which the spec does not declare", item.name)
                })?;
                if peer.sync.iter().find(|i| i.name == item.name) != Some(item) {
                    bail!(
                        "sync({}) differs between {} and {peer_name}, or {peer_name} does not list it",
                        item.name,
                        this.name
                    );
                }
                if !routes.contains_key(peer_name) {
                    routes.insert(peer_name.clone(), reach(runner, this, peer)?);
                }
                let Some(route) = routes[peer_name].clone() else {
                    println!("{}: {peer_name} is unreachable, skipped", item.name);
                    return Ok(false);
                };
                let side = Peer { runner, host: peer_name.clone(), route };
                Ok(match pair(local, &side, item, opts)? {
                    Outcome::Done { conflicts: c } => {
                        conflicts += c.len();
                        true
                    }
                    Outcome::Skipped => false,
                })
            })();
            match result {
                Ok(true) => {}
                Ok(false) => ok = false,
                Err(e) => {
                    eprintln!("fspec: {} with {peer_name}: {e:#}", item.name);
                    failed.push(format!("{} with {peer_name}", item.name));
                    ok = false;
                }
            }
        }
        if ok {
            done.insert(item.name.clone());
        }
    }
    if !failed.is_empty() {
        bail!("sync failed for {}", failed.join(", "));
    }
    if conflicts > 0 {
        bail!(
            "{conflicts} conflicting change(s) left as they are; set conflict = \"newest\" or hand one side over with --from"
        );
    }
    Ok(())
}

/// The first route to `peer` that answers with a current fspec on the declared machine: the
/// routes of this host's builder of the same name, else the peer's ssh alias.
fn reach(runner: &dyn Runner, this: &Host, peer: &Host) -> Result<Option<String>> {
    let alias = peer.ssh.clone().unwrap_or_else(|| peer.name.clone());
    let routes: Vec<String> = match this.builders.iter().find(|b| b.name == peer.name) {
        Some(b) => b.routes().into_iter().map(String::from).collect(),
        None => vec![alias],
    };
    for route in routes {
        if let Reach::Reachable(probe) = fspec_remote::probe(runner, &route)? {
            fspec_remote::verify(peer, &probe)?;
            fspec_remote::current(&route, &probe)?;
            return Ok(Some(route));
        }
    }
    Ok(None)
}

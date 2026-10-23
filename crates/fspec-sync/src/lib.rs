//! `fspec sync`: app data between the user's own hosts, peer to peer over ssh. Each side scans
//! the item into a [`Manifest`], [`plan`] merges the two against the base both agreed on last
//! time, and each side applies its half.

pub mod keyring;
mod local;
mod peer;
mod run;

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use fspec_core::resource::{Conflict, SyncItem};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use local::Local;
pub use peer::{Peer, agent};
pub use run::{Opts, Outcome, pair, run};

/// One file, keyring item or JSON key as a side has it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Meta {
    /// sha256 of a file or secret, or a JSON value as text.
    pub id: String,
    /// Modification time: seconds for files and JSON files, `YYYYMMDDhhmmss` for keyring items.
    pub time: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// An item as one side holds it. File keys are `~/` paths, keyring keys are
/// [`keyring::Found::key`], JSON keys are `<file>#<key>`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Manifest {
    #[serde(default)]
    pub files: BTreeMap<String, Meta>,
    #[serde(default)]
    pub keyring: BTreeMap<String, Meta>,
    #[serde(default)]
    pub keys: BTreeMap<String, Meta>,
    /// The `unless_running` process that runs on this side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy: Option<String>,
}

impl Manifest {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.keyring.is_empty() && self.keys.is_empty()
    }
}

/// What one side receives.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Changes {
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub delete: Vec<String>,
    #[serde(default)]
    pub keyring: Vec<String>,
    /// A key's new value, or `None` to remove it.
    #[serde(default)]
    pub keys: BTreeMap<String, Option<Value>>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.delete.is_empty() && self.keyring.is_empty() && self.keys.is_empty()
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        let mut count = |n: usize, one: &str, many: &str| {
            if n > 0 {
                parts.push(format!("{n} {}", if n == 1 { one } else { many }));
            }
        };
        count(self.files.len(), "file", "files");
        count(self.delete.len(), "deletion", "deletions");
        count(self.keyring.len(), "keyring item", "keyring items");
        count(self.keys.len(), "key", "keys");
        parts.join(", ")
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub to_local: Changes,
    pub to_peer: Changes,
    pub conflicts: Vec<String>,
    /// What both sides hold once the plan is applied.
    pub base: Manifest,
}

/// How two manifests combine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Way {
    /// Three-way against the base.
    Merge(Conflict),
    /// One side's copy replaces the other's.
    Copy { from_local: bool },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pick {
    Same,
    Local,
    Peer,
    Conflict,
}

fn id(m: Option<&Meta>) -> Option<&str> {
    m.map(|m| m.id.as_str())
}

fn pick(a: Option<&Meta>, b: Option<&Meta>, base: Option<&Meta>, way: Way) -> Pick {
    if id(a) == id(b) {
        return Pick::Same;
    }
    match way {
        Way::Copy { from_local: true } => Pick::Local,
        Way::Copy { from_local: false } => Pick::Peer,
        Way::Merge(_) if id(a) == id(base) => Pick::Peer,
        Way::Merge(_) if id(b) == id(base) => Pick::Local,
        Way::Merge(Conflict::Manual) => Pick::Conflict,
        Way::Merge(Conflict::Newest) => match (a, b) {
            (Some(x), Some(y)) if y.time > x.time => Pick::Peer,
            (None, Some(_)) => Pick::Peer,
            _ => Pick::Local,
        },
    }
}

/// Merges one kind of entry. A keyring item is never deleted: a side that lacks one gets it.
fn kind<'m>(
    local: &'m BTreeMap<String, Meta>,
    peer: &'m BTreeMap<String, Meta>,
    base: &BTreeMap<String, Meta>,
    way: Way,
    keep_all: bool,
    mut send: impl FnMut(bool, &str, Option<&'m Meta>),
    conflicts: &mut Vec<String>,
) -> BTreeMap<String, Meta> {
    let mut next = BTreeMap::new();
    let keys: std::collections::BTreeSet<&String> = local.keys().chain(peer.keys()).collect();
    for k in keys {
        let (a, b) = (local.get(k), peer.get(k));
        let mut p = pick(a, b, base.get(k), way);
        if keep_all {
            p = match (p, a, b) {
                (Pick::Local, None, _) | (Pick::Peer, _, None) => match way {
                    Way::Merge(_) => {
                        if a.is_none() {
                            Pick::Peer
                        } else {
                            Pick::Local
                        }
                    }
                    Way::Copy { .. } => Pick::Same,
                },
                _ => p,
            };
        }
        let kept = match p {
            Pick::Same => a.or(b),
            Pick::Local => {
                send(false, k, a);
                a
            }
            Pick::Peer => {
                send(true, k, b);
                b
            }
            Pick::Conflict => {
                conflicts.push(k.clone());
                base.get(k)
            }
        };
        if let Some(m) = kept {
            next.insert(k.clone(), m.clone());
        }
    }
    next
}

/// What each side receives so both end up the same, and the base they then share.
pub fn plan(local: &Manifest, peer: &Manifest, base: &Manifest, way: Way) -> Plan {
    let mut p = Plan::default();
    let (mut to_local, mut to_peer) = (Changes::default(), Changes::default());
    let mut conflicts = Vec::new();
    let files = kind(
        &local.files,
        &peer.files,
        &base.files,
        way,
        false,
        |to_local_side, k, m| {
            let c = if to_local_side { &mut to_local } else { &mut to_peer };
            match m {
                Some(_) => c.files.push(k.into()),
                None => c.delete.push(k.into()),
            }
        },
        &mut conflicts,
    );
    let keyring = kind(
        &local.keyring,
        &peer.keyring,
        &base.keyring,
        way,
        true,
        |to_local_side, k, _| {
            let c = if to_local_side { &mut to_local } else { &mut to_peer };
            c.keyring.push(k.into());
        },
        &mut conflicts,
    );
    let keys = kind(
        &local.keys,
        &peer.keys,
        &base.keys,
        way,
        false,
        |to_local_side, k, m| {
            let c = if to_local_side { &mut to_local } else { &mut to_peer };
            c.keys.insert(k.into(), m.and_then(|m| m.value.clone()));
        },
        &mut conflicts,
    );
    p.to_local = to_local;
    p.to_peer = to_peer;
    p.conflicts = conflicts;
    p.base = Manifest { files, keyring, keys, busy: None };
    p
}

/// What a side receives, with the secrets of the keyring items it gets.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Apply {
    pub item: SyncItem,
    /// The other side's host name, under which this side keeps the base.
    pub peer: String,
    pub base: Manifest,
    pub changes: Changes,
    #[serde(default)]
    pub secrets: Vec<keyring::Found>,
}

/// One end of a sync: this machine, or a peer over ssh.
pub trait Side {
    fn host(&self) -> &str;
    fn scan(&self, item: &SyncItem, base: &Manifest) -> Result<Manifest>;
    /// Writes the listed files as a tar to `tar`.
    fn pack(&self, item: &SyncItem, files: &[String], tar: &Path) -> Result<()>;
    /// Stages a tar from [`Side::pack`] for the next [`Side::apply`].
    fn unpack(&self, item: &str, tar: &Path) -> Result<()>;
    fn secrets(&self, item: &SyncItem, keys: &[String]) -> Result<Vec<keyring::Found>>;
    fn apply(&self, req: &Apply) -> Result<()>;
}

#[cfg(test)]
mod tests;

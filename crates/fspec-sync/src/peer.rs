//! A peer's side of a sync, run over ssh by `fspec sync-agent` on the peer, which reads a JSON
//! request on stdin. Files travel as tar; secrets only inside the ssh session's pipes.

use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use fspec_core::resource::SyncItem;
use fspec_core::runner::{Cmd, Runner};
use fspec_remote::cmd::{FSPEC, batch};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::keyring::Found;
use crate::{Apply, Local, Manifest, Side};

pub struct Peer<'a> {
    pub runner: &'a dyn Runner,
    pub host: String,
    /// The ssh destination that answered the probe.
    pub route: String,
}

#[derive(Serialize, Deserialize)]
struct Scan {
    item: SyncItem,
    base: Manifest,
}

#[derive(Serialize, Deserialize)]
struct Files {
    item: SyncItem,
    keys: Vec<String>,
}

impl Peer<'_> {
    fn agent(&self, op: &str) -> Cmd {
        batch(&self.route).arg(format!("{FSPEC} sync-agent {op}"))
    }

    fn call<T: DeserializeOwned>(&self, op: &str, req: &impl Serialize) -> Result<T> {
        let out = self
            .runner
            .check(&self.agent(op).stdin(serde_json::to_string(req)?))
            .with_context(|| format!("`fspec sync-agent {op}` on {}", self.host))?;
        serde_json::from_str(&out.stdout)
            .with_context(|| format!("`fspec sync-agent {op}` on {} printed no JSON", self.host))
    }
}

impl Side for Peer<'_> {
    fn host(&self) -> &str {
        &self.host
    }

    fn scan(&self, item: &SyncItem, base: &Manifest) -> Result<Manifest> {
        self.call("scan", &Scan { item: item.clone(), base: base.clone() })
    }

    fn pack(&self, item: &SyncItem, files: &[String], tar: &Path) -> Result<()> {
        let ssh = self.agent("pack");
        let req = serde_json::to_string(&Files { item: item.clone(), keys: files.to_vec() })?;
        let cmd = Cmd::new("sh")
            .args(["-c", r#"out=$1; shift; "$@" > "$out""#, "sh", &tar.display().to_string()])
            .arg(ssh.program)
            .args(ssh.args)
            .stdin(req);
        self.runner.check(&cmd).with_context(|| format!("fetching {} from {}", item.name, self.host))?;
        Ok(())
    }

    fn unpack(&self, item: &str, tar: &Path) -> Result<()> {
        self.runner
            .check(&self.agent(&format!("unpack {item}")).stdin_file(tar))
            .with_context(|| format!("sending {item} to {}", self.host))?;
        Ok(())
    }

    fn secrets(&self, item: &SyncItem, keys: &[String]) -> Result<Vec<Found>> {
        self.call("secrets", &Files { item: item.clone(), keys: keys.to_vec() })
    }

    fn apply(&self, req: &Apply) -> Result<()> {
        let _: serde_json::Value = self.call("apply", req)?;
        Ok(())
    }
}

/// `fspec sync-agent <op> [item]` on a peer: one [`Side`] call, the request on `input`, the
/// answer on `out`.
pub fn agent(local: &Local, op: &str, item: Option<&str>, input: &mut dyn Read, out: &mut dyn Write) -> Result<()> {
    let read = |input: &mut dyn Read| -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        input.read_to_end(&mut buf)?;
        Ok(buf)
    };
    match op {
        "scan" => {
            let req: Scan = serde_json::from_slice(&read(input)?)?;
            serde_json::to_writer(out, &local.scan(&req.item, &req.base)?)?;
        }
        "pack" => {
            let req: Files = serde_json::from_slice(&read(input)?)?;
            let tmp = tempfile::NamedTempFile::new_in(fs_dir(local)?)?;
            local.pack(&req.item, &req.keys, tmp.path())?;
            std::io::copy(&mut std::fs::File::open(tmp.path())?, out)?;
        }
        "unpack" => {
            let name = item.context("sync-agent unpack takes the item's name")?;
            let tmp = tempfile::NamedTempFile::new_in(fs_dir(local)?)?;
            std::io::copy(input, &mut std::fs::File::create(tmp.path())?)?;
            local.unpack(name, tmp.path())?;
        }
        "secrets" => {
            let req: Files = serde_json::from_slice(&read(input)?)?;
            serde_json::to_writer(out, &local.secrets(&req.item, &req.keys)?)?;
        }
        "apply" => {
            let req: Apply = serde_json::from_slice(&read(input)?)?;
            local.apply(&req)?;
            out.write_all(b"{}")?;
        }
        other => bail!("sync-agent has no `{other}`"),
    }
    Ok(())
}

/// Where the agent keeps a tar on its way, on the same filesystem as the item's state.
fn fs_dir(local: &Local) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(&local.state)?;
    Ok(local.state.clone())
}

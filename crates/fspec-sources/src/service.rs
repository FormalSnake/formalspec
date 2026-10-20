//! Sources that name a version or commit and leave the artifact to their own tool.

use anyhow::{Result, anyhow, bail};
use fspec_core::lock::{Artifact, LockedPkg};
use fspec_core::runner::Cmd;
use serde_json::Value;

use crate::Cx;

pub fn flatpak(id: &str, remote: &str, cx: &Cx, out: &mut LockedPkg) -> Result<()> {
    let o = cx.runner.check(&Cmd::new("flatpak").args(["remote-info", "--system", remote, id]))?;
    let field = |key: &str| o.stdout.lines().find_map(|l| l.trim().strip_prefix(key)).map(|v| v.trim().to_string());
    let commit =
        field("Commit:").ok_or_else(|| anyhow!("`flatpak remote-info {remote} {id}` printed no Commit line"))?;
    out.version = field("Version:");
    out.artifacts.insert("any".into(), Artifact { commit: Some(commit), ..Default::default() });
    Ok(())
}

pub fn brew(name: &str, cask: bool, cx: &Cx, out: &mut LockedPkg) -> Result<()> {
    let url = format!("https://formulae.brew.sh/api/{}/{name}.json", if cask { "cask" } else { "formula" });
    let doc = cx.fetch.get_json(&url)?;
    let version = if cask { doc.get("version") } else { doc.pointer("/versions/stable") }.and_then(Value::as_str);
    out.version = Some(version.ok_or_else(|| anyhow!("{url} has no version"))?.to_string());
    Ok(())
}

pub fn nix(attr: &str, cx: &Cx, out: &mut LockedPkg) -> Result<()> {
    let o = cx.runner.check(&Cmd::new("nix").args(["eval", "--raw", &format!("nixpkgs#{attr}.version")]))?;
    let v = o.stdout.trim();
    if v.is_empty() {
        bail!("nixpkgs#{attr}.version evaluated to nothing");
    }
    out.version = Some(v.to_string());
    Ok(())
}

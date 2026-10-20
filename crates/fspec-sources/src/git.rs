use anyhow::{Result, bail};
use fspec_core::lock::LockedPkg;
use fspec_core::runner::Cmd;

use crate::Cx;

/// Pin `rev` (a branch, tag, `latest` for the newest version tag, or a full commit) to a commit.
pub fn resolve(url: &str, rev: &str, cx: &Cx, out: &mut LockedPkg) -> Result<()> {
    if rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()) {
        out.rev = Some(rev.to_lowercase());
        return Ok(());
    }
    if rev == "latest" {
        let o = cx.runner.check(&Cmd::new("git").args(["ls-remote", "--tags", "--sort=-v:refname", url]))?;
        let refs = parse(&o.stdout);
        let Some((_, first)) = refs.iter().find(|(_, r)| r.starts_with("refs/tags/")) else {
            bail!("{url} has no tags to take `latest` from");
        };
        let tag = first.trim_end_matches("^{}");
        let commit = refs
            .iter()
            .find(|(_, r)| r.strip_suffix("^{}") == Some(tag))
            .or_else(|| refs.iter().find(|(_, r)| *r == tag))
            .map(|(c, _)| c.clone());
        out.rev = commit;
        out.version = Some(tag.trim_start_matches("refs/tags/").to_string());
        return Ok(());
    }
    let o = cx.runner.check(&Cmd::new("git").args(["ls-remote", url, rev]))?;
    let refs = parse(&o.stdout);
    let wants =
        [format!("refs/heads/{rev}"), format!("refs/tags/{rev}^{{}}"), format!("refs/tags/{rev}"), rev.to_string()];
    let found = wants.iter().find_map(|w| refs.iter().find(|(_, r)| r == w)).map(|(c, _)| c.clone());
    match found {
        Some(c) => out.rev = Some(c),
        None => bail!("`git ls-remote {url} {rev}` found no matching ref"),
    }
    Ok(())
}

fn parse(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(c, r)| (c.trim().to_string(), r.trim().to_string()))
        .collect()
}

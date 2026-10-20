use anyhow::{Context, Result, anyhow, bail};
use fspec_core::lock::LockedPkg;
use serde::Deserialize;

use crate::Cx;
use crate::age::parse_time;
use crate::github::{Platform, artifact, match_asset, pick_release_tagged};

#[derive(Deserialize)]
struct Crate {
    #[serde(rename = "crate")]
    info: Info,
    versions: Vec<Version>,
}

#[derive(Deserialize)]
struct Info {
    max_version: String,
    repository: Option<String>,
}

#[derive(Deserialize)]
struct Version {
    num: String,
    created_at: String,
    #[serde(default)]
    yanked: bool,
}

pub fn resolve(name: &str, platforms: &[String], cutoff: Option<i64>, cx: &Cx, out: &mut LockedPkg) -> Result<()> {
    let url = format!("https://crates.io/api/v1/crates/{name}");
    let doc: Crate = serde_json::from_value(cx.fetch.get_json(&url)?).with_context(|| format!("parsing {url}"))?;
    let version = match cutoff {
        None => doc.info.max_version.clone(),
        Some(c) => doc
            .versions
            .iter()
            .filter(|v| !v.yanked && !v.num.contains('-'))
            .filter_map(|v| Some((parse_time(&v.created_at)?, &v.num)))
            .filter(|(t, _)| *t <= c)
            .max()
            .map(|(_, n)| n.clone())
            .ok_or_else(|| anyhow!("no version of {name} is old enough for the release_age policy"))?,
    };
    out.version = Some(version.clone());
    let none = |p: &str, why: String| anyhow!("no prebuilt for {p}: {name} {version} {why}");
    let repo = doc.info.repository.as_deref().and_then(github_repo);
    for p in platforms {
        let Some(repo) = &repo else {
            bail!(none(
                p,
                format!("has no GitHub repository ({})", doc.info.repository.as_deref().unwrap_or("none listed"))
            ));
        };
        let release = pick_release_tagged(repo, &version, cx).map_err(|e| none(p, format!("({e:#})")))?;
        let asset = match_asset(&release.assets, None, Platform::parse(p)?).map_err(|e| none(p, format!("({e:#})")))?;
        out.artifacts.insert(p.clone(), artifact(asset, cx)?);
    }
    Ok(())
}

fn github_repo(url: &str) -> Option<String> {
    let rest = url.split("github.com/").nth(1)?;
    let mut parts = rest.trim_end_matches('/').split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    Some(format!("{owner}/{}", name.trim_end_matches(".git")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn repo_urls() {
        assert_eq!(super::github_repo("https://github.com/BurntSushi/ripgrep").as_deref(), Some("BurntSushi/ripgrep"));
        assert_eq!(super::github_repo("https://github.com/a/b.git/").as_deref(), Some("a/b"));
        assert_eq!(super::github_repo("https://gitlab.com/a/b"), None);
    }
}

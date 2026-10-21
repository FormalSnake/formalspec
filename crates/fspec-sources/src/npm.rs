use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use fspec_core::lock::{Artifact, LockedPkg};
use serde::Deserialize;

use crate::Cx;
use crate::age::parse_time;
use crate::hash::sri;

#[derive(Deserialize)]
struct Packument {
    #[serde(rename = "dist-tags")]
    dist_tags: BTreeMap<String, String>,
    versions: BTreeMap<String, Version>,
    #[serde(default)]
    time: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Version {
    dist: Dist,
}

#[derive(Deserialize)]
struct Dist {
    tarball: String,
    #[serde(default)]
    integrity: Option<String>,
}

pub fn resolve(name: &str, pinned: Option<&str>, cutoff: Option<i64>, cx: &Cx, out: &mut LockedPkg) -> Result<()> {
    let url = format!("https://registry.npmjs.org/{name}");
    let doc: Packument = serde_json::from_value(cx.fetch.get_json(&url)?).with_context(|| format!("parsing {url}"))?;
    let version = match (pinned, cutoff) {
        (Some(v), _) => v.to_string(),
        (None, None) => {
            doc.dist_tags.get("latest").cloned().ok_or_else(|| anyhow!("{name} has no `latest` dist-tag"))?
        }
        (None, Some(c)) => doc
            .versions
            .keys()
            .filter(|v| !v.contains('-'))
            .filter_map(|v| Some((parse_time(doc.time.get(v)?)?, v)))
            .filter(|(t, _)| *t <= c)
            .max()
            .map(|(_, v)| v.clone())
            .ok_or_else(|| anyhow!("no version of {name} is old enough for the release_age policy"))?,
    };
    let Some(v) = doc.versions.get(&version) else { bail!("{name} lists {version} but has no such version") };
    let hash = match &v.dist.integrity {
        Some(i) => i.clone(),
        None => sri(&cx.fetch.get(&v.dist.tarball)?),
    };
    out.version = Some(version);
    out.artifacts
        .insert("any".into(), Artifact { url: Some(v.dist.tarball.clone()), hash: Some(hash), ..Default::default() });
    Ok(())
}

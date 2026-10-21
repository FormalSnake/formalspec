//! `pkg("python", version = "3.13")`: CPython from python-build-standalone, located through the
//! download metadata uv ships, which carries each build's URL and SHA-256.

use anyhow::{Context, Result, bail};
use fspec_core::lock::{Artifact, LockedPkg};
use serde_json::Value;

use crate::Cx;
use crate::hash::sri_from_digest;

const METADATA: &str =
    "https://raw.githubusercontent.com/astral-sh/uv/main/crates/uv-python-managed/download-metadata.json";

/// `aarch64-darwin` as the metadata's os, arch family and libc.
fn target(platform: &str) -> Result<(&'static str, &'static str, &'static str)> {
    Ok(match platform {
        "x86_64-linux" => ("linux", "x86_64", "gnu"),
        "aarch64-linux" => ("linux", "aarch64", "gnu"),
        "x86_64-darwin" => ("darwin", "x86_64", "none"),
        "aarch64-darwin" => ("darwin", "aarch64", "none"),
        other => bail!("no python build for {other}"),
    })
}

/// `3.13.9` of an entry.
fn version(e: &Value) -> Option<(u64, u64, u64)> {
    Some((e["major"].as_u64()?, e["minor"].as_u64()?, e["patch"].as_u64()?))
}

/// Whether `v` is the requested `3`, `3.13` or `3.13.9`.
fn matches(want: Option<&str>, v: (u64, u64, u64)) -> bool {
    let Some(want) = want else { return true };
    let have = [v.0, v.1, v.2];
    want.split('.').enumerate().all(|(i, part)| have.get(i).is_some_and(|h| part.parse() == Ok(*h)))
}

pub fn resolve(want: Option<&str>, platforms: &[String], cx: &Cx, out: &mut LockedPkg) -> Result<()> {
    let meta = cx.fetch.get_json(METADATA)?;
    let entries = meta.as_object().context("python download metadata is not an object")?;
    // The default build of a final release: no free-threaded or debug variant, no
    // microarchitecture level.
    let plain = |e: &Value| {
        e["name"] == "cpython" && e["variant"].is_null() && e["arch"]["variant"].is_null() && e["prerelease"] == ""
    };
    let newest = entries
        .values()
        .filter(|e| plain(e))
        .filter_map(version)
        .filter(|v| matches(want, *v))
        .max()
        .with_context(|| format!("no CPython release matches {}", want.unwrap_or("any version")))?;
    out.version = Some(format!("{}.{}.{}", newest.0, newest.1, newest.2));
    for platform in platforms {
        let (os, arch, libc) = target(platform)?;
        let e = entries
            .values()
            .find(|e| {
                plain(e)
                    && version(e) == Some(newest)
                    && e["os"] == os
                    && e["arch"]["family"] == arch
                    && e["libc"] == libc
            })
            .with_context(|| format!("no CPython {} build for {platform}", out.version.as_deref().unwrap_or("")))?;
        let url = e["url"].as_str().context("python build without a url")?;
        let sha = e["sha256"].as_str().context("python build without a sha256")?;
        out.artifacts.insert(
            platform.clone(),
            Artifact {
                url: Some(url.into()),
                hash: Some(sri_from_digest(&format!("sha256:{sha}"))?),
                bins: ["python3", "python", &format!("python{}.{}", newest.0, newest.1)]
                    .iter()
                    .map(|c| format!("{c}=python/bin/python3"))
                    .collect(),
                ..Default::default()
            },
        );
    }
    Ok(())
}

/// `url("https://...")`: one artifact for every platform, hashed from the download.
pub fn url(url: &str, cx: &Cx, out: &mut LockedPkg) -> Result<()> {
    let hash = crate::hash::sri(&cx.fetch.get(url)?);
    out.artifacts.insert("any".into(), Artifact { url: Some(url.into()), hash: Some(hash), ..Default::default() });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_prefixes_match_component_wise() {
        assert!(matches(Some("3.13"), (3, 13, 9)));
        assert!(!matches(Some("3.1"), (3, 13, 9)));
        assert!(matches(Some("3.13.9"), (3, 13, 9)));
        assert!(matches(None, (3, 14, 0)));
    }
}

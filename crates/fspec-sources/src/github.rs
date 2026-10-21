use anyhow::{Context, Result, anyhow, bail};
use fspec_core::lock::{Artifact, LockedPkg};
use serde::Deserialize;

use crate::Cx;
use crate::age::parse_time;
use crate::hash::hash_of;

#[derive(Deserialize, Clone, Debug)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    pub published_at: Option<String>,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    /// `sha256:<hex>`, absent on releases published before GitHub added digests.
    #[serde(default)]
    pub digest: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct Platform {
    pub goos: &'static str,
    pub goarch: &'static str,
}

impl Platform {
    pub fn parse(platform: &str) -> Result<Platform> {
        let goarch = match platform.split_once('-').map(|(a, _)| a) {
            Some("x86_64") => "amd64",
            Some("aarch64") => "arm64",
            _ => bail!("unknown platform `{platform}`"),
        };
        let goos = match platform.split_once('-').map(|(_, o)| o) {
            Some("linux") => "linux",
            Some("darwin") => "darwin",
            _ => bail!("unknown platform `{platform}`"),
        };
        Ok(Platform { goos, goarch })
    }
}

pub fn trim_v(tag: &str) -> &str {
    tag.strip_prefix('v').filter(|r| r.starts_with(|c: char| c.is_ascii_digit())).unwrap_or(tag)
}

/// The release to lock: the latest stable one, or the newest stable one published before `cutoff`.
/// `prefix` restricts monorepo tags (`cli-v1.2.3`).
pub fn pick_release(repo: &str, cutoff: Option<i64>, prefix: Option<&str>, cx: &Cx) -> Result<Release> {
    if cutoff.is_none() && prefix.is_none() {
        let url = format!("https://api.github.com/repos/{repo}/releases/latest");
        return serde_json::from_value(cx.fetch.get_json(&url)?).with_context(|| format!("parsing {url}"));
    }
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=100");
    let all: Vec<Release> =
        serde_json::from_value(cx.fetch.get_json(&url)?).with_context(|| format!("parsing {url}"))?;
    all.into_iter()
        .filter(|r| !r.draft && !r.prerelease)
        .filter(|r| prefix.is_none_or(|p| r.tag_name.starts_with(p)))
        .find(|r| match cutoff {
            Some(c) => r.published_at.as_deref().and_then(parse_time).is_some_and(|t| t <= c),
            None => true,
        })
        .ok_or_else(|| match cutoff {
            Some(_) => anyhow!("no stable release of {repo} is old enough for the release_age policy"),
            None => anyhow!("{repo} has no stable release"),
        })
}

/// The release a crate version was tagged as (`1.2.3`, `v1.2.3`, `name-v1.2.3`).
pub fn pick_release_tagged(repo: &str, version: &str, cx: &Cx) -> Result<Release> {
    for tag in [format!("v{version}"), version.to_string()] {
        let url = format!("https://api.github.com/repos/{repo}/releases/tags/{tag}");
        if let Ok(body) = cx.fetch.get_json(&url) {
            return serde_json::from_value(body).with_context(|| format!("parsing {url}"));
        }
    }
    bail!("{repo} has no release tagged v{version} or {version}")
}

pub fn resolve(
    repo: &str,
    glob: Option<&str>,
    version: Option<&str>,
    platforms: &[String],
    cutoff: Option<i64>,
    cx: &Cx,
    out: &mut LockedPkg,
) -> Result<()> {
    let release = match version {
        Some(v) => pick_release_tagged(repo, v, cx)?,
        None => pick_release(repo, cutoff, None, cx)?,
    };
    out.version = Some(trim_v(&release.tag_name).to_string());
    for p in platforms {
        let asset = match_asset(&release.assets, glob, Platform::parse(p)?)
            .with_context(|| format!("{repo} {} for {p}", release.tag_name))?;
        out.artifacts.insert(p.clone(), artifact(asset, cx)?);
    }
    Ok(())
}

pub fn artifact(asset: &Asset, cx: &Cx) -> Result<Artifact> {
    Ok(Artifact {
        url: Some(asset.browser_download_url.clone()),
        hash: Some(hash_of(&asset.browser_download_url, asset.digest.as_deref(), cx)?),
        ..Default::default()
    })
}

const SKIP_EXT: &[&str] = &[
    ".sha256", ".sha512", ".sha1", ".md5", ".sig", ".asc", ".pem", ".sbom", ".json", ".jsonl", ".txt", ".minisig",
    ".bundle", ".crt", ".pub", ".att", ".yml", ".yaml", ".sh", ".ps1", ".md",
];
const PACKAGE_EXT: &[&str] =
    &[".deb", ".rpm", ".apk", ".pkg", ".dmg", ".msi", ".appimage", ".snap", ".flatpak", ".exe"];
const OTHER_OS: &[&str] =
    &["windows", "win", "win32", "win64", "freebsd", "netbsd", "openbsd", "android", "ios", "illumos", "solaris"];
const OTHER_ARCH: &[&str] = &[
    "i386", "i686", "386", "x86", "armv5", "armv6", "armv7", "armhf", "arm", "ppc64", "ppc64le", "s390x", "riscv64",
    "mips", "mips64", "mipsle", "loong64", "wasm",
];

/// Lowercased name split into tokens, with the common arch spellings folded to `x8664` and `arm64`.
fn tokens(name: &str) -> Vec<String> {
    let n = name.to_lowercase().replace("x86_64", "x8664").replace("x86-64", "x8664");
    n.split(|c: char| !c.is_ascii_alphanumeric())
        .map(|t| match t {
            "amd64" | "x64" => "x8664".to_string(),
            "aarch64" => "arm64".to_string(),
            t => t.to_string(),
        })
        .filter(|t| !t.is_empty())
        .collect()
}

fn ext_score(name: &str) -> i32 {
    let n = name.to_lowercase();
    match () {
        _ if n.ends_with(".tar.gz") || n.ends_with(".tgz") => 8,
        _ if n.ends_with(".tar.xz") || n.ends_with(".txz") => 7,
        _ if n.ends_with(".tar.zst") => 6,
        _ if n.ends_with(".tar.bz2") || n.ends_with(".tbz") => 5,
        _ if n.ends_with(".zip") => 4,
        _ if !n.rsplit('/').next().unwrap_or("").contains('.') => 2,
        _ => 1,
    }
}

/// `*` and `?` over the whole name.
pub fn glob_match(glob: &str, name: &str) -> bool {
    fn go(p: &[u8], n: &[u8]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some((b'*', rest)) => (0..=n.len()).any(|i| go(rest, &n[i..])),
            Some((b'?', rest)) => !n.is_empty() && go(rest, &n[1..]),
            Some((c, rest)) => n.first() == Some(c) && go(rest, &n[1..]),
        }
    }
    go(glob.as_bytes(), name.as_bytes())
}

fn score(name: &str, p: Platform) -> Option<i32> {
    let lower = name.to_lowercase();
    if SKIP_EXT.iter().chain(PACKAGE_EXT).any(|e| lower.ends_with(e)) {
        return None;
    }
    let toks = tokens(name);
    let has = |words: &[&str]| toks.iter().any(|t| words.contains(&t.as_str()));
    let (want_os, want_arch, other_arch) = match (p.goos, p.goarch) {
        ("linux", "amd64") => (&["linux"][..], "x8664", "arm64"),
        ("linux", _) => (&["linux"][..], "arm64", "x8664"),
        (_, "amd64") => (&["darwin", "macos", "apple", "osx", "mac"][..], "x8664", "arm64"),
        _ => (&["darwin", "macos", "apple", "osx", "mac"][..], "arm64", "x8664"),
    };
    let os_match = toks.iter().any(|t| want_os.iter().any(|w| t == w || t.starts_with("linux") && *w == "linux"));
    if !os_match || has(OTHER_OS) {
        return None;
    }
    let universal = p.goos == "darwin" && has(&["universal", "universal2"]);
    let mut s = ext_score(name);
    if has(&[want_arch]) {
        s += 100;
        if has(&[other_arch]) {
            // Both architectures named: a fat or universal build.
            if !universal {
                return None;
            }
        }
    } else if universal {
        s += 50;
    } else if has(&[other_arch]) || has(OTHER_ARCH) || p.goarch != "amd64" {
        return None;
    }
    if p.goos == "linux" && has(&["musl", "static"]) {
        s += 20;
    }
    Some(s)
}

pub fn match_asset<'a>(assets: &'a [Asset], glob: Option<&str>, p: Platform) -> Result<&'a Asset> {
    let pool: Vec<&Asset> = assets.iter().filter(|a| glob.is_none_or(|g| glob_match(g, &a.name))).collect();
    if pool.is_empty() {
        let names: Vec<_> = assets.iter().map(|a| a.name.as_str()).collect();
        bail!("no asset matches glob `{}`; assets: {}", glob.unwrap_or("*"), names.join(", "));
    }
    let scored: Vec<(i32, &Asset)> = pool.iter().filter_map(|a| score(&a.name, p).map(|s| (s, *a))).collect();
    let Some(best) = scored.iter().map(|(s, _)| *s).max() else {
        if glob.is_some() && pool.len() == 1 {
            return Ok(pool[0]);
        }
        let names: Vec<_> = pool.iter().map(|a| a.name.as_str()).collect();
        bail!("no asset for {}/{}; candidates: {}", p.goos, p.goarch, names.join(", "));
    };
    let top: Vec<&Asset> = scored.iter().filter(|(s, _)| *s == best).map(|(_, a)| *a).collect();
    if top.len() > 1 {
        let names: Vec<_> = top.iter().map(|a| a.name.as_str()).collect();
        bail!(
            "{} assets match {}/{}, set `asset` to a glob that picks one: {}",
            top.len(),
            p.goos,
            p.goarch,
            names.join(", ")
        );
    }
    Ok(top[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assets(names: &[&str]) -> Vec<Asset> {
        names
            .iter()
            .map(|n| Asset { name: n.to_string(), browser_download_url: format!("https://x/{n}"), digest: None })
            .collect()
    }

    const FD: &[&str] = &[
        "fd-v10.2.0-aarch64-apple-darwin.tar.gz",
        "fd-v10.2.0-aarch64-unknown-linux-gnu.tar.gz",
        "fd-v10.2.0-aarch64-unknown-linux-musl.tar.gz",
        "fd-v10.2.0-x86_64-apple-darwin.tar.gz",
        "fd-v10.2.0-x86_64-pc-windows-msvc.zip",
        "fd-v10.2.0-x86_64-unknown-linux-gnu.tar.gz",
        "fd-v10.2.0-x86_64-unknown-linux-musl.tar.gz",
        "fd-v10.2.0-i686-unknown-linux-gnu.tar.gz",
        "fd-v10.2.0-arm-unknown-linux-gnueabihf.tar.gz",
        "fd_10.2.0_amd64.deb",
        "fd_10.2.0_arm64.deb",
        "fd-v10.2.0-x86_64-unknown-linux-musl.tar.gz.sha256",
    ];

    fn pick(names: &[&str], glob: Option<&str>, platform: &str) -> Result<String> {
        let a = assets(names);
        match_asset(&a, glob, Platform::parse(platform)?).map(|a| a.name.clone())
    }

    #[test]
    fn matches_each_platform() {
        assert_eq!(pick(FD, None, "x86_64-linux").unwrap(), "fd-v10.2.0-x86_64-unknown-linux-musl.tar.gz");
        assert_eq!(pick(FD, None, "aarch64-linux").unwrap(), "fd-v10.2.0-aarch64-unknown-linux-musl.tar.gz");
        assert_eq!(pick(FD, None, "aarch64-darwin").unwrap(), "fd-v10.2.0-aarch64-apple-darwin.tar.gz");
        assert_eq!(pick(FD, None, "x86_64-darwin").unwrap(), "fd-v10.2.0-x86_64-apple-darwin.tar.gz");
    }

    #[test]
    fn other_spellings() {
        let a =
            &["tool-linux-amd64.tar.gz", "tool-linux-arm64.tar.gz", "tool-macos-universal.zip", "tool-windows-x64.zip"];
        assert_eq!(pick(a, None, "x86_64-linux").unwrap(), "tool-linux-amd64.tar.gz");
        assert_eq!(pick(a, None, "aarch64-linux").unwrap(), "tool-linux-arm64.tar.gz");
        assert_eq!(pick(a, None, "aarch64-darwin").unwrap(), "tool-macos-universal.zip");
    }

    #[test]
    fn ambiguity_and_glob() {
        let a = &["t-linux-x86_64-a.tar.gz", "t-linux-x86_64-b.tar.gz"];
        let err = pick(a, None, "x86_64-linux").unwrap_err().to_string();
        assert!(err.contains("t-linux-x86_64-a.tar.gz") && err.contains("t-linux-x86_64-b.tar.gz"), "{err}");
        assert_eq!(pick(a, Some("*-b.tar.gz"), "x86_64-linux").unwrap(), "t-linux-x86_64-b.tar.gz");
    }

    #[test]
    fn no_match_lists_candidates() {
        let err = pick(&["t-linux-x86_64.tar.gz"], None, "aarch64-darwin").unwrap_err().to_string();
        assert!(err.contains("t-linux-x86_64.tar.gz"), "{err}");
    }
}

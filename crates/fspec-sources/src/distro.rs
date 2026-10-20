//! Pins for `dnf`, `pacman`, `apt` and `xbps` packages: the exact package file per platform.
//! The machine running `fspec update` asks its own package manager when it is the right distro
//! and arch. Otherwise Arch and Void repo metadata is read over HTTP; Fedora and Debian have no
//! metadata simple enough to parse here, so those platforms stay name-only.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use anyhow::{Context, Result, anyhow, bail};
use fspec_core::lock::{Artifact, LockedPkg};
use fspec_core::resource::{DistroPkgs, Source};
use fspec_core::runner::Cmd;

use crate::Cx;
use crate::hash::{sri, sri_from_digest};

const ARCH_MIRROR: &str = "https://geo.mirror.pkgbuild.com";
const ARCH_REPOS: [&str; 2] = ["core", "extra"];
const VOID_MIRROR: &str = "https://repo-default.voidlinux.org/current";

#[derive(Clone, Copy, PartialEq)]
enum Family {
    Dnf,
    Pacman,
    Apt,
    Xbps,
}

impl Family {
    fn of(source: &Source) -> Option<(Family, &DistroPkgs)> {
        match source {
            Source::Dnf(d) => Some((Family::Dnf, d)),
            Source::Pacman(d) => Some((Family::Pacman, d)),
            Source::Apt(d) => Some((Family::Apt, d)),
            Source::Xbps(d) => Some((Family::Xbps, d)),
            _ => None,
        }
    }

    fn runs_on(self, distro: &str) -> bool {
        match self {
            Family::Dnf => distro == "fedora",
            Family::Pacman => distro == "arch",
            Family::Apt => matches!(distro, "debian" | "ubuntu"),
            Family::Xbps => distro == "void",
        }
    }
}

/// One repo package: its version, file URL and hash when the source already knows it.
struct Found {
    version: String,
    url: String,
    /// `sha256-<base64>`; the file is downloaded and hashed when the source has none.
    hash: Option<String>,
}

pub fn resolve(source: &Source, name: &str, platforms: &[String], cx: &Cx, out: &mut LockedPkg) -> Result<()> {
    let Some((fam, d)) = Family::of(source) else { bail!("{source} is not a distro source") };
    let names = if d.names.is_empty() { vec![name.to_string()] } else { d.names.clone() };
    // The lock holds one artifact per package, so a multi-name package installs by name.
    let [pkg] = names.as_slice() else { return Ok(()) };
    for platform in platforms {
        let Some((arch, "linux")) = platform.split_once('-') else { continue };
        let found = if cx.local.0 == *platform && fam.runs_on(&cx.local.1) {
            Some(query(fam, pkg, cx)?)
        } else {
            match fam {
                Family::Pacman => arch_db(pkg, arch, cx)?,
                Family::Xbps => void_index(pkg, arch, cx)?,
                Family::Dnf | Family::Apt => None,
            }
        };
        let Some(f) = found else { continue };
        let hash = match f.hash {
            Some(h) => h,
            None => sri(&cx.fetch.get(&f.url)?),
        };
        out.version.get_or_insert(f.version);
        out.artifacts.insert(platform.clone(), Artifact { url: Some(f.url), hash: Some(hash), ..Default::default() });
    }
    Ok(())
}

fn query(fam: Family, name: &str, cx: &Cx) -> Result<Found> {
    match fam {
        Family::Dnf => dnf_query(name, cx),
        Family::Pacman => pacman_query(name, cx),
        Family::Apt => apt_query(name, cx),
        Family::Xbps => xbps_query(name, cx),
    }
}

fn dnf_query(name: &str, cx: &Cx) -> Result<Found> {
    let o = cx.runner.check(&Cmd::new("dnf").args(["repoquery", "--latest-limit=1", "--location"]).arg(name))?;
    let url = o
        .stdout
        .lines()
        .map(str::trim)
        .rfind(|l| l.ends_with(".rpm"))
        .ok_or_else(|| anyhow!("`dnf repoquery --location {name}` printed no .rpm URL"))?;
    let file = url.rsplit('/').next().unwrap_or(url).trim_end_matches(".rpm");
    let nevr = file.rsplit_once('.').map_or(file, |(n, _arch)| n);
    let version = nevr.strip_prefix(&format!("{name}-")).unwrap_or(nevr);
    Ok(Found { version: version.into(), url: url.into(), hash: None })
}

fn pacman_query(name: &str, cx: &Cx) -> Result<Found> {
    let o = cx.runner.check(&Cmd::new("pacman").args(["-Sddp", "--print-format", "%n|%v|%l"]).arg(name))?;
    o.stdout
        .lines()
        .filter_map(|l| l.trim().splitn(3, '|').collect::<Vec<_>>().try_into().ok())
        .find_map(|[n, v, url]: [&str; 3]| {
            (n == name).then(|| Found { version: v.into(), url: url.into(), hash: None })
        })
        .ok_or_else(|| anyhow!("`pacman -Sddp {name}` printed no package line"))
}

/// `'URI' filename size SHA256:hex`, the format of `apt-get download --print-uris`.
fn apt_query(name: &str, cx: &Cx) -> Result<Found> {
    let o = cx.runner.check(&Cmd::new("apt-get").args(["download", "--print-uris"]).arg(name))?;
    let line = o
        .stdout
        .lines()
        .find(|l| l.starts_with('\''))
        .ok_or_else(|| anyhow!("`apt-get download --print-uris {name}` printed no URI"))?;
    let mut parts = line.split_whitespace();
    let url = parts.next().unwrap_or_default().trim_matches('\'').to_string();
    let file = parts.next().unwrap_or_default();
    let hash =
        parts.find_map(|p| p.strip_prefix("SHA256:")).map(|h| sri_from_digest(&format!("sha256:{h}"))).transpose()?;
    let version =
        file.split('_').nth(1).ok_or_else(|| anyhow!("cannot read a version from {file}"))?.replace("%3a", ":");
    Ok(Found { version, url, hash })
}

/// `key: value` lines, the format of `xbps-query -R --property=a,b,c`.
fn xbps_query(name: &str, cx: &Cx) -> Result<Found> {
    let o = cx.runner.check(
        &Cmd::new("xbps-query").args(["-R", "--property=pkgver,architecture,repository,filename-sha256"]).arg(name),
    )?;
    let field =
        |key: &str| o.stdout.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(':')).map(|v| v.trim().to_string());
    let missing = |k: &str| anyhow!("`xbps-query -R {name}` printed no {k}");
    let pkgver = field("pkgver").ok_or_else(|| missing("pkgver"))?;
    let arch = field("architecture").ok_or_else(|| missing("architecture"))?;
    let repo = field("repository").ok_or_else(|| missing("repository"))?;
    let hash = field("filename-sha256").map(|h| sri_from_digest(&format!("sha256:{h}"))).transpose()?;
    let version = pkgver.strip_prefix(&format!("{name}-")).unwrap_or(&pkgver).to_string();
    Ok(Found { url: format!("{}/{pkgver}.{arch}.xbps", repo.trim_end_matches('/')), version, hash })
}

/// Unwraps gzip or zstd; anything else is returned as is.
fn decompress(bytes: Vec<u8>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if bytes.starts_with(&[0x1f, 0x8b]) {
        flate2::read::GzDecoder::new(&bytes[..]).read_to_end(&mut out).context("unpacking gzip")?;
    } else if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        let mut dec =
            ruzstd::decoding::StreamingDecoder::new(&bytes[..]).map_err(|e| anyhow!("unpacking zstd: {e}"))?;
        dec.read_to_end(&mut out).context("unpacking zstd")?;
    } else {
        return Ok(bytes);
    }
    Ok(out)
}

/// Files of a tarball by path, for the entries `keep` accepts.
fn tar_files(bytes: Vec<u8>, keep: impl Fn(&str) -> bool) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    let mut archive = tar::Archive::new(Cursor::new(decompress(bytes)?));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().trim_start_matches("./").to_string();
        if keep(&path) {
            let mut body = Vec::new();
            entry.read_to_end(&mut body)?;
            files.insert(path, body);
        }
    }
    Ok(files)
}

/// The sync database of an Arch repo: one `<name>-<version>/desc` per package.
fn arch_db(name: &str, arch: &str, cx: &Cx) -> Result<Option<Found>> {
    // Official mirrors carry x86_64 only; Arch Linux ARM has its own mirrors and keys.
    if arch != "x86_64" {
        return Ok(None);
    }
    for repo in ARCH_REPOS {
        let dir = format!("{ARCH_MIRROR}/{repo}/os/{arch}");
        let url = format!("{dir}/{repo}.db");
        let descs =
            tar_files(cx.fetch.get(&url)?, |p| p.ends_with("/desc")).with_context(|| format!("reading {url}"))?;
        for body in descs.values() {
            let desc = pacman_desc(&String::from_utf8_lossy(body));
            let get = |k: &str| desc.get(k).map(String::as_str);
            if get("NAME") != Some(name) {
                continue;
            }
            let (Some(version), Some(file), Some(sha)) = (get("VERSION"), get("FILENAME"), get("SHA256SUM")) else {
                bail!("{url} lists {name} without a version, file name or checksum");
            };
            let hash = Some(sri_from_digest(&format!("sha256:{sha}"))?);
            return Ok(Some(Found { version: version.into(), url: format!("{dir}/{file}"), hash }));
        }
    }
    bail!("{name} is in neither the core nor the extra repo of {ARCH_MIRROR}")
}

/// `%KEY%` headers followed by their first value line.
fn pacman_desc(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut lines = text.lines();
    while let Some(l) = lines.next() {
        if let Some(key) = l.strip_prefix('%').and_then(|l| l.strip_suffix('%'))
            && let Some(v) = lines.next()
        {
            map.insert(key.into(), v.trim().into());
        }
    }
    map
}

/// `<arch>-repodata`: a tarball whose `index.plist` maps package names to `pkgver` and
/// `filename-sha256`.
fn void_index(name: &str, arch: &str, cx: &Cx) -> Result<Option<Found>> {
    let dir = if arch == "x86_64" { VOID_MIRROR.to_string() } else { format!("{VOID_MIRROR}/{arch}") };
    let url = format!("{dir}/{arch}-repodata");
    let mut files = tar_files(cx.fetch.get(&url)?, |p| p == "index.plist").with_context(|| format!("reading {url}"))?;
    let index = files.remove("index.plist").ok_or_else(|| anyhow!("{url} holds no index.plist"))?;
    let index = plist::Value::from_reader(Cursor::new(index)).with_context(|| format!("parsing {url}"))?;
    let entry = index
        .as_dictionary()
        .and_then(|d| d.get(name))
        .and_then(plist::Value::as_dictionary)
        .ok_or_else(|| anyhow!("{name} is not in {url}"))?;
    let text = |k: &str| entry.get(k).and_then(plist::Value::as_string);
    let (Some(pkgver), Some(sha)) = (text("pkgver"), text("filename-sha256")) else {
        bail!("{url} lists {name} without pkgver or filename-sha256");
    };
    let pkg_arch = text("architecture").unwrap_or(arch);
    let version = pkgver.strip_prefix(&format!("{name}-")).unwrap_or(pkgver);
    let hash = Some(sri_from_digest(&format!("sha256:{sha}"))?);
    Ok(Some(Found { version: version.into(), url: format!("{dir}/{pkgver}.{pkg_arch}.xbps"), hash }))
}

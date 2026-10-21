//! `lib(...)` and `bin(...)`: which package provides each, asked of every package manager at
//! `fspec update` and kept in the lock, so activation only checks that it is installed.
//!
//! dnf and moss install a capability directly (`pkgconfig(gtk4)`, `/usr/bin/cmake`,
//! `binary(cmake)`), and zypper does for pkg-config modules, so those lock the capability. apt,
//! pacman, xbps and zypper's executables are looked up in their file indexes inside a container
//! of that distro, and brew through its API and the bottle's contents.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fspec_core::dev::{Dev, Native};
use fspec_core::fetch::Fetch;
use fspec_core::lock::{Lock, LockedNative};
use fspec_core::runner::{Cmd, Runner};

/// The managers a Linux dependency resolves for; macOS uses brew.
const LINUX: [&str; 6] = ["apt", "dnf", "zypper", "pacman", "xbps", "moss"];

/// The image a manager's file index is searched in, and the platform to run it as.
fn query_image(manager: &str) -> Option<(&'static str, Option<&'static str>)> {
    match manager {
        "apt" => Some(("docker.io/library/debian:stable", None)),
        "pacman" => Some(("docker.io/library/archlinux:latest", Some("linux/amd64"))),
        "xbps" => Some(("ghcr.io/void-linux/void-glibc-full:latest", None)),
        "zypper" => Some(("docker.io/opensuse/tumbleweed:latest", None)),
        _ => None,
    }
}

/// The package manager of a Linux system, from its os-release.
pub fn manager_of_os_release(text: &str) -> Option<&'static str> {
    let field = |k: &str| {
        text.lines().find_map(|l| l.strip_prefix(k)).map(|v| v.trim_matches('"').to_string()).unwrap_or_default()
    };
    let ids = format!("{} {}", field("ID="), field("ID_LIKE="));
    ids.split_whitespace().find_map(|id| match id {
        "debian" | "ubuntu" => Some("apt"),
        "fedora" | "rhel" | "centos" => Some("dnf"),
        "suse" | "opensuse" | "sles" => Some("zypper"),
        i if i.starts_with("opensuse") => Some("zypper"),
        "arch" => Some("pacman"),
        "void" => Some("xbps"),
        "aerynos" => Some("moss"),
        _ => None,
    })
}

/// The package manager inside an image, from its name (`debian:13`, `fedora:43`).
pub fn manager_of_image(reference: &str) -> Option<&'static str> {
    let name = reference.split(['@', ':']).next().unwrap_or(reference);
    let last = name.rsplit('/').next().unwrap_or(name);
    match last {
        "debian" | "ubuntu" => Some("apt"),
        "fedora" | "centos" | "almalinux" | "rockylinux" => Some("dnf"),
        "archlinux" => Some("pacman"),
        "tumbleweed" | "leap" => Some("zypper"),
        l if l.starts_with("void") => Some("xbps"),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Kind {
    Lib,
    Bin,
}

impl Kind {
    fn key(self) -> &'static str {
        match self {
            Kind::Lib => "lib",
            Kind::Bin => "bin",
        }
    }

    fn what(self, name: &str) -> String {
        match self {
            Kind::Lib => format!("{name}.pc"),
            Kind::Bin => format!("an executable `{name}`"),
        }
    }
}

/// One `lib(...)` or `bin(...)` and the managers it resolves for.
struct Want {
    kind: Kind,
    native: Native,
    managers: BTreeSet<&'static str>,
}

fn wants(devs: &BTreeMap<String, Dev>) -> Result<Vec<Want>> {
    let mut out: BTreeMap<(Kind, String), Want> = BTreeMap::new();
    for (platform, dev) in devs {
        let managers: &[&'static str] = if platform.ends_with("-darwin") { &["brew"] } else { &LINUX };
        let natives = dev.libs.iter().map(|n| (Kind::Lib, n)).chain(dev.bins.iter().map(|n| (Kind::Bin, n)));
        for (kind, n) in natives {
            let w = out.entry((kind, n.name.clone())).or_insert_with(|| Want {
                kind,
                native: n.clone(),
                managers: BTreeSet::new(),
            });
            if w.native != *n {
                bail!("{}(\"{}\") is declared differently on {platform}", kind.key(), n.name);
            }
            w.managers.extend(managers);
        }
    }
    Ok(out.into_values().collect())
}

/// What a resolution needs: a container runtime for the file-index lookups and HTTP for brew.
pub struct Resolver<'a> {
    pub runner: &'a dyn Runner,
    pub fetch: &'a dyn Fetch,
    /// `docker` or `podman`, when one is installed.
    pub runtime: Option<String>,
}

/// Per package manager: the package found, and why none was.
type Found = (BTreeMap<String, String>, BTreeMap<String, String>);

pub struct Resolved {
    pub native: Vec<LockedNative>,
    pub changes: Vec<String>,
    /// One line per manager a dependency has no package for, with why.
    pub misses: Vec<String>,
}

/// Resolve every `lib(...)` and `bin(...)` the project declares on any platform.
pub fn resolve(devs: &BTreeMap<String, Dev>, old: &Lock, r: &Resolver) -> Result<Resolved> {
    let wants = wants(devs)?;
    let mut found: BTreeMap<(Kind, String), Found> = BTreeMap::new();
    let mut by_manager: BTreeMap<&'static str, Vec<&Want>> = BTreeMap::new();
    for w in &wants {
        let slot = found.entry((w.kind, w.native.name.clone())).or_default();
        for m in &w.managers {
            match w.native.overrides.get(*m) {
                Some(pkg) => {
                    slot.0.insert(m.to_string(), pkg.clone());
                }
                None => by_manager.entry(m).or_default().push(w),
            }
        }
    }
    for (manager, ws) in by_manager {
        let answers: Vec<Result<String, String>> = match manager {
            "dnf" => ws.iter().map(|w| Ok(capability(manager, w))).collect(),
            "moss" => ws.iter().map(|w| Ok(capability(manager, w))).collect(),
            "zypper" => {
                let (libs, bins): (Vec<&Want>, Vec<&Want>) = ws.iter().partition(|w| w.kind == Kind::Lib);
                let mut a: BTreeMap<String, Result<String, String>> = BTreeMap::new();
                for w in libs {
                    a.insert(id(w), Ok(capability(manager, w)));
                }
                for (w, ans) in bins.iter().zip(search(r, manager, &bins)) {
                    a.insert(id(w), ans);
                }
                ws.iter().map(|w| a.remove(&id(w)).unwrap_or_else(|| Err("not looked up".into()))).collect()
            }
            "brew" => brew(r.fetch, &ws),
            _ => search(r, manager, &ws),
        };
        for (w, answer) in ws.iter().zip(answers) {
            let slot = found.entry((w.kind, w.native.name.clone())).or_default();
            match answer {
                Ok(pkg) => slot.0.insert(manager.to_string(), pkg),
                Err(why) => slot.1.insert(manager.to_string(), why),
            };
        }
    }

    let (mut changes, mut misses) = (Vec::new(), Vec::new());
    let mut native = Vec::new();
    for ((kind, name), (provides, miss)) in found {
        let entry = LockedNative { kind: kind.key().into(), name: name.clone(), miss, provides };
        let label = format!("{} {name}", kind.key());
        match old.native(kind.key(), &name) {
            None => changes.push(format!("+ {label}")),
            Some(b) => {
                for (m, pkg) in &entry.provides {
                    match b.provides.get(m) {
                        Some(was) if was != pkg => changes.push(format!("{label} {m} {was} -> {pkg}")),
                        None => changes.push(format!("{label} {m} {pkg}")),
                        Some(_) => {}
                    }
                }
            }
        }
        for (m, why) in &entry.miss {
            misses.push(format!(
                "{label}: no {m} package, {why}; name it with {}(\"{name}\", {m} = \"...\")",
                kind.key()
            ));
        }
        native.push(entry);
    }
    for gone in &old.native {
        if !native.iter().any(|n| n.kind == gone.kind && n.name == gone.name) {
            changes.push(format!("- {} {}", gone.kind, gone.name));
        }
    }
    Ok(Resolved { native, changes, misses })
}

fn id(w: &Want) -> String {
    format!("{}:{}", w.kind.key(), w.native.name)
}

/// The capability dnf, zypper and moss install by: they map it to a package themselves.
fn capability(manager: &str, w: &Want) -> String {
    let n = &w.native.name;
    match (manager, w.kind) {
        (_, Kind::Lib) => format!("pkgconfig({n})"),
        ("moss", Kind::Bin) => format!("binary({n})"),
        (_, Kind::Bin) => format!("/usr/bin/{n}"),
    }
}

/// `.`, `+` and the like are literal in pkg-config module names.
fn regex_escape(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_string() } else { format!("\\{c}") })
        .collect()
}

fn setup(manager: &str) -> &'static str {
    match manager {
        "apt" => {
            "export DEBIAN_FRONTEND=noninteractive; apt-get update -qq >/dev/null && apt-get install -y -qq apt-file \
             >/dev/null && apt-file update >/dev/null"
        }
        // pacman's download sandbox needs syscalls container runtimes filter out.
        "pacman" => "sed -i 's/^#DisableSandbox/DisableSandbox/' /etc/pacman.conf && pacman -Fy >/dev/null",
        "xbps" => {
            "xbps-install -Syu xbps >/dev/null 2>&1; xbps-install -Sy xtools git >/dev/null && xlocate -S >/dev/null 2>&1"
        }
        "zypper" => "zypper -n -q refresh >/dev/null",
        _ => "true",
    }
}

fn query(manager: &str, w: &Want) -> String {
    let n = &w.native.name;
    let e = regex_escape(n);
    match (manager, w.kind) {
        ("apt", Kind::Lib) => format!("apt-file search -x '/pkgconfig/{e}\\.pc$'"),
        ("apt", Kind::Bin) => format!("apt-file search -x '^/(usr/)?s?bin/{e}$'"),
        ("pacman", Kind::Lib) => format!("pacman -F --machinereadable '{n}.pc'"),
        ("pacman", Kind::Bin) => format!("pacman -F --machinereadable '{n}'"),
        ("xbps", Kind::Lib) => format!("xlocate '/pkgconfig/{n}.pc'"),
        ("xbps", Kind::Bin) => format!("xlocate '/usr/bin/{n}'"),
        ("zypper", _) => format!("zypper -n -q se --file-list --match-exact '/usr/bin/{n}'"),
        _ => "true".into(),
    }
}

/// Look every dependency up in one container run of `manager`'s distro.
fn search(r: &Resolver, manager: &str, ws: &[&Want]) -> Vec<Result<String, String>> {
    let fail = |why: String| ws.iter().map(|_| Err(why.clone())).collect();
    let Some((image, platform)) = query_image(manager) else { return fail(format!("{manager} has no lookup")) };
    let Some(runtime) = &r.runtime else { return fail("looking it up needs docker or podman".into()) };
    let mut script = format!("{} || exit 3\n", setup(manager));
    for (i, w) in ws.iter().enumerate() {
        script.push_str(&format!("echo '@@{i}'; {} 2>/dev/null\n", query(manager, w)));
    }
    let mut cmd = Cmd::new(runtime.as_str()).args(["run", "--rm", "-i"]);
    if let Some(p) = platform {
        cmd = cmd.args(["--platform", p]);
    }
    let out = match r.runner.run(&cmd.args([image, "sh", "-s"]).stdin(script)) {
        Ok(o) if o.status == 0 => o.stdout,
        Ok(o) => return fail(format!("the {image} lookup failed: {}", o.stderr.trim().lines().last().unwrap_or(""))),
        Err(e) => return fail(format!("the {image} lookup failed: {e:#}")),
    };
    let mut sections: BTreeMap<usize, Vec<&str>> = BTreeMap::new();
    let mut at = None;
    for line in out.lines() {
        match line.strip_prefix("@@").and_then(|i| i.parse().ok()) {
            Some(i) => at = Some(i),
            None => {
                if let Some(i) = at {
                    sections.entry(i).or_default().push(line);
                }
            }
        }
    }
    ws.iter()
        .enumerate()
        .map(|(i, w)| {
            let lines = sections.remove(&i).unwrap_or_default();
            pick(manager, w, &candidates(manager, w, &lines))
        })
        .collect()
}

/// Package names in one lookup's output that ship exactly the file asked for.
fn candidates(manager: &str, w: &Want, lines: &[&str]) -> Vec<String> {
    let n = &w.native.name;
    let file_ok = |path: &str| match w.kind {
        // 32-bit multilib copies are never what a project builds against.
        Kind::Lib => path.ends_with(&format!("/pkgconfig/{n}.pc")) && !path.contains("/lib32/"),
        Kind::Bin => {
            let p = path.trim_start_matches('/');
            ["usr/bin/", "bin/", "usr/sbin/", "sbin/"].iter().any(|d| p.strip_prefix(d) == Some(n.as_str()))
        }
    };
    let mut out = BTreeSet::new();
    for l in lines {
        let found = match manager {
            "apt" => l.split_once(": ").filter(|(_, p)| file_ok(p)).map(|(pkg, _)| pkg.to_string()),
            "pacman" => {
                let f: Vec<&str> = l.split('\0').collect();
                (f.len() >= 4 && file_ok(f[3])).then(|| f[1].to_string())
            }
            // xbps-alternatives ships an executable as `/usr/bin/<name>.<package>` and links it.
            "xbps" => l.split_once(char::is_whitespace).and_then(|(pkgver, p)| {
                let (pkg, p) = (xbps_name(pkgver), p.trim());
                let alternative = w.kind == Kind::Bin && p == format!("/usr/bin/{n}.{pkg}");
                (file_ok(p) || alternative).then_some(pkg)
            }),
            "zypper" => {
                let cols: Vec<&str> = l.split('|').map(str::trim).collect();
                (cols.len() >= 4 && cols[3] == "package").then(|| cols[1].to_string())
            }
            _ => None,
        };
        out.extend(found);
    }
    out.into_iter().collect()
}

/// `gtk4-devel-4.18.6_1` is package `gtk4-devel`.
fn xbps_name(pkgver: &str) -> String {
    pkgver.rsplit_once('-').map_or(pkgver, |(n, _)| n).to_string()
}

fn pick(manager: &str, w: &Want, found: &[String]) -> Result<String, String> {
    let n = &w.native.name;
    match found {
        [] => Err(format!("its file index has nothing shipping {}", w.kind.what(n))),
        [one] => Ok(one.clone()),
        many => {
            let preferred: Vec<&String> = match w.kind {
                Kind::Bin => many.iter().filter(|p| *p == n).collect(),
                Kind::Lib => many.iter().filter(|p| p.ends_with("-dev") || p.ends_with("-devel")).collect(),
            };
            match preferred.as_slice() {
                [one] => Ok((*one).clone()),
                _ => Err(format!("{manager} has several packages shipping {}: {}", w.kind.what(n), many.join(", "))),
            }
        }
    }
}

const BREW_API: &str = "https://formulae.brew.sh/api/formula.json";

/// brew's API lists each formula's executables but no other files, so a pkg-config module is
/// matched to formulae by name and confirmed against the bottle's contents.
fn brew(fetch: &dyn Fetch, ws: &[&Want]) -> Vec<Result<String, String>> {
    let formulae = match fetch.get_json(BREW_API) {
        Ok(serde_json::Value::Array(f)) => f,
        Ok(_) => return ws.iter().map(|_| Err(format!("{BREW_API} is not a list"))).collect(),
        Err(e) => return ws.iter().map(|_| Err(format!("reading {BREW_API}: {e:#}"))).collect(),
    };
    let name_of = |f: &serde_json::Value| f["name"].as_str().unwrap_or_default().to_string();
    ws.iter()
        .map(|w| {
            let n = &w.native.name;
            match w.kind {
                Kind::Bin => {
                    let found: Vec<String> = formulae
                        .iter()
                        .filter(|f| f["executables"].as_array().is_some_and(|e| e.iter().any(|x| x == n.as_str())))
                        .map(name_of)
                        .collect();
                    pick("brew", w, &found)
                }
                Kind::Lib => {
                    let guesses = brew_guesses(n);
                    let mut found = Vec::new();
                    for f in formulae.iter().filter(|f| {
                        let aliases = f["aliases"].as_array().cloned().unwrap_or_default();
                        guesses.iter().any(|g| name_of(f) == *g || aliases.iter().any(|a| a == g.as_str()))
                    }) {
                        match bottle_ships(fetch, f, &format!("/lib/pkgconfig/{n}.pc")) {
                            Ok(true) => found.push(name_of(f)),
                            Ok(false) => {}
                            Err(e) => return Err(format!("checking the {} bottle: {e:#}", name_of(f))),
                        }
                    }
                    if found.is_empty() {
                        return Err(format!(
                            "no formula named {} ships {n}.pc (the API lists no files, so only those names are checked)",
                            guesses.join(" or ")
                        ));
                    }
                    pick("brew", w, &found)
                }
            }
        })
        .collect()
}

/// Formula names a module may come from: `gtk4` itself, `dbus-1` as `dbus`, `gtk+-3.0` as
/// `gtk+3`, `libssl` as `openssl`'s alias.
fn brew_guesses(module: &str) -> Vec<String> {
    let mut out = vec![module.to_string()];
    if let Some((stem, ver)) = module.rsplit_once('-')
        && ver.chars().all(|c| c.is_ascii_digit() || c == '.')
        && !ver.is_empty()
    {
        out.push(stem.to_string());
        out.push(format!("{stem}{}", ver.split('.').next().unwrap_or_default()));
    }
    if let Some(rest) = module.strip_prefix("lib") {
        out.push(rest.to_string());
    }
    out.dedup();
    out
}

fn bottle_ships(fetch: &dyn Fetch, formula: &serde_json::Value, suffix: &str) -> Result<bool> {
    let files = formula["bottle"]["stable"]["files"].as_object().context("the formula has no bottles")?;
    let file = files
        .iter()
        .find(|(k, _)| k.starts_with("arm64_") && !k.ends_with("linux"))
        .or_else(|| files.iter().next())
        .map(|(_, v)| v)
        .context("the formula has no bottles")?;
    let url = file["url"].as_str().context("bottle without a url")?;
    let repo = url.split("/v2/").nth(1).and_then(|r| r.split("/blobs/").next()).context("bottle url")?;
    let token_url = format!("https://ghcr.io/token?scope=repository:{repo}:pull");
    let token = fetch.get_json(&token_url)?;
    let token = token["token"].as_str().with_context(|| format!("{token_url} returned no token"))?;
    let body = fetch.request(url, &[("Authorization", &format!("Bearer {token}"))])?.body;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(&body[..]));
    for entry in tar.entries()? {
        let entry = entry?;
        if entry.path()?.to_string_lossy().ends_with(suffix) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The locked package for each dependency on `manager`, or why there is none.
pub fn locked_for<'a>(dev: &'a Dev, lock: &'a Lock, manager: &str) -> Vec<(String, Result<String, String>)> {
    let natives = dev.libs.iter().map(|n| (Kind::Lib, n)).chain(dev.bins.iter().map(|n| (Kind::Bin, n)));
    natives
        .map(|(kind, n)| {
            let label = format!("{}(\"{}\")", kind.key(), n.name);
            let answer = match lock.native(kind.key(), &n.name) {
                None => Err(format!("{label} is not in fspec.lock, run fspec update")),
                Some(l) => match (l.provides.get(manager), l.miss.get(manager)) {
                    (Some(p), _) => Ok(p.clone()),
                    (None, Some(why)) => Err(format!(
                        "{label} has no {manager} package: {why}; name it with {}(\"{}\", {manager} = \"...\")",
                        kind.key(),
                        n.name
                    )),
                    (None, None) => Err(format!("{label} was not resolved for {manager}, run fspec update")),
                },
            };
            (label, answer)
        })
        .collect()
}

/// What a missing dependency is installed with on this machine.
pub fn install_hint(manager: &str, pkgs: &[String]) -> String {
    let quoted: Vec<String> =
        pkgs.iter().map(|p| if p.contains(['(', ')']) { format!("'{p}'") } else { p.clone() }).collect();
    let cmd = match manager {
        "apt" => "sudo apt-get install",
        "dnf" => "sudo dnf install",
        "zypper" => "sudo zypper install",
        "pacman" => "sudo pacman -S",
        "xbps" => "sudo xbps-install",
        "moss" => "sudo moss install",
        _ => "brew install",
    };
    format!("{cmd} {}", quoted.join(" "))
}

/// The same install, non-interactive, for a container image's `RUN`.
pub fn image_install(manager: &str, pkgs: &[String]) -> Result<String> {
    let quoted: Vec<String> = pkgs.iter().map(|p| format!("'{p}'")).collect();
    let list = quoted.join(" ");
    Ok(match manager {
        "apt" => format!(
            "apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y {list} && rm -rf /var/lib/apt/lists/*"
        ),
        "dnf" => format!("dnf install -y {list} && dnf clean all"),
        "zypper" => format!("zypper -n install {list} && zypper clean -a"),
        "pacman" => format!(
            "sed -i 's/^#DisableSandbox/DisableSandbox/' /etc/pacman.conf && pacman -Syu --noconfirm {list} && pacman -Scc --noconfirm"
        ),
        "xbps" => format!("xbps-install -Syu xbps && xbps-install -Sy {list}"),
        other => bail!("installing into an image is not supported for {other}"),
    })
}

/// Where Homebrew lives.
pub fn brew_prefix() -> PathBuf {
    match std::env::var_os("HOMEBREW_PREFIX") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ if cfg!(target_arch = "aarch64") => PathBuf::from("/opt/homebrew"),
        _ => PathBuf::from("/usr/local"),
    }
}

/// Whether the dependency is on this Linux system: the module's `.pc` in a pkg-config
/// directory, or the executable in a system bin directory.
pub fn present_on_linux(root: &Path, kind_lib: bool, name: &str) -> bool {
    if kind_lib {
        let mut dirs: Vec<PathBuf> =
            ["usr/lib/pkgconfig", "usr/lib64/pkgconfig", "usr/share/pkgconfig", "usr/local/lib/pkgconfig"]
                .iter()
                .map(|d| root.join(d))
                .collect();
        if let Ok(entries) = std::fs::read_dir(root.join("usr/lib")) {
            dirs.extend(entries.flatten().map(|e| e.path().join("pkgconfig")));
        }
        dirs.iter().any(|d| d.join(format!("{name}.pc")).is_file())
    } else {
        ["usr/bin", "bin", "usr/sbin", "sbin", "usr/local/bin"].iter().any(|d| root.join(d).join(name).is_file())
    }
}

/// Each dependency of `dev` with its kind, as `(is_lib, name)`.
pub fn natives(dev: &Dev) -> Vec<(bool, String)> {
    dev.libs.iter().map(|n| (true, n.name.clone())).chain(dev.bins.iter().map(|n| (false, n.name.clone()))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn want(kind: Kind, name: &str) -> Want {
        Want { kind, native: Native { name: name.into(), ..Default::default() }, managers: BTreeSet::new() }
    }

    #[test]
    fn managers_come_from_os_release_and_image_names() {
        assert_eq!(manager_of_os_release("ID=bazzite\nID_LIKE=\"fedora\"\n"), Some("dnf"));
        assert_eq!(manager_of_os_release("ID=opensuse-tumbleweed\n"), Some("zypper"));
        assert_eq!(manager_of_os_release("ID=nixos\n"), None);
        assert_eq!(manager_of_image("debian:13"), Some("apt"));
        assert_eq!(manager_of_image("docker.io/library/fedora@sha256:aa"), Some("dnf"));
        assert_eq!(manager_of_image("ghcr.io/void-linux/void-glibc-full"), Some("xbps"));
    }

    #[test]
    fn lookups_keep_only_packages_shipping_the_exact_file() {
        let gtk = want(Kind::Lib, "gtk4");
        let apt = ["libgtk-4-dev: /usr/lib/aarch64-linux-gnu/pkgconfig/gtk4.pc", "x: /usr/share/doc/gtk4.pc.txt"];
        assert_eq!(candidates("apt", &gtk, &apt), ["libgtk-4-dev"]);
        let cmake = want(Kind::Bin, "cmake");
        let pacman = ["extra\0cmake\x004.1\0usr/bin/cmake", "extra\0cmake-gui\x001\0usr/bin/cmake-gui"];
        assert_eq!(candidates("pacman", &cmake, &pacman), ["cmake"]);
        let xbps = [
            "gtk4-devel-4.18.6_1  /usr/lib/pkgconfig/gtk4.pc",
            "gtk4-devel-32bit-4.18.6_1  /usr/lib32/pkgconfig/gtk4.pc",
        ];
        assert_eq!(candidates("xbps", &gtk, &xbps), ["gtk4-devel"]);
        let pc = want(Kind::Bin, "pkg-config");
        assert_eq!(candidates("xbps", &pc, &["pkg-config-0.29.2_3  /usr/bin/pkg-config.pkg-config"]), ["pkg-config"]);
        let zypper = ["S  | Name  | Summary | Type", "---+---", "   | cmake-full | Make | package"];
        assert_eq!(candidates("zypper", &cmake, &zypper), ["cmake-full"]);
    }

    #[test]
    fn several_providers_prefer_the_dev_package_or_the_same_name_else_miss() {
        let gtk = want(Kind::Lib, "gtk4");
        assert_eq!(pick("apt", &gtk, &["libgtk-4-dev".into(), "libgtk-4-doc".into()]), Ok("libgtk-4-dev".into()));
        let ninja = want(Kind::Bin, "ninja");
        assert_eq!(pick("apt", &ninja, &["ninja".into(), "ninja-build".into()]), Ok("ninja".into()));
        assert!(pick("apt", &ninja, &["a".into(), "b".into()]).unwrap_err().contains("several packages"));
        assert!(pick("apt", &ninja, &[]).unwrap_err().contains("nothing shipping"));
    }

    #[test]
    fn capabilities_and_install_hints() {
        assert_eq!(capability("dnf", &want(Kind::Lib, "gtk4")), "pkgconfig(gtk4)");
        assert_eq!(capability("dnf", &want(Kind::Bin, "cmake")), "/usr/bin/cmake");
        assert_eq!(capability("moss", &want(Kind::Bin, "cmake")), "binary(cmake)");
        assert_eq!(install_hint("dnf", &["pkgconfig(gtk4)".into()]), "sudo dnf install 'pkgconfig(gtk4)'");
        assert_eq!(brew_guesses("dbus-1"), ["dbus-1", "dbus", "dbus1"]);
        assert_eq!(brew_guesses("gtk+-3.0"), ["gtk+-3.0", "gtk+", "gtk+3"]);
        assert_eq!(regex_escape("gtk+-3.0"), "gtk\\+-3\\.0");
    }
}

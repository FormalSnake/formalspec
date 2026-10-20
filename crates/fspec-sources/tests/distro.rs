use fspec_core::fetch::MapFetch;
use fspec_core::lock::LockedPkg;
use fspec_core::resource::Pkg;
use fspec_core::runner::{Output, RecordingRunner};
use fspec_sources::{Cx, resolve};
use serde_json::json;

const HELLO_HEX: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
const HELLO_SRI: &str = "sha256-LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=";

fn pkg(kind: &str, name: &str, names: &[&str]) -> Pkg {
    serde_json::from_value(json!({"name": name, "source": {"kind": kind, "names": names}})).unwrap()
}

fn platforms(p: &[&str]) -> Vec<String> {
    p.iter().map(ToString::to_string).collect()
}

fn on<'a>(fetch: &'a MapFetch, runner: &'a RecordingRunner, platform: &str, distro: &str) -> Cx<'a> {
    let mut cx = Cx::new(fetch, runner);
    cx.local = (platform.into(), distro.into());
    cx
}

fn answer(runner: &RecordingRunner, prefix: &str, stdout: &str) {
    runner.respond(prefix, Output { status: 0, stdout: stdout.into(), stderr: String::new() });
}

fn resolve_on(p: &Pkg, platform: &[&str], cx: &Cx) -> anyhow::Result<LockedPkg> {
    resolve(p, &platforms(platform), None, cx)
}

#[test]
fn dnf_asks_repoquery_on_fedora() {
    let url = "https://mirror/fedora/htop-3.4.1-1.fc42.x86_64.rpm";
    let fetch = MapFetch::default().with(url, "hello");
    let runner = RecordingRunner::default();
    answer(&runner, "dnf repoquery", &format!("{url}\n"));
    let cx = on(&fetch, &runner, "x86_64-linux", "fedora");
    let locked = resolve_on(&pkg("dnf", "htop", &[]), &["x86_64-linux"], &cx).unwrap();
    assert_eq!(runner.lines(), ["dnf repoquery --latest-limit=1 --location htop"]);
    assert_eq!(locked.source, "dnf:");
    assert_eq!(locked.version.as_deref(), Some("3.4.1-1.fc42"));
    let a = &locked.artifacts["x86_64-linux"];
    assert_eq!((a.url.as_deref(), a.hash.as_deref()), (Some(url), Some(HELLO_SRI)));
}

#[test]
fn pacman_asks_pacman_on_arch() {
    let url = "https://mirror/extra/os/x86_64/htop-3.4.1-1-x86_64.pkg.tar.zst";
    let fetch = MapFetch::default().with(url, "hello");
    let runner = RecordingRunner::default();
    answer(&runner, "pacman -Sddp", &format!("htop|3.4.1-1|{url}\n"));
    let cx = on(&fetch, &runner, "x86_64-linux", "arch");
    let locked = resolve_on(&pkg("pacman", "htop", &[]), &["x86_64-linux"], &cx).unwrap();
    assert_eq!(runner.lines(), ["pacman -Sddp --print-format %n|%v|%l htop"]);
    assert_eq!(locked.version.as_deref(), Some("3.4.1-1"));
    assert_eq!(locked.artifacts["x86_64-linux"].url.as_deref(), Some(url));
}

#[test]
fn apt_reads_the_uri_and_checksum_from_print_uris() {
    let url = "http://deb.debian.org/debian/pool/main/h/htop/htop_3.4.1-1_amd64.deb";
    let runner = RecordingRunner::default();
    answer(&runner, "apt-get download", &format!("'{url}' htop_3.4.1-1_amd64.deb 190000 SHA256:{HELLO_HEX}\n"));
    let fetch = MapFetch::default();
    let cx = on(&fetch, &runner, "x86_64-linux", "debian");
    let locked = resolve_on(&pkg("apt", "htop", &[]), &["x86_64-linux"], &cx).unwrap();
    assert_eq!(locked.version.as_deref(), Some("3.4.1-1"));
    let a = &locked.artifacts["x86_64-linux"];
    assert_eq!((a.url.as_deref(), a.hash.as_deref()), (Some(url), Some(HELLO_SRI)));
}

#[test]
fn xbps_reads_key_value_properties() {
    let runner = RecordingRunner::default();
    answer(
        &runner,
        "xbps-query -R",
        &format!(
            "pkgver: htop-3.4.1_1\narchitecture: x86_64\nrepository: https://repo-default.voidlinux.org/current\nfilename-sha256: {HELLO_HEX}\n"
        ),
    );
    let fetch = MapFetch::default();
    let cx = on(&fetch, &runner, "x86_64-linux", "void");
    let locked = resolve_on(&pkg("xbps", "htop", &[]), &["x86_64-linux"], &cx).unwrap();
    assert_eq!(locked.version.as_deref(), Some("3.4.1_1"));
    let a = &locked.artifacts["x86_64-linux"];
    assert_eq!(a.url.as_deref(), Some("https://repo-default.voidlinux.org/current/htop-3.4.1_1.x86_64.xbps"));
    assert_eq!(a.hash.as_deref(), Some(HELLO_SRI));
}

fn arch_db(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (dir, desc) in entries {
        let mut h = tar::Header::new_gnu();
        h.set_size(desc.len() as u64);
        h.set_mode(0o644);
        tar.append_data(&mut h, format!("{dir}/desc"), desc.as_bytes()).unwrap();
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut gz, &tar.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}

fn desc(name: &str, version: &str) -> String {
    format!(
        "%FILENAME%\n{name}-{version}-x86_64.pkg.tar.zst\n\n%NAME%\n{name}\n\n%VERSION%\n{version}\n\n%SHA256SUM%\n{HELLO_HEX}\n"
    )
}

#[test]
fn pacman_falls_back_to_the_mirror_database_off_arch() {
    let core = arch_db(&[("glibc-2.41-1", &desc("glibc", "2.41-1"))]);
    let extra = arch_db(&[("htop-3.4.1-1", &desc("htop", "3.4.1-1")), ("htoprc-1-1", &desc("htoprc", "1-1"))]);
    let fetch = MapFetch::default()
        .with("https://geo.mirror.pkgbuild.com/core/os/x86_64/core.db", core)
        .with("https://geo.mirror.pkgbuild.com/extra/os/x86_64/extra.db", extra);
    let runner = RecordingRunner::default();
    let cx = on(&fetch, &runner, "aarch64-darwin", "macos");
    let locked = resolve_on(&pkg("pacman", "htop", &[]), &["x86_64-linux", "aarch64-linux"], &cx).unwrap();
    assert!(runner.lines().is_empty());
    assert_eq!(locked.version.as_deref(), Some("3.4.1-1"));
    let a = &locked.artifacts["x86_64-linux"];
    assert_eq!(
        a.url.as_deref(),
        Some("https://geo.mirror.pkgbuild.com/extra/os/x86_64/htop-3.4.1-1-x86_64.pkg.tar.zst")
    );
    assert_eq!(a.hash.as_deref(), Some(HELLO_SRI));
    assert!(!locked.artifacts.contains_key("aarch64-linux"), "official mirrors carry x86_64 only");

    let err = resolve_on(&pkg("pacman", "nothere", &[]), &["x86_64-linux"], &cx).unwrap_err().to_string();
    assert!(err.contains("neither the core nor the extra repo"), "{err}");
}

#[test]
fn xbps_falls_back_to_the_repodata_index() {
    let fetch = MapFetch::default().with(
        "https://repo-default.voidlinux.org/current/x86_64-repodata",
        include_bytes!("fixtures/void-x86_64-repodata").as_slice(),
    );
    let runner = RecordingRunner::default();
    let cx = on(&fetch, &runner, "aarch64-darwin", "macos");
    let locked = resolve_on(&pkg("xbps", "htop", &[]), &["x86_64-linux"], &cx).unwrap();
    assert_eq!(locked.version.as_deref(), Some("3.4.1_1"));
    let a = &locked.artifacts["x86_64-linux"];
    assert_eq!(a.url.as_deref(), Some("https://repo-default.voidlinux.org/current/htop-3.4.1_1.x86_64.xbps"));
    assert_eq!(a.hash.as_deref(), Some(HELLO_SRI));

    let err = resolve_on(&pkg("xbps", "nothere", &[]), &["x86_64-linux"], &cx).unwrap_err().to_string();
    assert!(err.contains("not in"), "{err}");
}

#[test]
fn dnf_and_apt_stay_name_only_off_their_distro_and_multi_name_packages_are_not_pinned() {
    let fetch = MapFetch::default();
    let runner = RecordingRunner::default();
    let cx = on(&fetch, &runner, "aarch64-darwin", "macos");
    for kind in ["dnf", "apt"] {
        let locked = resolve_on(&pkg(kind, "htop", &[]), &["x86_64-linux"], &cx).unwrap();
        assert!(locked.version.is_none() && locked.artifacts.is_empty(), "{kind}");
    }
    let cx = on(&fetch, &runner, "x86_64-linux", "arch");
    let locked = resolve_on(&pkg("pacman", "tools", &["jq", "ripgrep"]), &["x86_64-linux"], &cx).unwrap();
    assert!(locked.artifacts.is_empty());
    assert!(runner.lines().is_empty());
}

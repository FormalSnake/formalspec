use fspec_core::fetch::MapFetch;
use fspec_core::lock::Lock;
use fspec_core::resource::{Host, Pkg};
use fspec_core::runner::{Output, RecordingRunner};
use fspec_sources::update::update;
use fspec_sources::{Cx, parse_age, resolve};
use serde_json::{Value, json};

const NOW: i64 = 1_760_000_000; // 2025-10-09T08:53:20Z
const REGISTRY: &str = "https://raw.githubusercontent.com/aquaproj/aqua-registry/main/registry.yaml";
const HELLO_HEX: &str = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
const HELLO_SRI: &str = "sha256-LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=";

fn pkg(v: Value) -> Pkg {
    serde_json::from_value(v).unwrap()
}

fn host(name: &str, platform: &str, age: Option<&str>, pkgs: Vec<Value>) -> Host {
    let resources: Vec<Value> = pkgs.into_iter().map(|mut p| (p["kind"] = "pkg".into(), p).1).collect();
    serde_json::from_value(json!({"name": name, "platform": platform, "release_age": age, "resources": resources}))
        .unwrap()
}

fn asset(name: &str) -> Value {
    json!({"name": name, "browser_download_url": format!("https://dl/{name}"), "digest": HELLO_HEX})
}

fn release(tag: &str, published: &str, assets: &[&str]) -> Value {
    json!({"tag_name": tag, "draft": false, "prerelease": false, "published_at": published,
           "assets": assets.iter().map(|a| asset(a)).collect::<Vec<_>>()})
}

fn cx<'a>(fetch: &'a MapFetch, runner: &'a RecordingRunner) -> Cx<'a> {
    let mut cx = Cx::new(fetch, runner);
    cx.now = NOW;
    cx
}

fn platforms(p: &[&str]) -> Vec<String> {
    p.iter().map(|s| s.to_string()).collect()
}

fn registry() -> MapFetch {
    MapFetch::default().with(REGISTRY, include_str!("fixtures/aqua-registry.yaml"))
}

#[test]
fn aqua_ripgrep() {
    let rel = release(
        "15.1.0",
        "2025-10-01T00:00:00Z",
        &["ripgrep-15.1.0-x86_64-unknown-linux-musl.tar.gz", "ripgrep-15.1.0-aarch64-apple-darwin.tar.gz"],
    );
    let fetch = registry().with("https://api.github.com/repos/BurntSushi/ripgrep/releases/latest", rel.to_string());
    let runner = RecordingRunner::default();
    let p = pkg(json!({"name": "ripgrep", "source": {"kind": "aqua", "name": "ripgrep"}}));
    let locked = resolve(&p, &platforms(&["x86_64-linux", "aarch64-darwin"]), None, &cx(&fetch, &runner)).unwrap();
    assert_eq!(locked.source, "aqua:ripgrep");
    assert_eq!(locked.version.as_deref(), Some("15.1.0"));
    let linux = &locked.artifacts["x86_64-linux"];
    assert_eq!(linux.url.as_deref(), Some("https://dl/ripgrep-15.1.0-x86_64-unknown-linux-musl.tar.gz"));
    assert_eq!(linux.hash.as_deref(), Some(HELLO_SRI));
    assert_eq!(linux.bins, ["ripgrep-15.1.0-x86_64-unknown-linux-musl/rg"]);
    assert_eq!(locked.artifacts["aarch64-darwin"].bins, ["ripgrep-15.1.0-aarch64-apple-darwin/rg"]);
}

#[test]
fn aqua_gh_strips_v_and_reports_missing_asset() {
    let rel =
        release("v2.65.0", "2025-10-01T00:00:00Z", &["gh_2.65.0_macOS_arm64.zip", "gh_2.65.0_linux_amd64.tar.gz"]);
    let fetch = registry().with("https://api.github.com/repos/cli/cli/releases/latest", rel.to_string());
    let runner = RecordingRunner::default();
    let p = pkg(json!({"name": "gh", "source": {"kind": "aqua", "name": "github/gh"}}));
    let locked = resolve(&p, &platforms(&["aarch64-darwin", "x86_64-linux"]), None, &cx(&fetch, &runner)).unwrap();
    assert_eq!(locked.version.as_deref(), Some("2.65.0"));
    assert_eq!(locked.artifacts["aarch64-darwin"].bins, ["gh_2.65.0_macOS_arm64/bin/gh"]);
    let err = resolve(&p, &platforms(&["aarch64-linux"]), None, &cx(&fetch, &runner)).unwrap_err().to_string();
    assert!(err.contains("gh_2.65.0_linux_arm64.tar.gz") && err.contains("gh_2.65.0_macOS_arm64.zip"), "{err}");
}

#[test]
fn github_matches_per_platform() {
    let rel = release(
        "v10.2.0",
        "2025-10-01T00:00:00Z",
        &[
            "fd-v10.2.0-x86_64-unknown-linux-gnu.tar.gz",
            "fd-v10.2.0-x86_64-unknown-linux-musl.tar.gz",
            "fd-v10.2.0-aarch64-apple-darwin.tar.gz",
            "fd_10.2.0_amd64.deb",
        ],
    );
    let fetch = MapFetch::default().with("https://api.github.com/repos/sharkdp/fd/releases/latest", rel.to_string());
    let runner = RecordingRunner::default();
    let p = pkg(json!({"name": "fd", "source": {"kind": "github", "repo": "sharkdp/fd"}}));
    let locked = resolve(&p, &platforms(&["x86_64-linux", "aarch64-darwin"]), None, &cx(&fetch, &runner)).unwrap();
    assert_eq!(locked.source, "github:sharkdp/fd");
    assert_eq!(locked.version.as_deref(), Some("10.2.0"));
    assert!(locked.artifacts["x86_64-linux"].url.as_deref().unwrap().ends_with("linux-musl.tar.gz"));
    assert!(locked.artifacts["aarch64-darwin"].url.as_deref().unwrap().ends_with("aarch64-apple-darwin.tar.gz"));
}

#[test]
fn npm_records_tarball_and_integrity() {
    let doc = json!({
        "dist-tags": {"latest": "2.0.1"},
        "versions": {
            "2.0.0": {"dist": {"tarball": "https://r/c-2.0.0.tgz", "integrity": "sha512-old"}},
            "2.0.1": {"dist": {"tarball": "https://r/c-2.0.1.tgz", "integrity": "sha512-new"}}
        },
        "time": {"2.0.0": "2025-09-01T00:00:00.000Z", "2.0.1": "2025-10-08T00:00:00.000Z"}
    });
    let fetch = MapFetch::default().with("https://registry.npmjs.org/@anthropic-ai/claude-code", doc.to_string());
    let runner = RecordingRunner::default();
    let p = pkg(json!({"name": "claude-code", "source": {"kind": "npm", "name": "@anthropic-ai/claude-code"}}));
    let locked = resolve(&p, &platforms(&["x86_64-linux"]), None, &cx(&fetch, &runner)).unwrap();
    assert_eq!(locked.version.as_deref(), Some("2.0.1"));
    let any = &locked.artifacts["any"];
    assert_eq!((any.url.as_deref(), any.hash.as_deref()), (Some("https://r/c-2.0.1.tgz"), Some("sha512-new")));
    assert_eq!(locked.artifacts.len(), 1);
    // Two days old at NOW: a 3d policy falls back to 2.0.0.
    let aged =
        resolve(&p, &platforms(&["x86_64-linux"]), Some(parse_age("3d").unwrap()), &cx(&fetch, &runner)).unwrap();
    assert_eq!(aged.version.as_deref(), Some("2.0.0"));
}

fn crates_io(repo: &str) -> String {
    json!({"crate": {"max_version": "1.4.0", "repository": repo},
           "versions": [{"num": "1.4.0", "created_at": "2025-10-01T00:00:00+00:00", "yanked": false}]})
    .to_string()
}

#[test]
fn cargo_uses_the_tagged_release_or_errors() {
    let rel = release("v1.4.0", "2025-10-01T00:00:00Z", &["tool-1.4.0-x86_64-unknown-linux-musl.tar.gz"]);
    let fetch = MapFetch::default()
        .with("https://crates.io/api/v1/crates/tool", crates_io("https://github.com/o/tool"))
        .with("https://api.github.com/repos/o/tool/releases/tags/v1.4.0", rel.to_string());
    let runner = RecordingRunner::default();
    let p = pkg(json!({"name": "tool", "source": {"kind": "cargo", "name": "tool"}}));
    let locked = resolve(&p, &platforms(&["x86_64-linux"]), None, &cx(&fetch, &runner)).unwrap();
    assert_eq!(locked.version.as_deref(), Some("1.4.0"));
    assert!(locked.artifacts["x86_64-linux"].url.is_some());

    let err = resolve(&p, &platforms(&["x86_64-linux", "aarch64-darwin"]), None, &cx(&fetch, &runner)).unwrap_err();
    assert!(err.to_string().contains("no prebuilt for aarch64-darwin"), "{err:#}");

    let fetch =
        MapFetch::default().with("https://crates.io/api/v1/crates/tool", crates_io("https://gitlab.com/o/tool"));
    let err = resolve(&p, &platforms(&["x86_64-linux"]), None, &cx(&fetch, &runner)).unwrap_err();
    assert!(err.to_string().contains("no prebuilt for x86_64-linux"), "{err:#}");
}

fn aged_fetch() -> MapFetch {
    let list = json!([
        release("v3.0.0", "2025-10-08T00:00:00Z", &["t-3.0.0-linux-x86_64.tar.gz"]),
        release("v2.0.0", "2025-10-01T00:00:00Z", &["t-2.0.0-linux-x86_64.tar.gz"]),
        release("v1.0.0", "2025-09-01T00:00:00Z", &["t-1.0.0-linux-x86_64.tar.gz"]),
    ]);
    MapFetch::default().with("https://api.github.com/repos/o/t/releases?per_page=100", list.to_string()).with(
        "https://api.github.com/repos/o/t/releases/latest",
        release("v3.0.0", "2025-10-08T00:00:00Z", &["t-3.0.0-linux-x86_64.tar.gz"]).to_string(),
    )
}

#[test]
fn release_age_picks_the_newest_old_enough_and_bleeding_skips_it() {
    let (fetch, runner) = (aged_fetch(), RecordingRunner::default());
    let age = Some(parse_age("3d").unwrap());
    let stable = pkg(json!({"name": "t", "source": {"kind": "github", "repo": "o/t"}}));
    let got = resolve(&stable, &platforms(&["x86_64-linux"]), age, &cx(&fetch, &runner)).unwrap();
    assert_eq!(got.version.as_deref(), Some("2.0.0"));
    let got =
        resolve(&stable, &platforms(&["x86_64-linux"]), Some(parse_age("30d").unwrap()), &cx(&fetch, &runner)).unwrap();
    assert_eq!(got.version.as_deref(), Some("1.0.0"));
    let err = resolve(&stable, &platforms(&["x86_64-linux"]), Some(parse_age("90d").unwrap()), &cx(&fetch, &runner));
    assert!(err.unwrap_err().to_string().contains("old enough"));

    let bleeding = pkg(json!({"name": "t", "channel": "bleeding", "source": {"kind": "github", "repo": "o/t"}}));
    let got = resolve(&bleeding, &platforms(&["x86_64-linux"]), age, &cx(&fetch, &runner)).unwrap();
    assert_eq!(got.version.as_deref(), Some("3.0.0"));
}

#[test]
fn runner_backed_sources() {
    let (fetch, runner) = (MapFetch::default(), RecordingRunner::default());
    runner.respond(
        "flatpak remote-info --system flathub org.mozilla.firefox",
        Output {
            stdout: "Ref: app/org.mozilla.firefox/x86_64/stable\nVersion: 131.0\nCommit: abc123\n".into(),
            ..Default::default()
        },
    );
    runner.respond(
        "git ls-remote https://h/r main",
        Output { stdout: "deadbeef\trefs/heads/main\n".into(), ..Default::default() },
    );
    runner.respond("nix eval --raw nixpkgs#hello.version", Output { stdout: "2.12.1\n".into(), ..Default::default() });
    let cx = cx(&fetch, &runner);
    let ff = resolve(
        &pkg(json!({"name": "firefox", "source": {"kind": "flatpak", "id": "org.mozilla.firefox"}})),
        &[],
        None,
        &cx,
    )
    .unwrap();
    assert_eq!(ff.artifacts["any"].commit.as_deref(), Some("abc123"));
    assert_eq!(ff.version.as_deref(), Some("131.0"));
    let git =
        resolve(&pkg(json!({"name": "r", "source": {"kind": "git", "url": "https://h/r"}})), &[], None, &cx).unwrap();
    assert_eq!(git.rev.as_deref(), Some("deadbeef"));
    let nix =
        resolve(&pkg(json!({"name": "hello", "source": {"kind": "nix", "attr": "hello"}})), &[], None, &cx).unwrap();
    assert_eq!(nix.version.as_deref(), Some("2.12.1"));
    let dnf =
        resolve(&pkg(json!({"name": "htop", "source": {"kind": "dnf", "names": ["htop"]}})), &[], None, &cx).unwrap();
    assert_eq!((dnf.source.as_str(), dnf.version), ("dnf:htop", None));
}

#[test]
fn brew_formula_and_cask() {
    let fetch = MapFetch::default()
        .with("https://formulae.brew.sh/api/formula/jq.json", r#"{"versions": {"stable": "1.7.1"}}"#)
        .with("https://formulae.brew.sh/api/cask/aerospace.json", r#"{"version": "0.15.2"}"#);
    let runner = RecordingRunner::default();
    let cx = cx(&fetch, &runner);
    let jq = resolve(&pkg(json!({"name": "jq", "source": {"kind": "brew"}})), &[], None, &cx).unwrap();
    assert_eq!(jq.version.as_deref(), Some("1.7.1"));
    let cask =
        resolve(&pkg(json!({"name": "aerospace", "source": {"kind": "brew", "cask": true}})), &[], None, &cx).unwrap();
    assert_eq!(cask.version.as_deref(), Some("0.15.2"));
}

#[test]
fn update_keeps_unnamed_drops_undeclared_and_reports_changes() {
    let fetch = aged_fetch().with(
        "https://api.github.com/repos/o/u/releases/latest",
        release("v5.0.0", "2025-10-01T00:00:00Z", &["u-5.0.0-linux-x86_64.tar.gz"]).to_string(),
    );
    let runner = RecordingRunner::default();
    let t = json!({"name": "t", "source": {"kind": "github", "repo": "o/t"}});
    let u = json!({"name": "u", "source": {"kind": "github", "repo": "o/u"}});
    let hosts = vec![host("a", "x86_64-linux", Some("3d"), vec![t.clone(), u.clone()])];

    let old: Lock = toml_lock(
        r#"
[[pkg]]
name = "t"
source = "github:o/t"
version = "1.0.0"

[[pkg]]
name = "u"
source = "github:o/u"
version = "4.0.0"

[[pkg]]
name = "gone"
source = "npm:gone"
version = "0.1.0"
"#,
    );
    let got = update(&hosts, &old, &["t".into()], &cx(&fetch, &runner)).unwrap();
    assert!(got.errors.is_empty(), "{:?}", got.errors);
    assert_eq!(got.changes, ["- gone 0.1.0", "t 1.0.0 -> 2.0.0"]);
    let names: Vec<_> = got.lock.pkg.iter().map(|p| (p.name.as_str(), p.version.as_deref().unwrap())).collect();
    assert_eq!(names, [("t", "2.0.0"), ("u", "4.0.0")]);

    // The delay applies per package: u is only declared on a host without one.
    let hosts = vec![
        host("a", "x86_64-linux", Some("3d"), vec![t.clone()]),
        host("b", "x86_64-linux", None, vec![t.clone(), u.clone()]),
    ];
    let got = update(&hosts, &Lock::default(), &[], &cx(&fetch, &runner)).unwrap();
    assert_eq!(got.changes, ["+ t 2.0.0", "+ u 5.0.0"]);
    assert!(update(&hosts, &Lock::default(), &["nope".into()], &cx(&fetch, &runner)).is_err());
}

#[test]
fn update_collects_platforms_across_hosts_and_survives_a_failure() {
    let rel = release("v1.0.0", "2025-09-01T00:00:00Z", &["t-linux-x86_64.tar.gz", "t-macos-arm64.tar.gz"]);
    let fetch = MapFetch::default().with("https://api.github.com/repos/o/t/releases/latest", rel.to_string());
    let runner = RecordingRunner::default();
    let t = json!({"name": "t", "source": {"kind": "github", "repo": "o/t"}});
    let bad = json!({"name": "bad", "source": {"kind": "npm", "name": "bad"}});
    let hosts = vec![
        host("a", "x86_64-linux", None, vec![t.clone(), bad.clone()]),
        host("b", "aarch64-darwin", None, vec![t.clone()]),
    ];
    let got = update(&hosts, &Lock::default(), &[], &cx(&fetch, &runner)).unwrap();
    assert_eq!(got.lock.pkg("t").unwrap().artifacts.keys().collect::<Vec<_>>(), ["aarch64-darwin", "x86_64-linux"]);
    assert_eq!(got.errors.len(), 1);
    assert!(got.errors[0].contains("bad"), "{:?}", got.errors);
}

fn toml_lock(text: &str) -> Lock {
    toml::from_str(text).unwrap()
}

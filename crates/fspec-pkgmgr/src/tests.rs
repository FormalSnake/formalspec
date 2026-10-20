use std::fs;

use fspec_core::apply::{Backend, Cx, apply_scope, plan_scope, state_dir};
use fspec_core::facts::Facts;
use fspec_core::fetch::MapFetch;
use fspec_core::lock::{Artifact, Lock, LockedPkg};
use fspec_core::plan::{Action, Plan};
use fspec_core::resource::{Desired, Distro, Image, Resource, Scope, System};
use fspec_core::runner::{Output, RecordingRunner};
use fspec_core::{Paths, State};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{AptBackend, BrewBackend, DistroBackend, DnfBackend, FlatpakBackend, PacmanBackend, XbpsBackend};

const ALL: [&dyn Backend; 7] =
    [&FlatpakBackend, &BrewBackend, &DnfBackend, &PacmanBackend, &AptBackend, &XbpsBackend, &DistroBackend];

struct World {
    _dir: tempfile::TempDir,
    paths: Paths,
    facts: Facts,
    lock: Lock,
    runner: RecordingRunner,
    fetch: MapFetch,
}

impl World {
    fn new(platform: &str, system: Option<System>) -> World {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().into(), dir.path().join("home/u"));
        let facts = Facts::declared("h", platform, system.as_ref()).unwrap();
        World {
            _dir: dir,
            paths,
            facts,
            lock: Lock::default(),
            runner: RecordingRunner::default(),
            fetch: MapFetch::default(),
        }
    }

    fn linux() -> World {
        World::on("fedora", &[])
    }

    fn on(distro: &str, packages: &[&str]) -> World {
        let packages = packages.iter().map(ToString::to_string).collect();
        let distro = Distro { distro: distro.into(), init: fspec_core::facts::Init::Systemd, packages };
        World::new("x86_64-linux", Some(System::Distro(distro)))
    }

    fn darwin() -> World {
        World::new("aarch64-darwin", None)
    }

    fn cx(&self, scope: Scope) -> Cx<'_> {
        let generation = self.state(scope).generation + 1;
        Cx {
            paths: &self.paths,
            facts: &self.facts,
            spec_dir: &self.paths.root,
            lock: &self.lock,
            runner: &self.runner,
            fetch: &self.fetch,
            generation,
            scope,
        }
    }

    fn state(&self, scope: Scope) -> State {
        State::load(&state_dir(&self.paths, scope).join("state.json")).unwrap()
    }

    fn apply(&self, scope: Scope, want: &[Desired]) -> anyhow::Result<Plan> {
        apply_scope(&self.cx(scope), &ALL, want)
    }

    fn plan(&self, scope: Scope, want: &[Desired]) -> anyhow::Result<Plan> {
        let state = self.state(scope);
        plan_scope(&self.cx(scope), &ALL, want, &state)
    }

    /// Commands since the last call.
    fn take(&self) -> Vec<String> {
        let lines = self.runner.lines();
        self.runner.calls.lock().unwrap().clear();
        lines
    }

    fn answer(&self, prefix: &str, status: i32, stdout: &str) {
        let out = Output { status, stdout: stdout.into(), stderr: String::new() };
        self.runner.responses.lock().unwrap().insert(0, (prefix.into(), out));
    }

    fn forget_answers(&self) {
        self.runner.responses.lock().unwrap().clear();
    }
}

fn pkg(name: &str, source: Value, mode: &str) -> Desired {
    let resource = json!({"kind": "pkg", "name": name, "source": source, "mode": mode});
    Desired { resource: serde_json::from_value::<Resource>(resource).unwrap(), owner: None }
}

fn flatpak(name: &str, id: &str) -> Desired {
    pkg(name, json!({"kind": "flatpak", "id": id}), "pinned")
}

fn locked(name: &str, source: &str, version: Option<&str>, artifacts: &[(&str, Artifact)]) -> LockedPkg {
    LockedPkg {
        name: name.into(),
        source: source.into(),
        version: version.map(Into::into),
        rev: None,
        patches: None,
        artifacts: artifacts.iter().map(|(k, a)| (k.to_string(), a.clone())).collect(),
    }
}

fn commit(c: &str) -> Artifact {
    Artifact { commit: Some(c.into()), ..Default::default() }
}

fn sri(bytes: &[u8]) -> String {
    use base64::Engine;
    format!("sha256-{}", base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes)))
}

fn is_drift(plan: &Plan) -> bool {
    plan.changes.iter().any(|c| matches!(c.action, Action::Drift { .. }))
}

const INSTALL: &str = "flatpak install -y --noninteractive --system flathub";
const PIN: &str = "flatpak update -y --noninteractive --system";
const UNINSTALL: &str = "flatpak uninstall -y --noninteractive --system";
const INFO: &str = "flatpak info --system --show-commit";

#[test]
fn flatpak_install_pins_and_converges() {
    let mut w = World::linux();
    w.lock.upsert(locked("firefox", "flatpak:org.mozilla.firefox", None, &[("any", commit("aaa"))]));
    let want = [flatpak("firefox", "org.mozilla.firefox")];

    w.apply(Scope::System, &want).unwrap();
    assert_eq!(
        w.take(),
        [format!("{INSTALL} org.mozilla.firefox"), format!("{PIN} --commit=aaa org.mozilla.firefox"),]
    );
    assert_eq!(w.state(Scope::System).entries[&want[0].resource.key()].record["commit"], "aaa");

    w.answer(INFO, 0, "aaa\n");
    assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty());
    w.take();

    w.forget_answers();
    w.answer(INFO, 0, "zzz\n");
    let plan = w.plan(Scope::System, &want).unwrap();
    assert!(is_drift(&plan), "{plan}");
    w.apply(Scope::System, &want).unwrap();
    assert_eq!(w.take().last().unwrap(), &format!("{PIN} --commit=aaa org.mozilla.firefox"));
}

#[test]
fn flatpak_lock_bump_repins_and_missing_commit_fails() {
    let mut w = World::linux();
    w.lock.upsert(locked("firefox", "flatpak:org.mozilla.firefox", None, &[("any", commit("aaa"))]));
    let want = [flatpak("firefox", "org.mozilla.firefox")];
    w.apply(Scope::System, &want).unwrap();
    w.take();

    w.lock.upsert(locked("firefox", "flatpak:org.mozilla.firefox", None, &[("any", commit("bbb"))]));
    w.answer(INFO, 0, "aaa\n");
    assert!(is_drift(&w.plan(Scope::System, &want).unwrap()));
    w.apply(Scope::System, &want).unwrap();
    let lines = w.take();
    assert_eq!(lines.last().unwrap(), &format!("{PIN} --commit=bbb org.mozilla.firefox"));
    assert_eq!(w.state(Scope::System).entries[&want[0].resource.key()].record["commit"], "bbb");

    let bare = World::linux();
    let err = bare.plan(Scope::System, &want).unwrap_err();
    assert!(err.to_string().contains("fspec update"), "{err}");
}

#[test]
fn flatpak_self_managed_installs_once() {
    let mut w = World::linux();
    w.lock.upsert(locked("steam", "flatpak:com.valvesoftware.Steam", None, &[("any", commit("aaa"))]));
    let want = [pkg("steam", json!({"kind": "flatpak", "id": "com.valvesoftware.Steam"}), "self_managed")];
    w.apply(Scope::System, &want).unwrap();
    assert!(!w.take().iter().any(|l| l.starts_with(PIN)));

    // The app updated itself and the lock moved on: nothing to do.
    w.lock.upsert(locked("steam", "flatpak:com.valvesoftware.Steam", None, &[("any", commit("bbb"))]));
    w.answer(INFO, 0, "ccc\n");
    assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty());
}

#[test]
fn flatpak_removal_uninstalls() {
    let mut w = World::linux();
    w.lock.upsert(locked("firefox", "flatpak:org.mozilla.firefox", None, &[("any", commit("aaa"))]));
    let want = [flatpak("firefox", "org.mozilla.firefox")];
    w.apply(Scope::System, &want).unwrap();
    w.take();

    w.apply(Scope::System, &[]).unwrap();
    assert_eq!(w.take(), [format!("{INFO} org.mozilla.firefox"), format!("{UNINSTALL} org.mozilla.firefox")]);
    assert!(w.state(Scope::System).entries.is_empty());
}

#[test]
fn flatpak_failure_rolls_back_the_earlier_install() {
    let mut w = World::linux();
    w.lock.upsert(locked("a", "flatpak:org.a", None, &[("any", commit("aaa"))]));
    w.lock.upsert(locked("b", "flatpak:org.b", None, &[("any", commit("bbb"))]));
    w.answer(&format!("{INSTALL} org.b"), 1, "");
    let err = w.apply(Scope::System, &[flatpak("a", "org.a"), flatpak("b", "org.b")]).unwrap_err();
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    assert_eq!(
        w.take(),
        [
            format!("{INSTALL} org.a"),
            format!("{PIN} --commit=aaa org.a"),
            format!("{INSTALL} org.b"),
            format!("{UNINSTALL} org.b"),
            format!("{UNINSTALL} org.a"),
        ]
    );
    assert!(w.state(Scope::System).entries.is_empty());
}

#[test]
fn flatpak_is_not_claimed_on_darwin() {
    let w = World::darwin();
    let d = flatpak("firefox", "org.mozilla.firefox");
    assert!(!FlatpakBackend.handles(&w.cx(Scope::System), &d.resource));
    assert!(!w.plan(Scope::System, &[d]).unwrap().unsupported.is_empty());
}

fn brew(name: &str, cask: bool, mode: &str) -> Desired {
    pkg(name, json!({"kind": "brew", "cask": cask}), mode)
}

#[test]
fn brew_installs_formula_and_cask_in_user_scope() {
    let mut w = World::darwin();
    w.lock.upsert(locked("ripgrep", "brew", Some("14.1.0"), &[]));
    w.answer("brew list --versions ripgrep", 0, "ripgrep 14.1.0\n");
    let want = [
        brew("ripgrep", false, "pinned"),
        brew("aerospace", true, "pinned"),
        pkg("fd-find", json!({"kind": "brew", "name": "fd"}), "pinned"),
    ];
    assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty());

    w.apply(Scope::User, &want).unwrap();
    assert_eq!(
        w.take(),
        [
            "brew install --cask aerospace",
            "brew list --cask --versions aerospace",
            "brew install fd",
            "brew list --versions fd",
            "brew install ripgrep",
            "brew list --versions ripgrep",
        ]
    );
    let state = w.state(Scope::User);
    assert_eq!(state.entries[&want[0].resource.key()].record["version"], "14.1.0");
    assert!(!state.entries[&want[1].resource.key()].record.contains_key("version"));
    assert!(w.plan(Scope::User, &want).unwrap().changes.is_empty());
}

#[test]
fn brew_pinned_upgrades_on_drift_and_self_managed_never_does() {
    let mut w = World::darwin();
    w.lock.upsert(locked("ripgrep", "brew", Some("14.1.0"), &[]));
    w.lock.upsert(locked("jq", "brew", Some("1.7.0"), &[]));
    w.answer("brew list --versions ripgrep", 0, "ripgrep 14.1.0\n");
    w.answer("brew list --versions jq", 0, "jq 1.7.0\n");
    let want = [brew("ripgrep", false, "pinned"), brew("jq", false, "self_managed")];
    w.apply(Scope::User, &want).unwrap();
    w.take();

    w.lock.upsert(locked("ripgrep", "brew", Some("15.0.0"), &[]));
    w.lock.upsert(locked("jq", "brew", Some("1.8.0"), &[]));
    let plan = w.plan(Scope::User, &want).unwrap();
    assert_eq!(plan.changes.len(), 1, "{plan}");
    assert!(is_drift(&plan));

    w.apply(Scope::User, &want).unwrap();
    let lines = w.take();
    assert_eq!(lines.iter().filter(|l| l.starts_with("brew upgrade")).collect::<Vec<_>>(), ["brew upgrade ripgrep"]);
    assert_eq!(
        &lines[lines.len() - 3..],
        ["brew list --versions ripgrep", "brew upgrade ripgrep", "brew list --versions ripgrep"]
    );

    // A rebuilt formula reports 14.1.0_1 for upstream 14.1.0.
    w.lock.upsert(locked("ripgrep", "brew", Some("14.1.0"), &[]));
    w.forget_answers();
    w.answer("brew list --versions ripgrep", 0, "ripgrep 14.1.0_1\n");
    assert!(w.plan(Scope::User, &want).unwrap().changes.is_empty());
}

#[test]
fn brew_removal_and_rollback() {
    let w = World::darwin();
    let want = [brew("a", false, "pinned"), brew("b", true, "pinned")];
    w.apply(Scope::User, &want).unwrap();
    w.take();
    w.apply(Scope::User, &want[..1]).unwrap();
    assert_eq!(w.take().last().unwrap(), "brew uninstall --cask b");

    w.answer("brew install --cask c", 1, "");
    let err = w.apply(Scope::User, &[want[0].clone(), brew("c", true, "pinned")]).unwrap_err();
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    w.forget_answers();
    let lines: Vec<_> = w.take().into_iter().filter(|l| !l.starts_with("brew list")).collect();
    assert_eq!(lines, ["brew install --cask c", "brew uninstall --cask c"]);
}

#[test]
fn brew_failure_rolls_back_the_earlier_install() {
    let w = World::darwin();
    w.answer("brew install b", 1, "");
    let err = w.apply(Scope::User, &[brew("a", false, "pinned"), brew("b", false, "pinned")]).unwrap_err();
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    assert_eq!(
        w.take(),
        ["brew install a", "brew list --versions a", "brew install b", "brew uninstall b", "brew uninstall a"]
    );
}

fn dnf(name: &str, names: &[&str], mode: &str) -> Desired {
    pkg(name, json!({"kind": "dnf", "names": names}), mode)
}

const QF: &str = "rpm -q --qf %{VERSION}-%{RELEASE}";

#[test]
fn dnf_installs_by_name_and_detects_drift() {
    let w = World::linux();
    w.answer(QF, 0, "3.3-1.fc42");
    let want = [dnf("htop", &[], "pinned"), dnf("tools", &["jq", "ripgrep"], "pinned")];
    w.apply(Scope::System, &want).unwrap();
    assert_eq!(
        w.take(),
        [
            "dnf install -y htop",
            "rpm -q --qf %{VERSION}-%{RELEASE} htop",
            "dnf install -y jq ripgrep",
            "rpm -q --qf %{VERSION}-%{RELEASE} jq",
            "rpm -q --qf %{VERSION}-%{RELEASE} ripgrep",
        ]
    );
    assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty());

    w.forget_answers();
    w.answer(QF, 0, "3.3-1.fc42");
    w.answer("rpm -q --qf %{VERSION}-%{RELEASE} htop", 0, "3.4-1.fc42");
    let plan = w.plan(Scope::System, &want).unwrap();
    assert!(is_drift(&plan), "{plan}");

    w.forget_answers();
    w.answer(QF, 0, "3.3-1.fc42");
    w.answer("rpm -q --qf %{VERSION}-%{RELEASE} jq", 1, "package jq is not installed");
    let plan = w.plan(Scope::System, &want).unwrap();
    assert_eq!(plan.changes.len(), 1, "{plan}");

    w.forget_answers();
    w.take();
    w.apply(Scope::System, &[]).unwrap();
    assert_eq!(w.take(), ["dnf remove -y htop", "dnf remove -y jq ripgrep"]);
}

fn rpm_world(body: &[u8], file: &str) -> World {
    let mut w = World::linux();
    let url = format!("https://example/{file}");
    let artifact = Artifact { url: Some(url.clone()), hash: Some(sri(body)), ..Default::default() };
    w.lock.upsert(locked("htop", "dnf:", Some("3.3"), &[("x86_64-linux", artifact)]));
    w.fetch = MapFetch::default().with(&url, body);
    w
}

#[test]
fn dnf_caches_the_locked_rpm_and_installs_the_file() {
    let w = rpm_world(b"rpm one", "htop-3.3-1.fc42.x86_64.rpm");
    let want = [dnf("htop", &[], "pinned")];
    w.apply(Scope::System, &want).unwrap();
    let cached = w.paths.root.join("var/cache/fspec/pkgs/htop-3.3-1.fc42.x86_64.rpm");
    assert_eq!(fs::read(&cached).unwrap(), b"rpm one");
    assert_eq!(w.take()[0], format!("dnf install -y {}", cached.display()));
    assert_eq!(w.state(Scope::System).entries[&want[0].resource.key()].record["file"], "htop-3.3-1.fc42.x86_64.rpm");
    assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty());
}

#[test]
fn dnf_lock_bump_installs_the_new_rpm_and_journals_the_old_one() {
    let mut w = rpm_world(b"rpm one", "htop-3.3-1.fc42.x86_64.rpm");
    let want = [dnf("htop", &[], "pinned"), dnf("zzz", &[], "pinned")];
    w.apply(Scope::System, &want[..1]).unwrap();
    w.take();

    let new = Artifact {
        url: Some("https://example/htop-3.4-1.fc42.x86_64.rpm".into()),
        hash: Some(sri(b"rpm two")),
        ..Default::default()
    };
    w.lock.upsert(locked("htop", "dnf:", Some("3.4"), &[("x86_64-linux", new)]));
    w.fetch = MapFetch::default().with("https://example/htop-3.4-1.fc42.x86_64.rpm", &b"rpm two"[..]);
    let plan = w.plan(Scope::System, &want[..1]).unwrap();
    assert!(is_drift(&plan), "{plan}");

    // zzz fails after htop upgraded, so the old cached RPM goes back on.
    w.answer("dnf install -y zzz", 1, "");
    assert!(w.apply(Scope::System, &want).is_err());
    let dir = w.paths.root.join("var/cache/fspec/pkgs");
    let lines: Vec<_> = w.take().into_iter().filter(|l| !l.starts_with("rpm -q")).collect();
    assert_eq!(
        lines,
        [
            format!("dnf install -y {}", dir.join("htop-3.4-1.fc42.x86_64.rpm").display()),
            "dnf install -y zzz".to_string(),
            "dnf remove -y zzz".to_string(),
            format!("dnf install -y {}", dir.join("htop-3.3-1.fc42.x86_64.rpm").display()),
        ]
    );
}

#[test]
fn dnf_rejects_a_bad_hash_before_touching_the_system() {
    let mut w = rpm_world(b"rpm one", "htop-3.3-1.fc42.x86_64.rpm");
    w.fetch = MapFetch::default().with("https://example/htop-3.3-1.fc42.x86_64.rpm", &b"tampered"[..]);
    let err = w.apply(Scope::System, &[dnf("htop", &[], "pinned")]).unwrap_err();
    assert!(format!("{err:#}").contains("hashes to"), "{err:#}");
    assert!(w.take().is_empty());
}

#[test]
fn dnf_failure_rolls_back_the_earlier_install() {
    let w = World::linux();
    w.answer("dnf install -y b", 1, "");
    assert!(w.apply(Scope::System, &[dnf("a", &[], "pinned"), dnf("b", &[], "pinned")]).is_err());
    let lines = w.take();
    assert_eq!(&lines[lines.len() - 2..], ["dnf remove -y b", "dnf remove -y a"]);
}

#[test]
fn dnf_is_left_to_the_image_on_image_hosts() {
    let image =
        Image { base: "quay.io/fedora/fedora-silverblue:42".into(), packages: None, kernel_args: vec![], distro: None };
    let w = World::new("x86_64-linux", Some(System::Image(image)));
    assert_eq!(w.facts.distro, "fedora");
    let d = dnf("htop", &[], "pinned");
    assert!(!DnfBackend.handles(&w.cx(Scope::System), &d.resource));
    assert_eq!(w.plan(Scope::System, &[d]).unwrap().unsupported.len(), 1);

    let arch = World::new(
        "x86_64-linux",
        Some(System::Distro(Distro {
            distro: "arch".into(),
            init: fspec_core::facts::Init::Systemd,
            packages: vec![],
        })),
    );
    assert!(!DnfBackend.handles(&arch.cx(Scope::System), &dnf("htop", &[], "pinned").resource));
}

/// How each distro family spells its commands.
struct Fam {
    distro: &'static str,
    kind: &'static str,
    install: &'static str,
    remove: &'static str,
    version: &'static str,
    /// What the version query prints for a package at 3.3.
    printed: &'static str,
    ext: &'static str,
}

const FAMS: [Fam; 4] = [
    Fam {
        distro: "fedora",
        kind: "dnf",
        install: "dnf install -y",
        remove: "dnf remove -y",
        version: "rpm -q --qf %{VERSION}-%{RELEASE}",
        printed: "3.3-1",
        ext: "rpm",
    },
    Fam {
        distro: "arch",
        kind: "pacman",
        install: "pacman -S --noconfirm --needed",
        remove: "pacman -R --noconfirm",
        version: "pacman -Q",
        printed: "htop 3.3-1",
        ext: "pkg.tar.zst",
    },
    Fam {
        distro: "debian",
        kind: "apt",
        install: "apt-get install -y",
        remove: "apt-get remove -y",
        version: "dpkg-query -W -f=${Version}",
        printed: "3.3-1",
        ext: "deb",
    },
    Fam {
        distro: "void",
        kind: "xbps",
        install: "xbps-install -y",
        remove: "xbps-remove -y",
        version: "xbps-query -p pkgver",
        printed: "htop-3.3_1",
        ext: "xbps",
    },
];

impl Fam {
    fn pkg(&self, name: &str, names: &[&str]) -> Desired {
        pkg(name, json!({"kind": self.kind, "names": names}), "pinned")
    }

    fn world(&self) -> World {
        World::on(self.distro, &[])
    }

    /// The commands that install the cached `file` as `names`.
    fn install_file(&self, w: &World, file: &str, names: &str) -> Vec<String> {
        let dir = w.paths.root.join("var/cache/fspec/pkgs");
        let path = dir.join(file);
        match self.kind {
            "dnf" => vec![format!("dnf install -y {}", path.display())],
            "pacman" => vec![format!("pacman -U --noconfirm --needed {}", path.display())],
            "apt" => vec![format!("apt-get install -y {}", path.display())],
            _ => vec![
                format!("xbps-rindex -a {}", path.display()),
                format!("xbps-install -y -R {} {names}", dir.display()),
            ],
        }
    }

    fn is_query(&self, line: &str) -> bool {
        line.starts_with(self.version)
    }

    fn without_queries(&self, lines: Vec<String>) -> Vec<String> {
        lines.into_iter().filter(|l| !self.is_query(l)).collect()
    }
}

fn locked_file(w: &mut World, f: &Fam, version: &str, body: &[u8]) -> String {
    let file = format!("htop-{version}.x86_64.{}", f.ext);
    let url = format!("https://example/{file}");
    let artifact = Artifact { url: Some(url.clone()), hash: Some(sri(body)), ..Default::default() };
    w.lock.upsert(locked("htop", &format!("{}:", f.kind), Some(version), &[("x86_64-linux", artifact)]));
    w.fetch.bodies.insert(url, body.to_vec());
    file
}

#[test]
fn every_family_installs_by_name_reports_drift_and_removes() {
    for f in &FAMS {
        let w = f.world();
        w.answer(f.version, 0, f.printed);
        let want = [f.pkg("htop", &[]), f.pkg("tools", &["jq", "ripgrep"])];
        w.apply(Scope::System, &want).unwrap();
        let lines = w.take();
        assert_eq!(
            f.without_queries(lines.clone()),
            [format!("{} htop", f.install), format!("{} jq ripgrep", f.install)]
        );
        assert_eq!(lines.iter().filter(|l| f.is_query(l)).count(), 3, "{}", f.kind);
        assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty(), "{}", f.kind);

        w.forget_answers();
        w.answer(f.version, 0, f.printed);
        w.answer(&format!("{} jq", f.version), 1, "not installed");
        let plan = w.plan(Scope::System, &want).unwrap();
        assert_eq!(plan.changes.len(), 1, "{}: {plan}", f.kind);

        w.forget_answers();
        w.take();
        w.apply(Scope::System, &[]).unwrap();
        assert_eq!(w.take(), [format!("{} htop", f.remove), format!("{} jq ripgrep", f.remove)], "{}", f.kind);
    }
}

#[test]
fn every_family_detects_a_version_that_moved() {
    for f in &FAMS {
        let w = f.world();
        w.answer(f.version, 0, f.printed);
        let want = [f.pkg("htop", &[])];
        w.apply(Scope::System, &want).unwrap();
        w.forget_answers();
        let moved = f.printed.replace("3.3", "3.4");
        w.answer(f.version, 0, &moved);
        assert!(is_drift(&w.plan(Scope::System, &want).unwrap()), "{}", f.kind);
    }
}

#[test]
fn every_family_caches_the_locked_file_and_installs_it() {
    for f in &FAMS {
        let mut w = f.world();
        let file = locked_file(&mut w, f, "3.3-1", b"package one");
        let want = [f.pkg("htop", &[])];
        w.apply(Scope::System, &want).unwrap();
        let cached = w.paths.root.join("var/cache/fspec/pkgs").join(&file);
        assert_eq!(fs::read(&cached).unwrap(), b"package one", "{}", f.kind);
        assert_eq!(f.without_queries(w.take()), f.install_file(&w, &file, "htop"), "{}", f.kind);
        assert_eq!(w.state(Scope::System).entries[&want[0].resource.key()].record["file"], file.as_str());
        assert!(w.plan(Scope::System, &want).unwrap().changes.is_empty(), "{}", f.kind);
    }
}

#[test]
fn every_family_rejects_a_bad_hash_before_touching_the_system() {
    for f in &FAMS {
        let mut w = f.world();
        let file = locked_file(&mut w, f, "3.3-1", b"package one");
        w.fetch.bodies.insert(format!("https://example/{file}"), b"tampered".to_vec());
        let err = w.apply(Scope::System, &[f.pkg("htop", &[])]).unwrap_err();
        assert!(format!("{err:#}").contains("hashes to"), "{}: {err:#}", f.kind);
        assert!(w.take().is_empty(), "{}", f.kind);
    }
}

#[test]
fn every_family_journals_the_old_file_on_a_lock_bump() {
    for f in &FAMS {
        let mut w = f.world();
        let old = locked_file(&mut w, f, "3.3-1", b"package one");
        let want = [f.pkg("htop", &[]), f.pkg("zzz", &[])];
        w.apply(Scope::System, &want[..1]).unwrap();
        w.take();

        let new = locked_file(&mut w, f, "3.4-1", b"package two");
        assert!(is_drift(&w.plan(Scope::System, &want[..1]).unwrap()), "{}", f.kind);

        w.answer(&format!("{} zzz", f.install), 1, "");
        assert!(w.apply(Scope::System, &want).is_err(), "{}", f.kind);
        w.forget_answers();
        let mut expect = f.install_file(&w, &new, "htop");
        expect.push(format!("{} zzz", f.install));
        expect.push(format!("{} zzz", f.remove));
        expect.extend(f.install_file(&w, &old, "htop"));
        assert_eq!(f.without_queries(w.take()), expect, "{}", f.kind);
    }
}

#[test]
fn every_family_rolls_back_the_earlier_install() {
    for f in &FAMS {
        let w = f.world();
        w.answer(&format!("{} b", f.install), 1, "");
        assert!(w.apply(Scope::System, &[f.pkg("a", &[]), f.pkg("b", &[])]).is_err());
        let lines = w.take();
        assert_eq!(&lines[lines.len() - 2..], [format!("{} b", f.remove), format!("{} a", f.remove)], "{}", f.kind);
    }
}

#[test]
fn apt_runs_noninteractive() {
    let w = World::on("debian", &[]);
    w.apply(Scope::System, &[FAMS[2].pkg("htop", &[])]).unwrap();
    let calls = w.runner.calls.lock().unwrap();
    let install = calls.iter().find(|c| c.program == "apt-get").unwrap();
    assert_eq!(install.env, [("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())]);
}

#[test]
fn distro_managers_claim_only_their_own_distro_and_never_image_hosts() {
    for (i, f) in FAMS.iter().enumerate() {
        for (j, other) in FAMS.iter().enumerate() {
            let w = other.world();
            let claimed = ALL.iter().filter(|b| b.handles(&w.cx(Scope::System), &f.pkg("htop", &[]).resource)).count();
            assert_eq!(claimed, usize::from(i == j), "{} source on {}", f.kind, other.distro);
        }
    }
    let image =
        Image { base: "quay.io/fedora/fedora-silverblue:42".into(), packages: None, kernel_args: vec![], distro: None };
    let w = World::new("x86_64-linux", Some(System::Image(image)));
    let want = FAMS[0].pkg("htop", &[]);
    assert!(!ALL.iter().any(|b| b.handles(&w.cx(Scope::System), &want.resource)));
}

fn distro_res(distro: &str, packages: &[&str]) -> Desired {
    let packages: Vec<_> = packages.iter().map(ToString::to_string).collect();
    Desired {
        resource: Resource::Distro(Distro { distro: distro.into(), init: fspec_core::facts::Init::Systemd, packages }),
        owner: None,
    }
}

#[test]
fn distro_backend_installs_the_diff_of_the_base_packages() {
    for f in &FAMS {
        let w = World::on(f.distro, &["a", "b"]);
        w.apply(Scope::System, &[distro_res(f.distro, &["a", "b"])]).unwrap();
        assert_eq!(f.without_queries(w.take()).last().unwrap(), &format!("{} a b", f.install), "{}", f.kind);

        w.apply(Scope::System, &[distro_res(f.distro, &["b", "c"])]).unwrap();
        let lines = f.without_queries(w.take());
        assert_eq!(&lines[lines.len() - 2..], [format!("{} a", f.remove), format!("{} c", f.install)], "{}", f.kind);

        let rec = &w.state(Scope::System).entries[&distro_res(f.distro, &[]).resource.key()].record;
        assert_eq!(rec["names"], json!(["b", "c"]));
        assert!(w.plan(Scope::System, &[distro_res(f.distro, &["b", "c"])]).unwrap().changes.is_empty());
    }
}

#[test]
fn distro_backend_rolls_back_a_failed_install_and_reports_drift() {
    let f = &FAMS[1];
    let w = World::on(f.distro, &["a"]);
    w.answer(f.version, 0, "a 1.0-1");
    let want = [distro_res(f.distro, &["a"])];
    w.apply(Scope::System, &want).unwrap();
    w.take();

    w.answer(&format!("{} z", f.install), 1, "");
    assert!(w.apply(Scope::System, &[distro_res(f.distro, &["a", "z"])]).is_err());
    let lines = f.without_queries(w.take());
    assert_eq!(&lines[lines.len() - 2..], [format!("{} z", f.install), format!("{} z", f.remove)]);

    w.forget_answers();
    w.answer(f.version, 1, "");
    assert!(is_drift(&w.plan(Scope::System, &want).unwrap()));
}

#[test]
fn btrfs_snapshot_is_taken_before_the_commit_only_on_btrfs() {
    let want = [distro_res("arch", &["linux"])];
    let w = World::on("arch", &["linux"]);
    w.answer("findmnt -no FSTYPE", 0, "ext4\n");
    w.apply(Scope::System, &want).unwrap();
    assert!(w.take().iter().all(|l| !l.starts_with("btrfs")));

    let w = World::on("arch", &["linux"]);
    w.answer("findmnt -no FSTYPE", 0, "btrfs\n");
    w.apply(Scope::System, &want).unwrap();
    let lines = w.take();
    let root = w.paths.system("/").display().to_string();
    let snap = format!("btrfs subvolume snapshot -r {root} {}", w.paths.system("/.snapshots/fspec-1").display());
    let at = lines.iter().position(|l| *l == snap).unwrap_or_else(|| panic!("{lines:?}"));
    let install = lines.iter().position(|l| l.starts_with("pacman -S")).unwrap();
    assert!(at < install, "{lines:?}");

    // Nothing to change, nothing to snapshot.
    w.apply(Scope::System, &want).unwrap();
    assert!(w.take().iter().all(|l| !l.starts_with("btrfs")));
}

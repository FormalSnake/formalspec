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

use crate::{BrewBackend, DnfBackend, FlatpakBackend};

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
        let fedora = Distro { distro: "fedora".into(), init: fspec_core::facts::Init::Systemd, packages: vec![] };
        World::new("x86_64-linux", Some(System::Distro(fedora)))
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
        apply_scope(&self.cx(scope), &[&FlatpakBackend, &BrewBackend, &DnfBackend], want)
    }

    fn plan(&self, scope: Scope, want: &[Desired]) -> anyhow::Result<Plan> {
        let state = self.state(scope);
        plan_scope(&self.cx(scope), &[&FlatpakBackend, &BrewBackend, &DnfBackend], want, &state)
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
    let plan = bare.plan(Scope::System, &want).unwrap();
    assert!(plan.unsupported[0].contains("fspec update"), "{:?}", plan.unsupported);
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
    assert_eq!(w.state(Scope::System).entries[&want[0].resource.key()].record["rpm"], "htop-3.3-1.fc42.x86_64.rpm");
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

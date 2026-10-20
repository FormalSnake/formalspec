use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fspec_core::apply::{apply_scope, desired_from_generation, plan_scope, state_dir};
use fspec_core::fetch::MapFetch;
use fspec_core::lock::{Artifact, LockedPkg};
use fspec_core::runner::{Cmd, Output, RecordingRunner, Runner};
use fspec_core::{Backend, Cx, Desired, Facts, Lock, Paths, Plan, Scope, State};
use fspec_store::{StoreBackend, lock_from_state, pin_dirs, sri_sha256};
use serde_json::json;

const PLATFORM: &str = "aarch64-darwin";

fn tgz(files: &[(&str, &str, u32)]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (path, content, mode) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(content.len() as u64);
        h.set_mode(*mode);
        tar.append_data(&mut h, path, content.as_bytes()).unwrap();
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}

fn hello(version: &str) -> Vec<u8> {
    tgz(&[(&format!("hello-{version}/hello"), &format!("#!/bin/sh\necho {version}\n"), 0o755)])
}

fn locked(name: &str, source: &str, version: &str, url: &str, body: &[u8]) -> LockedPkg {
    let artifact = Artifact { url: Some(url.into()), hash: Some(sri_sha256(body)), ..Default::default() };
    LockedPkg {
        name: name.into(),
        source: source.into(),
        version: Some(version.into()),
        rev: None,
        patches: None,
        artifacts: BTreeMap::from([(PLATFORM.to_string(), artifact)]),
    }
}

fn pkg(name: &str, source: serde_json::Value, mode: &str) -> Desired {
    let resource = serde_json::from_value(json!({"kind": "pkg", "name": name, "source": source, "mode": mode}));
    Desired { resource: resource.unwrap(), owner: None }
}

fn aqua(name: &str) -> Desired {
    pkg(name, json!({"kind": "aqua", "name": name}), "pinned")
}

struct World {
    _dir: tempfile::TempDir,
    paths: Paths,
    facts: Facts,
    lock: Lock,
    runner: Box<dyn Runner>,
    fetch: MapFetch,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().into(), dir.path().join("home/u"));
        World {
            _dir: dir,
            paths,
            facts: Facts::declared("m", PLATFORM, None).unwrap(),
            lock: Lock::default(),
            runner: Box::new(RecordingRunner::default()),
            fetch: MapFetch::default(),
        }
    }

    fn cx(&self) -> Cx<'_> {
        let generation = self.state().generation + 1;
        Cx {
            paths: &self.paths,
            facts: &self.facts,
            spec_dir: &self.paths.root,
            lock: &self.lock,
            runner: self.runner.as_ref(),
            fetch: &self.fetch,
            generation,
            scope: Scope::System,
            protect: &[],
        }
    }

    fn state(&self) -> State {
        State::load(&self.state_dir().join("state.json")).unwrap()
    }

    fn state_dir(&self) -> PathBuf {
        state_dir(&self.paths, Scope::System)
    }

    fn apply(&self, want: &[Desired]) -> anyhow::Result<Plan> {
        apply_scope(&self.cx(), &[&StoreBackend as &dyn Backend], want)
    }

    fn plan(&self, want: &[Desired]) -> Plan {
        plan_scope(&self.cx(), &[&StoreBackend as &dyn Backend], want, &self.state()).unwrap()
    }

    fn root(&self) -> PathBuf {
        self.paths.fspec_root()
    }

    fn current(&self) -> PathBuf {
        fs::read_link(self.root().join("current")).unwrap()
    }

    fn run(&self, bin: &str) -> String {
        let out = std::process::Command::new(self.root().join("current/bin").join(bin)).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn store_entries(&self) -> usize {
        fs::read_dir(self.root().join("store")).map(|d| d.count()).unwrap_or(0)
    }

    fn add_hello(&mut self, version: &str) {
        let url = format!("https://example/hello-{version}.tar.gz");
        let body = hello(version);
        self.lock.upsert(locked("hello", "aqua:hello", version, &url, &body));
        self.fetch = std::mem::take(&mut self.fetch).with(&url, body);
    }
}

#[test]
fn applies_switches_removes_and_rolls_back() {
    let mut w = World::new();
    let want = [aqua("hello")];

    w.add_hello("1.0.0");
    w.apply(&want).unwrap();
    assert_eq!(w.current(), Path::new("generations/1"));
    assert_eq!(w.run("hello"), "1.0.0");
    assert!(w.plan(&want).changes.is_empty());
    let record = &w.state().entries[&want[0].resource.key()].record;
    assert_eq!(record["version"], "1.0.0");
    assert_eq!(record["bins"]["hello"], "hello-1.0.0/hello");
    let store_v1 = record["store"].as_str().unwrap().to_string();

    // A newer lock shows up as drift and builds a new generation beside the old one.
    w.add_hello("2.0.0");
    let plan = w.plan(&want);
    assert_eq!(plan.changes.len(), 1, "{plan}");
    w.apply(&want).unwrap();
    assert_eq!(w.current(), Path::new("generations/2"));
    assert_eq!(w.run("hello"), "2.0.0");
    assert!(w.plan(&want).changes.is_empty());
    assert!(w.root().join("generations/1/bin/hello").exists());
    assert_eq!(w.store_entries(), 4);

    // Rollback needs no network: the old store path is reused and only `current` moves.
    let dir = w.state_dir();
    let past = State::load(&dir.join("generations/1.json")).unwrap();
    let desired = desired_from_generation(&dir, 1).unwrap();
    w.lock = lock_from_state(&past);
    w.fetch = MapFetch::default();
    w.apply(&desired).unwrap();
    assert_eq!(w.current(), Path::new("generations/3"));
    assert_eq!(w.run("hello"), "1.0.0");
    assert_eq!(w.state().entries[&want[0].resource.key()].record["store"], store_v1);
    assert_eq!(w.store_entries(), 4);
    assert!(w.plan(&want).changes.is_empty());

    w.apply(&[]).unwrap();
    assert_eq!(w.current(), Path::new("generations/4"));
    assert!(!w.root().join("current/bin/hello").exists());
    assert!(w.state().entries.is_empty());
    assert_eq!(w.store_entries(), 4);
}

#[test]
fn hash_mismatch_aborts_before_anything_changes() {
    let mut w = World::new();
    w.add_hello("1.0.0");
    w.fetch = MapFetch::default().with("https://example/hello-1.0.0.tar.gz", hello("9.9.9"));
    let err = w.apply(&[aqua("hello")]).unwrap_err();
    assert!(format!("{err:#}").contains("hash mismatch"), "{err:#}");
    assert!(fs::symlink_metadata(w.root().join("current")).is_err());
    assert_eq!(w.store_entries(), 0);
    assert!(!w.root().join("generations").exists());
    assert_eq!(w.state().generation, 0);
}

#[test]
fn names_the_fix_when_the_lock_does_not_cover_a_pkg() {
    let mut w = World::new();
    let err = w.apply(&[aqua("hello")]).unwrap_err();
    assert!(format!("{err:#}").contains("pkg hello is not in fspec.lock, run fspec update"), "{err:#}");

    w.add_hello("1.0.0");
    let github = pkg("hello", json!({"kind": "github", "repo": "acme/hello"}), "pinned");
    let err = w.apply(&[github]).unwrap_err();
    assert!(format!("{err:#}").contains("pkg hello is not in fspec.lock, run fspec update"), "{err:#}");
}

#[test]
fn self_managed_installs_once_and_ignores_later_locks() {
    let mut w = World::new();
    let want = [pkg("hello", json!({"kind": "aqua", "name": "hello"}), "self_managed")];
    w.add_hello("1.0.0");
    w.apply(&want).unwrap();

    w.add_hello("2.0.0");
    assert!(w.plan(&want).changes.is_empty());
    w.apply(&want).unwrap();
    assert_eq!(w.state().generation, 1);
    assert_eq!(w.run("hello"), "1.0.0");

    // A config change on the package keeps the installed version too.
    let mut with_data = serde_json::to_value(&want[0].resource).unwrap();
    with_data["data"] = json!(["~/.hello"]);
    let with_data = Desired { resource: serde_json::from_value(with_data).unwrap(), owner: None };
    w.apply(std::slice::from_ref(&with_data)).unwrap();
    assert_eq!(w.state().generation, 2);
    assert_eq!(w.run("hello"), "1.0.0");
}

#[test]
fn failed_commit_leaves_no_trace() {
    let mut w = World::new();
    w.add_hello("1.0.0");
    let empty = tgz(&[("lib/readme.txt", "no executables here", 0o644)]);
    w.lock.upsert(locked("zlib", "aqua:zlib", "1.0.0", "https://example/zlib.tar.gz", &empty));
    w.fetch = std::mem::take(&mut w.fetch).with("https://example/zlib.tar.gz", empty);

    let err = w.apply(&[aqua("hello"), aqua("zlib")]).unwrap_err();
    assert!(format!("{err:#}").contains("no executables"), "{err:#}");
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    assert!(fs::symlink_metadata(w.root().join("current")).is_err());
    assert!(!w.root().join("generations/1").exists());
    assert_eq!(w.store_entries(), 0);
    assert_eq!(w.state().generation, 0);
}

#[test]
fn a_failed_second_apply_restores_current() {
    let mut w = World::new();
    w.add_hello("1.0.0");
    w.apply(&[aqua("hello")]).unwrap();

    let empty = tgz(&[("lib/readme.txt", "no executables here", 0o644)]);
    w.lock.upsert(locked("zlib", "aqua:zlib", "1.0.0", "https://example/zlib.tar.gz", &empty));
    w.fetch = std::mem::take(&mut w.fetch).with("https://example/zlib.tar.gz", empty);
    w.apply(&[aqua("hello"), aqua("zlib")]).unwrap_err();
    assert_eq!(w.current(), Path::new("generations/1"));
    assert_eq!(w.run("hello"), "1.0.0");
    assert!(!w.root().join("generations/2").exists());
}

#[test]
fn artifact_bins_pick_the_links() {
    let mut w = World::new();
    let body = tgz(&[
        ("tool-1/bin/tool", "#!/bin/sh\necho tool\n", 0o755),
        ("tool-1/bin/helper", "#!/bin/sh\necho helper\n", 0o755),
    ]);
    let mut l = locked("tool", "aqua:tool", "1.0.0", "https://example/tool.tar.gz", &body);
    l.artifacts.get_mut(PLATFORM).unwrap().bins = vec!["tool-1/bin/tool".into()];
    w.lock.upsert(l);
    w.fetch = MapFetch::default().with("https://example/tool.tar.gz", body);
    w.apply(&[aqua("tool")]).unwrap();
    assert_eq!(w.run("tool"), "tool");
    assert!(!w.root().join("current/bin/helper").exists());
}

#[test]
fn npm_without_dependencies_links_its_bin() {
    let mut w = World::new();
    let body = tgz(&[
        ("package/package.json", r#"{"name": "@scope/tool", "bin": "./cli.sh"}"#, 0o644),
        ("package/cli.sh", "#!/bin/sh\necho npm tool\n", 0o644),
    ]);
    w.lock.upsert(locked("tool", "npm:@scope/tool", "1.2.3", "https://example/tool.tgz", &body));
    w.fetch = MapFetch::default().with("https://example/tool.tgz", body);
    w.apply(&[pkg("tool", json!({"kind": "npm", "name": "@scope/tool"}), "pinned")]).unwrap();
    assert_eq!(w.run("tool"), "npm tool");
}

/// Stands in for npm: `npm install --omit=dev --prefix <dir> <tarball>` writes a fake tree.
struct FakeNpm {
    calls: Mutex<Vec<String>>,
}

impl Runner for FakeNpm {
    fn run(&self, cmd: &Cmd) -> anyhow::Result<Output> {
        self.calls.lock().unwrap().push(cmd.to_string());
        let prefix = Path::new(&cmd.args[3]);
        let bin = prefix.join("node_modules/.bin");
        fs::create_dir_all(&bin)?;
        fs::create_dir_all(prefix.join("node_modules/@scope/tool"))?;
        fs::write(prefix.join("node_modules/@scope/tool/package.json"), r#"{"bin": {"tool": "cli.js"}}"#)?;
        fs::write(bin.join("tool"), "#!/bin/sh\necho installed\n")?;
        fs::set_permissions(bin.join("tool"), fs::Permissions::from_mode(0o755))?;
        Ok(Output::default())
    }
}

#[test]
fn npm_with_dependencies_runs_npm_install() {
    let mut w = World::new();
    let body = tgz(&[(
        "package/package.json",
        r#"{"name": "@scope/tool", "bin": {"tool": "cli.js"}, "dependencies": {"left-pad": "1.0.0"}}"#,
        0o644,
    )]);
    w.lock.upsert(locked("tool", "npm:@scope/tool", "1.2.3", "https://example/tool.tgz", &body));
    w.fetch = MapFetch::default().with("https://example/tool.tgz", body);
    let npm = FakeNpm { calls: Mutex::new(vec![]) };
    w.runner = Box::new(npm);
    w.apply(&[pkg("tool", json!({"kind": "npm", "name": "@scope/tool"}), "pinned")]).unwrap();
    assert_eq!(w.run("tool"), "installed");
}

#[test]
fn nix_builds_through_the_runner_and_links_its_bin() {
    let mut w = World::new();
    let out = w.paths.root.join("nixstore/hello-2.12");
    fs::create_dir_all(out.join("bin")).unwrap();
    fs::write(out.join("bin/hello"), "#!/bin/sh\necho nix hello\n").unwrap();
    fs::set_permissions(out.join("bin/hello"), fs::Permissions::from_mode(0o755)).unwrap();
    let runner = RecordingRunner::default();
    runner.respond("nix build", Output { status: 0, stdout: format!("{}\n", out.display()), stderr: String::new() });
    w.runner = Box::new(runner);
    w.lock.upsert(LockedPkg {
        name: "hello".into(),
        source: "nix:hello".into(),
        version: Some("2.12".into()),
        rev: None,
        patches: None,
        artifacts: BTreeMap::new(),
    });
    w.apply(&[pkg("hello", json!({"kind": "nix", "attr": "hello"}), "pinned")]).unwrap();
    assert_eq!(w.run("hello"), "nix hello");
    assert_eq!(fs::read_link(w.root().join("current/bin/hello")).unwrap(), out.join("bin/hello"));
}

#[test]
fn project_pins_put_the_pinned_version_first() {
    let mut w = World::new();
    let want = [aqua("hello")];
    w.add_hello("1.0.0");
    w.apply(&want).unwrap();
    w.add_hello("2.0.0");
    w.apply(&want).unwrap();

    let project = w.paths.root.join("work/proj");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join(".fspec.toml"), "[tools]\nhello = \"1.0.0\"\nmissing = \"3.0.0\"\n").unwrap();
    let dirs = pin_dirs(&w.paths, &project.join("src")).unwrap();
    assert_eq!(dirs.len(), 1);
    let out = std::process::Command::new(dirs[0].join("hello")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1.0.0");

    assert!(pin_dirs(&w.paths, &w.paths.root.join("elsewhere")).unwrap().is_empty());
}

fn with_data(d: Desired, data: &[&str]) -> Desired {
    let mut value = serde_json::to_value(&d.resource).unwrap();
    value["data"] = json!(data);
    Desired { resource: serde_json::from_value(value).unwrap(), owner: None }
}

impl World {
    fn add_tool(&mut self, name: &str) {
        let url = format!("https://example/{name}.tar.gz");
        let body = tgz(&[(&format!("{name}/{name}"), "#!/bin/sh\necho tool\n", 0o755)]);
        self.lock.upsert(locked(name, &format!("aqua:{name}"), "1.0.0", &url, &body));
        self.fetch = std::mem::take(&mut self.fetch).with(&url, body);
    }

    fn rollback_to(&mut self, n: u64) {
        let dir = self.state_dir();
        let past = State::load(&dir.join(format!("generations/{n}.json"))).unwrap();
        let desired = desired_from_generation(&dir, n).unwrap();
        self.lock = lock_from_state(&past);
        self.fetch = MapFetch::default();
        self.apply(&desired).unwrap();
    }
}

#[test]
fn rollback_brings_a_removed_packages_data_back() {
    let mut w = World::new();
    w.add_tool("extra");
    let data = w.paths.system("/var/lib/extra");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("db"), "rows").unwrap();
    let want = [with_data(aqua("extra"), &["/var/lib/extra"])];

    w.apply(&want).unwrap();
    w.apply(&[]).unwrap();
    assert!(!data.exists());
    assert!(w.state_dir().join("trash/2/extra/0-extra/db").exists());

    w.rollback_to(1);
    assert_eq!(fs::read_to_string(data.join("db")).unwrap(), "rows");
    assert_eq!(w.run("extra"), "tool");
}

#[test]
fn gc_keeps_what_current_references_and_deletes_the_rest() {
    let mut w = World::new();
    w.add_tool("extra");
    let data = w.paths.system("/var/lib/extra");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("db"), "rows").unwrap();
    let extra = with_data(aqua("extra"), &["/var/lib/extra"]);

    w.add_hello("1.0.0");
    w.apply(&[aqua("hello"), extra]).unwrap();
    w.add_hello("2.0.0");
    w.apply(&[aqua("hello")]).unwrap();
    w.add_hello("3.0.0");
    w.apply(&[aqua("hello")]).unwrap();
    let dir = w.state_dir();
    let now = std::time::SystemTime::now();
    let day = std::time::Duration::from_secs(86_400);

    let none = fspec_store::gc::run(&w.paths, Scope::System, now - 14 * day).unwrap();
    assert!(none.removed.is_empty(), "{none:?}");

    let old = fs::File::open(dir.join("generations/1.json")).unwrap();
    old.set_modified(now - 30 * day).unwrap();
    let some = fspec_store::gc::run(&w.paths, Scope::System, now - 14 * day).unwrap();
    assert_eq!(some.removed.len(), 1, "{some:?}");
    assert!(!dir.join("generations/1.json").exists());
    assert!(dir.join("generations/2.json").exists());

    let report = fspec_store::gc::run(&w.paths, Scope::System, now + day).unwrap();
    assert!(report.freed > 0);
    assert!(!dir.join("generations/2.json").exists());
    assert!(dir.join("generations/3.json").exists());
    assert!(!dir.join("trash/2").exists(), "{:?}", report.removed);
    assert!(!w.root().join("generations/1").exists());
    assert!(!w.root().join("generations/2").exists());
    assert_eq!(w.current(), Path::new("generations/3"));
    assert_eq!(w.run("hello"), "3.0.0");
    // hello 3.0.0 and its sidecar are all that is left in the store.
    assert_eq!(w.store_entries(), 2);
    assert!(w.plan(&[aqua("hello")]).changes.is_empty());
}

#[test]
fn durations_parse() {
    use fspec_store::gc::parse_duration;
    assert_eq!(parse_duration("14d").unwrap().as_secs(), 14 * 86_400);
    assert_eq!(parse_duration("12h").unwrap().as_secs(), 43_200);
    assert_eq!(parse_duration("0s").unwrap().as_secs(), 0);
    assert!(parse_duration("14").is_err());
    assert!(parse_duration("d").is_err());
}

const NPM_PATCH: &str = "\
diff --git a/cli.sh b/cli.sh
--- a/cli.sh
+++ b/cli.sh
@@ -1,2 +1,2 @@
 #!/bin/sh
-echo npm tool
+echo patched tool
";

fn npm_tool() -> Vec<u8> {
    tgz(&[
        ("package/package.json", r#"{"name": "@scope/tool", "bin": "./cli.sh"}"#, 0o644),
        ("package/cli.sh", "#!/bin/sh\necho npm tool\n", 0o644),
    ])
}

fn npm_pkg(patches: &[&str]) -> Desired {
    let resource = serde_json::from_value(json!({
        "kind": "pkg", "name": "tool", "source": {"kind": "npm", "name": "@scope/tool"}, "patches": patches,
    }));
    Desired { resource: resource.unwrap(), owner: None }
}

fn lock_npm(w: &mut World, patches: Option<String>) {
    let body = npm_tool();
    let mut l = locked("tool", "npm:@scope/tool", "1.2.3", "https://example/tool.tgz", &body);
    l.patches = patches;
    w.lock.upsert(l);
    w.fetch = MapFetch::default().with("https://example/tool.tgz", body);
}

fn write_patch(w: &World, text: &str) -> String {
    fs::create_dir_all(w.paths.root.join("patches")).unwrap();
    fs::write(w.paths.root.join("patches/fix.patch"), text).unwrap();
    "patches/fix.patch".into()
}

#[test]
fn npm_patches_apply_to_the_unpacked_tree_and_change_the_store_path() {
    let mut w = World::new();
    lock_npm(&mut w, None);
    w.apply(&[npm_pkg(&[])]).unwrap();
    assert_eq!(w.run("tool"), "npm tool");
    let plain = w.state().entries[&npm_pkg(&[]).resource.key()].record["store"].as_str().unwrap().to_string();

    let patch = write_patch(&w, NPM_PATCH);
    let hash = fspec_core::patches::hash(&w.paths.root, std::slice::from_ref(&patch)).unwrap();
    lock_npm(&mut w, Some(hash));
    let want = [npm_pkg(&[&patch])];
    assert_eq!(w.plan(&want).changes.len(), 1);
    w.apply(&want).unwrap();
    assert_eq!(w.run("tool"), "patched tool");
    let patched = w.state().entries[&want[0].resource.key()].record["store"].as_str().unwrap().to_string();
    assert_ne!(plain, patched);
    assert!(w.root().join(&plain).exists(), "the unpatched tree stays for rollback");
    assert!(w.plan(&want).changes.is_empty());
}

#[test]
fn a_patched_pkg_whose_patches_differ_from_the_lock_is_refused() {
    let mut w = World::new();
    let patch = write_patch(&w, NPM_PATCH);
    let hash = fspec_core::patches::hash(&w.paths.root, std::slice::from_ref(&patch)).unwrap();
    lock_npm(&mut w, Some(hash));
    write_patch(&w, &NPM_PATCH.replace("patched", "edited"));
    let err = w.apply(&[npm_pkg(&[&patch])]).unwrap_err();
    assert!(format!("{err:#}").contains("patches changed, run fspec update"), "{err:#}");
    assert_eq!(w.store_entries(), 0);

    lock_npm(&mut w, None);
    let err = w.apply(&[npm_pkg(&[&patch])]).unwrap_err();
    assert!(format!("{err:#}").contains("patches changed, run fspec update"), "{err:#}");
}

#[test]
fn a_patch_that_does_not_apply_aborts_the_apply() {
    let mut w = World::new();
    let patch = write_patch(&w, &NPM_PATCH.replace("echo npm tool", "echo something else"));
    let hash = fspec_core::patches::hash(&w.paths.root, std::slice::from_ref(&patch)).unwrap();
    lock_npm(&mut w, Some(hash));
    let err = w.apply(&[npm_pkg(&[&patch])]).unwrap_err();
    assert!(format!("{err:#}").contains("patching tool"), "{err:#}");
    assert_eq!(w.store_entries(), 0);
}

const REF: &str = "ghcr.io/o/fspec-cache/tool@sha256:aa";

fn tar_zst(name: &str, content: &str) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_size(content.len() as u64);
    h.set_mode(0o755);
    tar.append_data(&mut h, name, content.as_bytes()).unwrap();
    let tar = tar.into_inner().unwrap();
    ruzstd::encoding::compress_to_vec(tar.as_slice(), ruzstd::encoding::CompressionLevel::Fastest)
}

/// Plays `oras`: `pull` drops the artifact into the `-o` directory, `manifest fetch` names its layer.
struct FakeOras {
    artifact: Vec<u8>,
    layer_digest: String,
    calls: Mutex<Vec<String>>,
}

impl FakeOras {
    fn new(artifact: Vec<u8>) -> FakeOras {
        let layer_digest = format!("sha256:{}", sha256_hex(&artifact));
        FakeOras { artifact, layer_digest, calls: Mutex::new(vec![]) }
    }
}

fn sha256_hex(body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(body).iter().map(|b| format!("{b:02x}")).collect()
}

impl Runner for FakeOras {
    fn run(&self, cmd: &Cmd) -> anyhow::Result<Output> {
        self.calls.lock().unwrap().push(cmd.to_string());
        let mut out = Output::default();
        match cmd.args[0].as_str() {
            "pull" => {
                assert_eq!(cmd.args[1], REF);
                assert_eq!(cmd.args[2], "-o");
                fs::write(Path::new(&cmd.args[3]).join("tool-aarch64-darwin.tar.zst"), &self.artifact)?;
            }
            "manifest" => {
                out.stdout = json!({"layers": [{
                    "digest": self.layer_digest,
                    "annotations": {"org.opencontainers.image.title": "tool-aarch64-darwin.tar.zst"},
                }]})
                .to_string();
            }
            other => panic!("unexpected oras {other}"),
        }
        Ok(out)
    }
}

fn git_pkg() -> Desired {
    pkg("tool", json!({"kind": "git", "url": "https://example/tool", "rev": "main"}), "pinned")
}

fn lock_git(w: &mut World, reference: &str) {
    w.lock.upsert(LockedPkg {
        name: "tool".into(),
        source: "git:https://example/tool@main".into(),
        version: None,
        rev: Some("4f1c2a9d3e5b7a8c9d0e1f2a3b4c5d6e7f8a9b0c".into()),
        patches: None,
        artifacts: BTreeMap::from([(
            PLATFORM.to_string(),
            Artifact { oci: Some(reference.into()), ..Default::default() },
        )]),
    });
}

#[test]
fn a_ci_built_package_is_pulled_with_oras_and_installed() {
    let mut w = World::new();
    lock_git(&mut w, REF);
    let oras = std::sync::Arc::new(FakeOras::new(tar_zst("tool", "#!/bin/sh\necho from ci\n")));
    struct Shared(std::sync::Arc<FakeOras>);
    impl Runner for Shared {
        fn run(&self, cmd: &Cmd) -> anyhow::Result<Output> {
            self.0.run(cmd)
        }
    }
    w.runner = Box::new(Shared(oras.clone()));
    w.apply(&[git_pkg()]).unwrap();
    assert_eq!(w.run("tool"), "from ci");

    let calls = oras.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].starts_with(&format!("oras pull {REF} -o ")), "{calls:?}");
    assert_eq!(calls[1], format!("oras manifest fetch {REF}"));
    let record = &w.state().entries[&git_pkg().resource.key()].record;
    assert_eq!(record["id"], REF);
    assert_eq!(record["bins"]["tool"], "tool");
}

#[test]
fn an_oci_layer_that_does_not_match_its_manifest_is_refused() {
    let mut w = World::new();
    lock_git(&mut w, REF);
    let mut oras = FakeOras::new(tar_zst("tool", "#!/bin/sh\necho from ci\n"));
    oras.layer_digest = "sha256:00".into();
    w.runner = Box::new(oras);
    let err = w.apply(&[git_pkg()]).unwrap_err();
    assert!(format!("{err:#}").contains("manifest says"), "{err:#}");
    assert_eq!(w.store_entries(), 0);
}

#[test]
fn a_tag_without_a_digest_is_refused_and_never_pulled() {
    let mut w = World::new();
    lock_git(&mut w, "ghcr.io/o/fspec-cache/tool:abc-aarch64-darwin");
    let runner = RecordingRunner::default();
    w.runner = Box::new(runner);
    let err = w.apply(&[git_pkg()]).unwrap_err();
    assert!(format!("{err:#}").contains("not pinned to a digest"), "{err:#}");
}

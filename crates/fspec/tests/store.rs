use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use fspec_core::Lock;
use fspec_core::lock::{Artifact, LockedPkg};
use fspec_store::sri_sha256;

/// Pack `files` (path, content, executable) into a tarball, once: a rebuilt tarball would hash differently.
fn archive(work: &Path, file: &str, files: &[(&str, &str, bool)]) -> PathBuf {
    let out = work.join("fixtures").join(file);
    if out.exists() {
        return out;
    }
    let src = work.join("src").join(file);
    for (path, content, exec) in files {
        let at = src.join(path);
        fs::create_dir_all(at.parent().unwrap()).unwrap();
        fs::write(&at, content).unwrap();
        let mode = if *exec { 0o755 } else { 0o644 };
        fs::set_permissions(&at, std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap();
    }
    fs::create_dir_all(out.parent().unwrap()).unwrap();
    let status = Command::new("tar").arg("-czf").arg(&out).arg("-C").arg(&src).arg(".").status().unwrap();
    assert!(status.success());
    out
}

fn locked(name: &str, source: &str, version: &str, tarball: &Path) -> LockedPkg {
    let artifact = Artifact {
        url: Some(format!("file://{}", tarball.display())),
        hash: Some(sri_sha256(&fs::read(tarball).unwrap())),
        ..Default::default()
    };
    LockedPkg {
        name: name.into(),
        source: source.into(),
        version: Some(version.into()),
        rev: None,
        patches: None,
        artifacts: BTreeMap::from([("aarch64-darwin".to_string(), artifact)]),
    }
}

/// A copy of examples/store with fixture archives and a lock pointing at them. The lock is
/// generated here because `file://` URLs have to be absolute.
struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Project {
        let dir = tempfile::tempdir().unwrap();
        let spec = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/store/spec.star");
        fs::copy(spec, dir.path().join("spec.star")).unwrap();
        Project { dir }
    }

    fn work(&self) -> &Path {
        self.dir.path()
    }

    fn root(&self) -> PathBuf {
        self.work().join("root")
    }

    /// Write `fspec.lock` with hello at `version`.
    fn lock(&self, version: &str) {
        let w = self.work();
        let hello = archive(
            w,
            &format!("hello-{version}.tar.gz"),
            &[(&format!("hello-{version}/hello"), &format!("#!/bin/sh\necho hello {version}\n"), true)],
        );
        let greet = archive(w, "greet-0.3.0.tar.gz", &[("greet", "#!/bin/sh\necho greet\n", true)]);
        let shout = archive(
            w,
            "shout-1.0.0.tgz",
            &[
                ("package/package.json", r#"{"name": "@acme/shout", "bin": {"shout": "shout.sh"}}"#, false),
                ("package/shout.sh", "#!/bin/sh\necho SHOUT\n", false),
            ],
        );
        let mut lock = Lock::default();
        lock.upsert(locked("hello", "aqua:hello", version, &hello));
        lock.upsert(locked("greet", "github:acme/greet", "0.3.0", &greet));
        lock.upsert(locked("shout", "npm:@acme/shout", "1.0.0", &shout));
        lock.save(&w.join("fspec.lock")).unwrap();
    }

    fn fspec(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fspec"))
            .arg("--spec")
            .arg(self.work().join("spec.star"))
            .arg("--root")
            .arg(self.root())
            .arg("--home")
            .arg(self.root().join("home/u"))
            .args(args)
            .args(if args[0] == "plan" || args[0] == "apply" || args[0] == "rollback" {
                vec!["--host", "macbook"]
            } else {
                vec![]
            })
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.fspec(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn run(&self, bin: &str) -> String {
        let out = Command::new(self.root().join("opt/fspec/current/bin").join(bin)).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn current(&self) -> PathBuf {
        fs::read_link(self.root().join("opt/fspec/current")).unwrap()
    }
}

#[test]
fn apply_update_and_rollback_move_current() {
    let p = Project::new();
    p.lock("1.0.0");
    assert!(p.ok(&["plan"]).contains("3 to add"));
    p.ok(&["apply"]);
    assert_eq!(p.current(), Path::new("generations/1"));
    assert_eq!(p.run("hello"), "hello 1.0.0");
    assert_eq!(p.run("greet"), "greet");
    assert_eq!(p.run("shout"), "SHOUT");
    assert!(p.ok(&["plan"]).contains("no changes"));

    p.lock("2.0.0");
    assert!(p.ok(&["plan"]).contains("! pkg:hello"));
    p.ok(&["apply"]);
    assert_eq!(p.current(), Path::new("generations/2"));
    assert_eq!(p.run("hello"), "hello 2.0.0");
    assert!(p.ok(&["plan"]).contains("no changes"));

    p.ok(&["rollback"]);
    assert_eq!(p.current(), Path::new("generations/3"));
    assert_eq!(p.run("hello"), "hello 1.0.0");
    assert_eq!(p.run("greet"), "greet");
    // The lock still names 2.0.0, so the plan says so until the lock is reverted too.
    assert!(p.ok(&["plan"]).contains("locked 2.0.0 (installed 1.0.0)"));
    p.lock("1.0.0");
    assert!(p.ok(&["plan"]).contains("no changes"));
}

#[test]
fn shell_hook_and_pins() {
    let p = Project::new();
    p.lock("1.0.0");
    p.ok(&["apply"]);
    p.lock("2.0.0");
    p.ok(&["apply"]);

    let project = p.work().join("proj");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join(".fspec.toml"), "[tools]\nhello = \"1.0.0\"\n").unwrap();
    let dirs = p.ok(&["env", "--dir", project.to_str().unwrap()]);
    let bin = PathBuf::from(dirs.lines().next().unwrap());
    let out = Command::new(bin.join("hello")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello 1.0.0");

    for shell in ["fish", "zsh", "bash"] {
        assert!(p.ok(&["shell-hook", shell]).contains("fspec env --dir"));
    }
}

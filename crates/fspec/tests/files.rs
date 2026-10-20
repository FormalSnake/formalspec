use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// A copy of `examples/files` and an empty root, so nothing touches the repo or the machine.
struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/files");
        let status = Command::new("cp").arg("-R").arg(&example).arg(dir.path().join("spec")).status().unwrap();
        assert!(status.success());
        let key_dir = dir.path().join("home/u/.config/fspec");
        fs::create_dir_all(&key_dir).unwrap();
        fs::copy(example.join("age.key"), key_dir.join("age.key")).unwrap();
        Sandbox { dir }
    }

    fn fspec(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fspec"))
            .arg("--spec")
            .arg(self.dir.path().join("spec/spec.star"))
            .arg("--root")
            .arg(self.dir.path())
            .arg("--home")
            .arg(self.dir.path().join("home/u"))
            .args(args)
            .args(["--host", "macbook"])
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.fspec(args);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn demo(&self, name: &str) -> std::path::PathBuf {
        self.dir.path().join("home/u/.config/fspec-demo").join(name)
    }
}

#[test]
fn apply_converges_then_drift_is_reported_and_fixed() {
    let s = Sandbox::new();
    let applied = s.ok(&["apply"]);
    assert!(applied.contains("6 to add"), "{applied}");
    assert_eq!(fs::read_to_string(s.demo("token")).unwrap(), "demo-api-token-1234\n");
    assert!(fs::symlink_metadata(s.demo("rc.conf")).unwrap().file_type().is_symlink());
    let app: serde_json::Value = serde_json::from_str(&fs::read_to_string(s.demo("app.json")).unwrap()).unwrap();
    assert_eq!(app["model"], "opus");

    assert!(s.ok(&["plan"]).contains("no changes"));

    fs::write(s.demo("notes.txt"), "edited by an app\n").unwrap();
    let plan = s.ok(&["plan"]);
    assert!(plan.contains("notes.txt") && plan.contains("content changed"), "{plan}");

    s.ok(&["apply"]);
    assert_eq!(fs::read_to_string(s.demo("notes.txt")).unwrap(), "Managed from a file in the repo.\n");
    assert!(s.ok(&["plan"]).contains("no changes"));
}

#[test]
fn adopt_pulls_an_edit_into_the_repo() {
    let s = Sandbox::new();
    s.ok(&["apply"]);
    fs::write(s.demo("notes.txt"), "kept\n").unwrap();
    let out = s.ok(&["adopt", "~/.config/fspec-demo/notes.txt"]);
    assert!(out.contains("copied"), "{out}");
    assert_eq!(fs::read_to_string(s.dir.path().join("spec/files/notes.txt")).unwrap(), "kept\n");
    assert!(s.ok(&["plan"]).contains("no changes"));

    fs::write(s.demo("settings.json"), "{}").unwrap();
    let out = s.fspec(&["adopt", "~/.config/fspec-demo/settings.json"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("-  \"theme\""), "{}", String::from_utf8_lossy(&out.stdout));
}

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use fspec_core::Lock;
use fspec_core::lock::LockedSystem;

const SPEC: &str = r#"
hosts = [
    host(
        name = "box",
        platform = "x86_64-linux",
        system = image(base = "ghcr.io/ublue-os/bazzite"),
        persist = ["/var/lib/docker", "/etc/ssh"],
    ),
    host(
        name = "void",
        platform = "x86_64-linux",
        system = void(packages = ["base-system"]),
    ),
]
"#;

fn fspec(work: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fspec"))
        .arg("--spec")
        .arg(work.join("spec.star"))
        .arg("--root")
        .arg(work.join("root"))
        .arg("--home")
        .arg(work.join("root/home/u"))
        .args(args)
        .output()
        .unwrap()
}

fn log(work: &Path) -> Vec<String> {
    let text = fs::read_to_string(work.join("root/var/log/fspec-commands.log")).unwrap_or_default();
    text.lines().map(str::to_string).collect()
}

fn setup() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("spec.star"), SPEC).unwrap();
    dir
}

#[test]
fn a_rooted_distro_switch_only_logs_the_plan() {
    let dir = setup();
    let out = fspec(dir.path(), &["switch", "--distro", "void", "--host", "void"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let lines = log(dir.path());
    let root = dir.path().join("root").display().to_string();
    assert!(lines[0].starts_with(&format!("mkdir -p {root}/run/fspec/switch/top")), "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("xbps-install -S -y -r")));
    assert!(lines.iter().any(|l| l.contains("apply --local --scope system --host void")));
}

#[test]
fn the_host_must_declare_the_target_distro() {
    let dir = setup();
    let out = fspec(dir.path(), &["switch", "--distro", "void", "--host", "box"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("does not declare void"));
    assert!(log(dir.path()).is_empty());
}

#[test]
fn reset_var_stages_the_host_persist_list() {
    let dir = setup();
    let out = fspec(dir.path(), &["switch", "--reset-var", "--host", "box"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(log(dir.path()).last().unwrap().ends_with("/var/lib/fspec/reset-var/pending"));
}

#[test]
fn to_image_needs_a_locked_image_and_then_runs_bootc() {
    let dir = setup();
    let out = fspec(dir.path(), &["switch", "--to-image", "--host", "box"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("fspec update --images"));

    let mut lock = Lock::default();
    lock.system.insert("box".into(), LockedSystem { image: "ghcr.io/o/fspec-box@sha256:ab".into() });
    lock.save(&dir.path().join("fspec.lock")).unwrap();
    let out = fspec(dir.path(), &["switch", "--to-image", "--host", "box"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(log(dir.path())[0].contains("bootc install to-existing-root --acknowledge-destructive"));
}

#[test]
fn adopt_boot_logs_the_new_default_entry() {
    let dir = setup();
    let out = fspec(dir.path(), &["switch", "--adopt-boot", "--host", "void"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let lines = log(dir.path());
    assert!(lines.iter().any(|l| l.ends_with("loader/entries/fspec-void.conf")), "{lines:?}");
    assert!(lines.last().unwrap().ends_with("/run/fspec/switch/top"));
    assert!(!fspec(dir.path(), &["switch", "--adopt-boot", "--host", "box"]).status.success());
}

#[test]
fn forget_logs_the_delete_and_rejects_unknown_distros() {
    let dir = setup();
    assert!(fspec(dir.path(), &["forget", "arch"]).status.success());
    assert!(log(dir.path()).iter().any(|l| l.contains("btrfs subvolume delete --recursive") && l.ends_with("@arch")));
    assert!(!fspec(dir.path(), &["forget", "gentoo"]).status.success());
}

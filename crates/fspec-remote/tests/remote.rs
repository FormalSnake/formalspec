use std::fs;
use std::path::Path;

use anyhow::bail;
use fspec_core::Host;
use fspec_core::runner::{Output, RecordingRunner};
use fspec_remote::{Planned, Remote, apply_all, order};
use serde_json::json;

fn host(name: &str, platform: &str, distro: &str) -> Host {
    let system = if platform.ends_with("darwin") {
        json!(null)
    } else {
        json!({"kind": "distro", "distro": distro, "init": "systemd"})
    };
    serde_json::from_value(json!({"name": name, "platform": platform, "system": system})).unwrap()
}

fn ok(stdout: &str) -> Output {
    Output { status: 0, stdout: stdout.into(), stderr: String::new() }
}

fn probe_json(platform: &str, distro: &str) -> String {
    json!({"hostname": "g815", "platform": platform, "distro": distro}).to_string()
}

const PROBE: &str = "ssh -o ConnectTimeout=5 -o BatchMode=yes g815";

fn fixture(status: &str) -> (tempfile::TempDir, RecordingRunner) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("fspec.lock"), "").unwrap();
    let runner = RecordingRunner::default();
    let git = format!("git -C {}", dir.path().display());
    runner.respond(&format!("{git} rev-parse"), ok("abc123\n"));
    runner.respond(&format!("{git} status"), ok(status));
    runner.respond(&format!("{git} ls-files"), ok("spec.star\nmodules/cli.star\n"));
    (dir, runner)
}

fn remote<'a>(dir: &Path, runner: &'a RecordingRunner, dirty: bool) -> Remote<'a> {
    Remote { runner, spec_dir: dir.to_path_buf(), spec_file: "spec.star".into(), dirty }
}

fn apply_lines(dir: &Path, runner: &RecordingRunner) -> Vec<String> {
    let git = format!("git -C {}", dir.display());
    runner.lines().into_iter().filter(|l| !l.starts_with(&git)).collect()
}

#[test]
fn apply_probes_ships_then_runs_with_a_terminal() {
    let (dir, runner) = fixture("");
    runner.respond(&format!("{PROBE} 'fspec facts --json'"), ok(&probe_json("x86_64-linux", "bazzite")));
    let h = host("g815", "x86_64-linux", "bazzite");
    remote(dir.path(), &runner, false).apply(&h, "g815", None).unwrap();

    let lines = apply_lines(dir.path(), &runner);
    let tar = lines[1].clone();
    assert!(tar.starts_with("tar -cf ") && tar.ends_with(&format!(" -C {} -T -", dir.path().display())), "{tar}");
    let dest = "\"$HOME/.cache/fspec/spec/abc123\"";
    assert_eq!(lines[0], format!("{PROBE} 'fspec facts --json'"));
    assert_eq!(lines[2], format!("{PROBE} 'mkdir -p {dest} && tar -xf - -C {dest}'"));
    assert_eq!(lines[3], format!("ssh -t g815 'fspec --spec {dest}/spec.star apply --local --host g815'"));
    assert_eq!(lines.len(), 4);

    let calls = runner.calls.lock().unwrap();
    let tar = calls.iter().find(|c| c.program == "tar").unwrap();
    assert_eq!(tar.stdin.as_deref(), Some("spec.star\nmodules/cli.star\nfspec.lock"));
    assert!(calls.last().unwrap().interactive);
}

#[test]
fn probe_mismatch_fails_before_shipping() {
    let (dir, runner) = fixture("");
    runner.respond(PROBE, ok(&probe_json("aarch64-linux", "fedora")));
    let h = host("g815", "x86_64-linux", "bazzite");
    let err = remote(dir.path(), &runner, false).apply(&h, "g815", None).unwrap_err();
    assert_eq!(
        err.to_string(),
        "host g815 is declared as x86_64-linux/bazzite but the machine is aarch64-linux/fedora"
    );
    assert_eq!(runner.lines().len(), 1);
}

#[test]
fn dirty_tree_is_refused_unless_allowed() {
    let (dir, runner) = fixture(" M spec.star\n");
    runner.respond(PROBE, ok(&probe_json("x86_64-linux", "bazzite")));
    let h = host("g815", "x86_64-linux", "bazzite");
    let err = remote(dir.path(), &runner, false).apply(&h, "g815", None).unwrap_err();
    assert!(err.to_string().contains("uncommitted changes") && err.to_string().contains("--dirty"), "{err}");
    assert!(!runner.lines().iter().any(|l| l.starts_with("tar") || l.starts_with("ssh -t")));

    remote(dir.path(), &runner, true).apply(&h, "g815", None).unwrap();
    let lines = runner.lines();
    assert!(lines.iter().any(|l| l.contains("rm -rf \"$HOME/.cache/fspec/spec/abc123-dirty\"")));
}

#[test]
fn missing_fspec_on_the_remote_gives_an_install_hint() {
    let (dir, runner) = fixture("");
    runner.respond(PROBE, Output { status: 127, stdout: String::new(), stderr: "fspec: not found".into() });
    let h = host("g815", "x86_64-linux", "bazzite");
    let err = remote(dir.path(), &runner, false).apply(&h, "g815", None).unwrap_err();
    assert!(err.to_string().contains("fspec is not installed on g815"), "{err}");
}

#[test]
fn plan_returns_the_remote_plan() {
    let (dir, runner) = fixture("");
    runner.respond(&format!("{PROBE} 'fspec facts"), ok(&probe_json("x86_64-linux", "bazzite")));
    runner.respond(&format!("{PROBE} 'fspec --spec"), ok(r#"{"host":"g815","changes":[]}"#));
    let h = host("g815", "x86_64-linux", "bazzite");
    let Planned::Plan(plan) = remote(dir.path(), &runner, false).plan(&h, "g815").unwrap() else {
        panic!("expected a plan")
    };
    assert_eq!(plan.host, "g815");
    assert!(runner.lines().last().unwrap().ends_with("plan --json --local --host g815'"));
}

#[test]
fn unreachable_host_is_skipped_in_plan_and_fails_apply() {
    let (dir, runner) = fixture("");
    runner.respond(PROBE, Output { status: 255, stdout: String::new(), stderr: "timed out".into() });
    let h = host("g815", "x86_64-linux", "bazzite");
    let r = remote(dir.path(), &runner, false);
    assert!(matches!(r.plan(&h, "g815").unwrap(), Planned::Unreachable));
    assert_eq!(runner.lines().len(), 1);
    let err = r.apply(&h, "g815", None).unwrap_err();
    assert!(err.to_string().contains("host g815 is unreachable over ssh (g815): timed out"), "{err}");
}

fn fleet() -> Vec<Host> {
    vec![
        host("macbook", "aarch64-darwin", ""),
        host("g815", "x86_64-linux", "bazzite"),
        host("e1504g", "x86_64-linux", "bazzite"),
    ]
}

#[test]
fn all_applies_remote_hosts_first_and_this_machine_last() {
    let hosts = fleet();
    let names: Vec<_> = order(&hosts, "macbook").iter().map(|h| h.name.as_str()).collect();
    assert_eq!(names, ["g815", "e1504g", "macbook"]);

    let mut seen = Vec::new();
    apply_all(&hosts, "macbook", |h| {
        seen.push(h.name.clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(seen, ["g815", "e1504g", "macbook"]);
}

#[test]
fn all_stops_at_the_first_failure_and_summarises() {
    let hosts = fleet();
    let mut seen = Vec::new();
    let err = apply_all(&hosts, "macbook", |h| {
        seen.push(h.name.clone());
        if h.name == "e1504g" { bail!("sudo failed") } else { Ok(()) }
    })
    .unwrap_err();
    assert_eq!(seen, ["g815", "e1504g"]);
    assert_eq!(err.to_string(), "host e1504g failed: sudo failed\nconverged: g815\nnot attempted: macbook");
}

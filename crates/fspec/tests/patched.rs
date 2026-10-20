use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use fspec_core::Lock;
use fspec_core::lock::{Artifact, LockedPkg};
use fspec_store::sri_sha256;

const SPEC: &str = r#"
hosts = [
    host(
        name = "macbook",
        platform = "aarch64-darwin",
        modules = [
            lambda host: [
                pkg("shout", source = npm("@acme/shout"), patches = glob("patches/*.patch")),
                pkg("librepods", source = git("https://example/librepods", rev = "main"), patches = ["patches/lib.patch2"]),
            ],
        ],
    ),
]
"#;

const PATCH: &str = "\
diff --git a/shout.sh b/shout.sh
--- a/shout.sh
+++ b/shout.sh
@@ -1,2 +1,2 @@
 #!/bin/sh
-echo SHOUT
+echo PATCHED SHOUT
";

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

fn tarball(work: &Path) -> std::path::PathBuf {
    let src = work.join("src/package");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("package.json"), r#"{"name": "@acme/shout", "bin": {"shout": "shout.sh"}}"#).unwrap();
    fs::write(src.join("shout.sh"), "#!/bin/sh\necho SHOUT\n").unwrap();
    let out = work.join("shout-1.0.0.tgz");
    let status =
        Command::new("tar").arg("-czf").arg(&out).arg("-C").arg(work.join("src")).arg("package").status().unwrap();
    assert!(status.success());
    out
}

#[test]
fn a_patched_npm_package_applies_into_the_rooted_store() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path();
    fs::create_dir_all(work.join("patches")).unwrap();
    fs::write(work.join("patches/shout.patch"), PATCH).unwrap();
    // The git package only has to evaluate; this test applies the npm one.
    fs::write(work.join("patches/lib.patch2"), "unused").unwrap();
    fs::write(work.join("spec.star"), SPEC.replace(", patches = [\"patches/lib.patch2\"]", "")).unwrap();

    let tgz = tarball(work);
    let patches = vec!["patches/shout.patch".to_string()];
    let mut lock = Lock::default();
    lock.upsert(LockedPkg {
        name: "shout".into(),
        source: "npm:@acme/shout".into(),
        version: Some("1.0.0".into()),
        rev: None,
        patches: Some(fspec_core::patches::hash(work, &patches).unwrap()),
        artifacts: BTreeMap::from([(
            "aarch64-darwin".to_string(),
            Artifact {
                url: Some(format!("file://{}", tgz.display())),
                hash: Some(sri_sha256(&fs::read(&tgz).unwrap())),
                ..Default::default()
            },
        )]),
    });
    lock.upsert(LockedPkg {
        name: "librepods".into(),
        source: "git:https://example/librepods@main".into(),
        version: None,
        rev: Some("4f1c2a9".into()),
        patches: None,
        artifacts: BTreeMap::new(),
    });
    lock.save(&work.join("fspec.lock")).unwrap();

    let out = fspec(work, &["apply", "--host", "macbook"]);
    assert!(!out.status.success(), "librepods has no artifact, so the apply must stop");
    assert!(String::from_utf8_lossy(&out.stderr).contains("librepods"), "{}", String::from_utf8_lossy(&out.stderr));

    fs::write(work.join("spec.star"), SPEC.replace(
        "                pkg(\"librepods\", source = git(\"https://example/librepods\", rev = \"main\"), patches = [\"patches/lib.patch2\"]),\n",
        "",
    ))
    .unwrap();
    let mut lock = Lock::load(&work.join("fspec.lock")).unwrap();
    lock.pkg.retain(|p| p.name == "shout");
    lock.save(&work.join("fspec.lock")).unwrap();
    let out = fspec(work, &["apply", "--host", "macbook"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let shout = work.join("root/opt/fspec/current/bin/shout");
    let run = Command::new(&shout).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "PATCHED SHOUT");
    assert!(fs::read_to_string(shout.canonicalize().unwrap()).unwrap().contains("PATCHED SHOUT"));

    fs::write(work.join("patches/shout.patch"), PATCH.replace("PATCHED", "CHANGED")).unwrap();
    let out = fspec(work, &["apply", "--host", "macbook"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("patches changed, run fspec update"));
}

#[test]
fn ci_packages_writes_the_workflow() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path();
    fs::create_dir_all(work.join("patches")).unwrap();
    fs::write(work.join("patches/lib.patch2"), "x").unwrap();
    fs::write(work.join("spec.star"), SPEC).unwrap();
    let out = fspec(work, &["ci", "packages"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = fs::read_to_string(work.join(".github/workflows/fspec-packages.yml")).unwrap();
    assert!(text.contains("{ pkg: librepods, platform: aarch64-darwin, runner: macos-14 }"), "{text}");
}

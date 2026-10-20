use std::collections::BTreeMap;
use std::sync::Mutex;

use fspec_core::runner::Output;

use super::*;

/// Records commands and plays the parts of git and the build tools that leave files behind.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    /// Files `git checkout` leaves in the checkout.
    checkout: BTreeMap<&'static str, &'static str>,
    /// Files the build command leaves, as `(directory marker, name)`: the marker is replaced
    /// by the value after `--target-dir ` or `--prefix`-style output directories.
    produce: Vec<(&'static str, &'static str)>,
}

impl Runner for Fake {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        self.calls.lock().unwrap().push(cmd.to_string());
        let arg = |flag: &str| cmd.args.iter().position(|a| a == flag).map(|i| cmd.args[i + 1].clone());
        if cmd.program == "git" && cmd.args.contains(&"checkout".to_string()) {
            let src = PathBuf::from(&cmd.args[1]);
            fs::create_dir_all(&src).unwrap();
            for (name, text) in &self.checkout {
                fs::write(src.join(name), text).unwrap();
            }
        }
        if matches!(cmd.program.as_str(), "cargo" | "sh") {
            let dir = match cmd.program.as_str() {
                "cargo" => PathBuf::from(arg("--target-dir").unwrap()).join("release"),
                _ => PathBuf::from(&cmd.env.iter().find(|(k, _)| k == "FSPEC_OUT").unwrap().1).join("bin"),
            };
            fs::create_dir_all(&dir).unwrap();
            for (kind, name) in &self.produce {
                let path = dir.join(name);
                fs::write(&path, kind).unwrap();
                let mode = if *kind == "exe" { 0o755 } else { 0o644 };
                fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            }
        }
        Ok(Output::default())
    }
}

use std::os::unix::fs::PermissionsExt;

fn pkg(extra: serde_json::Value) -> Pkg {
    let mut v =
        serde_json::json!({"name": "tool", "source": {"kind": "git", "url": "https://example/tool", "rev": "main"}});
    v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    serde_json::from_value(v).unwrap()
}

fn job<'a>(pkg: &'a Pkg, spec: &'a Path, out: &'a Path) -> Job<'a> {
    Job { pkg, url: "https://example/tool", rev: "abc123", platform: "x86_64-linux", spec_dir: spec, out }
}

fn names(archive: &Path) -> Vec<String> {
    let bytes = fs::read(archive).unwrap();
    let dec = ruzstd::decoding::StreamingDecoder::new(bytes.as_slice()).unwrap();
    tar::Archive::new(dec)
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn cargo_build_runs_the_exact_sequence_and_packs_executables_at_the_top() {
    let spec = tempfile::tempdir().unwrap();
    fs::create_dir_all(spec.path().join("patches")).unwrap();
    fs::write(spec.path().join("patches/one.patch"), "p").unwrap();
    let out = tempfile::tempdir().unwrap();
    let p = pkg(serde_json::json!({"patches": ["patches/one.patch"], "build_flags": ["--features", "pipewire"]}));
    let fake = Fake {
        checkout: BTreeMap::from([("Cargo.toml", "")]),
        produce: vec![("exe", "tool"), ("data", "tool.d"), ("data", "libtool.rlib")],
        ..Default::default()
    };
    let built = build(&fake, &job(&p, spec.path(), out.path())).unwrap();

    let calls = fake.calls.lock().unwrap();
    let calls: Vec<String> = calls.iter().map(|c| c.replace(&spec.path().display().to_string(), "<spec>")).collect();
    assert!(calls[0].starts_with("git init -q "), "{calls:?}");
    assert!(calls[1].contains("remote add origin https://example/tool"));
    assert!(calls[2].ends_with("fetch -q --depth 1 origin abc123"));
    assert!(calls[3].ends_with("checkout -q FETCH_HEAD"));
    assert!(calls[4].ends_with("apply <spec>/patches/one.patch"));
    assert!(calls[5].starts_with("cargo build --release --locked --manifest-path "));
    assert!(calls[5].ends_with("--features pipewire"));
    assert_eq!(calls.len(), 6);

    assert_eq!(built.tag, "abc123-x86_64-linux");
    assert_eq!(built.archive, out.path().join("tool-x86_64-linux.tar.zst"));
    assert_eq!(names(&built.archive), ["tool"]);
    assert_eq!(fs::read_to_string(out.path().join("tag")).unwrap(), "abc123-x86_64-linux");
}

#[test]
fn explicit_build_and_named_bins() {
    let spec = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let p = pkg(serde_json::json!({"build": ["sh", "-c", "make && cp x $FSPEC_OUT/bin"], "bins": ["b", "a"]}));
    let fake = Fake {
        checkout: BTreeMap::from([("Cargo.toml", "")]),
        produce: vec![("exe", "a"), ("exe", "b"), ("exe", "c")],
        ..Default::default()
    };
    let built = build(&fake, &job(&p, spec.path(), out.path())).unwrap();
    assert!(fake.calls.lock().unwrap().last().unwrap().starts_with("sh -c 'make && cp x"));
    assert_eq!(names(&built.archive), ["b", "a"]);
}

#[test]
fn a_missing_named_bin_fails() {
    let spec = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let p = pkg(serde_json::json!({"bins": ["ghost"]}));
    let fake =
        Fake { checkout: BTreeMap::from([("Cargo.toml", "")]), produce: vec![("exe", "tool")], ..Default::default() };
    let err = build(&fake, &job(&p, spec.path(), out.path())).unwrap_err();
    assert!(err.to_string().contains("ghost"), "{err}");
}

#[test]
fn a_checkout_without_a_build_system_fails() {
    let spec = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let p = pkg(serde_json::json!({}));
    let fake = Fake { checkout: BTreeMap::from([("README", "")]), ..Default::default() };
    assert!(build(&fake, &job(&p, spec.path(), out.path())).is_err());
}

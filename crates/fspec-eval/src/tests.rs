use std::fs;
use std::path::{Path, PathBuf};

use fspec_core::resource::{Resource, Source};

use super::evaluate;

fn example() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/basic/spec.star")
}

/// Write `spec.star` (plus any extra files) into a temp dir and evaluate it.
fn eval_str(spec: &str, extra: &[(&str, &str)]) -> anyhow::Result<super::Spec> {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("spec.star"), spec).unwrap();
    for (path, text) in extra {
        let p = dir.path().join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    evaluate(&dir.path().join("spec.star"))
}

#[test]
fn example_spec() {
    let spec = evaluate(&example()).unwrap();
    assert_eq!(spec.hosts.len(), 3);

    let g815 = spec.host("g815").unwrap();
    let keys: Vec<String> = g815.desired().iter().map(|d| d.resource.key().to_string()).collect();
    assert!(keys.contains(&"image:system".into()));
    assert!(keys.contains(&"user:kyandesutter".into()));
    assert!(keys.contains(&"service:syncthing".into()));
    assert!(!keys.contains(&"pkg:aerospace".into()));
    let syncthing = g815.desired().into_iter().find(|d| d.resource.key().to_string() == "service:syncthing").unwrap();
    assert_eq!(syncthing.owner.as_deref(), Some("syncthing"));

    let mac = spec.host("macbook").unwrap();
    let aerospace = mac.resources.iter().find_map(|r| match r {
        Resource::Pkg(p) if p.name == "aerospace" => Some(p.source.clone()),
        _ => None,
    });
    assert_eq!(aerospace, Some(Source::Brew { name: None, cask: true }));
    let rg = mac.resources.iter().find_map(|r| match r {
        Resource::Pkg(p) if p.name == "ripgrep" => Some(p.source.clone()),
        _ => None,
    });
    assert_eq!(rg, Some(Source::Aqua { name: "ripgrep".into() }));
}

#[test]
fn facts_reach_modules() {
    let spec = eval_str(
        r#"
def m(host):
    return [pkg("x-" + host.distro + "-" + host.init + "-" + host.platform.arch)]
hosts = [
    host("a", platform = "aarch64-linux", system = void(), modules = [m]),
    host("b", platform = "x86_64-linux", system = image(base = "quay.io/fedora/fedora-silverblue:42"), modules = [m]),
]
"#,
        &[],
    )
    .unwrap();
    let names: Vec<String> = spec.hosts.iter().map(|h| h.resources[0].key().id).collect();
    assert_eq!(names, ["x-void-runit-aarch64", "x-fedora-systemd-x86_64"]);
}

#[test]
fn unknown_field_fails_at_construction() {
    let err = eval_str(r#"hosts = [pkg("rg", sorce = npm("x"))]"#, &[]).unwrap_err();
    assert!(format!("{err:#}").contains("unknown field `sorce`"), "{err:#}");
}

#[test]
fn platform_is_required() {
    let err = eval_str(r#"hosts = [host("a")]"#, &[]).unwrap_err();
    assert!(format!("{err:#}").contains("platform is required"), "{err:#}");
}

#[test]
fn linux_needs_a_system() {
    let err = eval_str(r#"hosts = [host("a", platform = "x86_64-linux")]"#, &[]).unwrap_err();
    assert!(format!("{err:#}").contains("needs system"), "{err:#}");
}

#[test]
fn load_paths_are_repo_rooted() {
    let err = eval_str(r#"load("m.star", "m")"#, &[("m.star", "m = 1")]).unwrap_err();
    assert!(format!("{err:#}").contains("paths start with //"), "{err:#}");

    let spec = eval_str(
        r#"
load("//lib/a.star", "a")
hosts = [host("h", platform = "aarch64-darwin", modules = [a()])]
"#,
        &[
            ("lib/a.star", "load(\"//lib/b.star\", \"b\")\ndef a():\n    return [b]\n"),
            ("lib/b.star", "b = pkg(\"jq\")\n"),
        ],
    )
    .unwrap();
    assert_eq!(spec.hosts[0].resources[0].key().id, "jq");
}

#[test]
fn load_cycle_is_reported() {
    let err = eval_str(
        r#"load("//a.star", "a")"#,
        &[("a.star", "load(\"//b.star\", \"b\")\na = 1"), ("b.star", "load(\"//a.star\", \"a\")\nb = 1")],
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("load cycle"), "{err:#}");
}

#[test]
fn image_is_not_a_module() {
    let err = eval_str(r#"hosts = [host("h", platform = "aarch64-darwin", modules = [image(base = "x")])]"#, &[])
        .unwrap_err();
    assert!(format!("{err:#}").contains("goes in host(system"), "{err:#}");
}

#[test]
fn pkg_files_must_be_files() {
    let err = eval_str(r#"pkg("x", files = [pkg("y")])"#, &[]).unwrap_err();
    assert!(format!("{err:#}").contains("takes file(...) values"), "{err:#}");
}

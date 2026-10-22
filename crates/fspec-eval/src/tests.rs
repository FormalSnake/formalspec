use std::fs;
use std::path::{Path, PathBuf};

use fspec_core::resource::{Resource, Source};

use super::{evaluate, evaluate_recipients};

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

#[test]
fn glob_lists_sorted_repo_relative_files() {
    let spec = r#"
names = glob("patches/*.patch")
hosts = [host(name = "m", platform = "aarch64-darwin", modules = [
    pkg("tool", source = git("https://example/tool", rev = "main"), patches = names),
])]
"#;
    let extra =
        [("patches/b.patch", "b"), ("patches/a.patch", "a"), ("patches/skip.txt", "x"), ("patches/sub/c.patch", "c")];
    let parsed = eval_str(spec, &extra).unwrap();
    let Some(Resource::Pkg(p)) = parsed.host("m").unwrap().resources.first() else { panic!("no pkg") };
    assert_eq!(p.patches, ["patches/a.patch", "patches/b.patch"]);
    let deep = eval_str(&spec.replace("patches/*.patch", "patches/**/*.patch"), &extra).unwrap();
    let Some(Resource::Pkg(p)) = deep.host("m").unwrap().resources.first() else { panic!("no pkg") };
    assert_eq!(p.patches, ["patches/a.patch", "patches/b.patch", "patches/sub/c.patch"]);
}

#[test]
fn glob_stays_inside_the_spec_directory() {
    for pattern in ["../*.patch", "/etc/*"] {
        let err = eval_str(&format!("x = glob(\"{pattern}\")\nhosts = []\n"), &[]).unwrap_err();
        assert!(format!("{err:#}").contains("relative to the spec's directory"), "{err:#}");
    }
}

#[test]
fn glob_works_inside_loaded_modules() {
    let spec = "load(\"//m.star\", \"found\")\nhosts = [host(name = \"m\", platform = \"aarch64-darwin\", modules = [pkg(\"t\", patches = found)])]\n";
    let parsed = eval_str(spec, &[("m.star", "found = glob(\"p/*.patch\")\n"), ("p/one.patch", "1")]).unwrap();
    let Some(Resource::Pkg(p)) = parsed.host("m").unwrap().resources.first() else { panic!("no pkg") };
    assert_eq!(p.patches, ["p/one.patch"]);
}

fn dev_str(text: &str, platform: &str) -> anyhow::Result<fspec_core::dev::Dev> {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("dev.star"), text).unwrap();
    super::evaluate_dev(&dir.path().join("dev.star"), platform)
}

#[test]
fn dev_star_declares_a_project_environment_per_platform() {
    let text = r#"
dev(
    packages = [pkg("just", version = "1.40.0"), pkg("bun", source = github("oven-sh/bun"))],
    libs = [lib("openssl"), lib("opus", apt = "libopus-dev")] + ([lib("dbus-1")] if platform.is_linux else []),
    bins = [bin("pkg-config")],
    env = {"RUST_BACKTRACE": "1"},
    services = [service("postgres", version = "17")],
    shell_hook = "export A=1",
)
"#;
    let linux = dev_str(text, "x86_64-linux").unwrap();
    assert_eq!(linux.packages[0].version.as_deref(), Some("1.40.0"));
    assert_eq!(linux.packages[1].source, Source::Github { repo: "oven-sh/bun".into(), asset: None });
    assert_eq!(linux.libs.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["openssl", "opus", "dbus-1"]);
    assert_eq!(linux.libs[1].overrides.get("apt").map(String::as_str), Some("libopus-dev"));
    assert_eq!(linux.bins[0].name, "pkg-config");
    assert_eq!(linux.services[0].image_ref().as_deref(), Some("postgres:17"));
    assert_eq!(linux.platforms.len(), 4);
    let mac = dev_str(text, "aarch64-darwin").unwrap();
    assert_eq!(mac.libs.len(), 2);
}

#[test]
fn dev_star_must_call_dev_once_and_dev_stays_out_of_specs() {
    assert!(dev_str("x = 1", "x86_64-linux").unwrap_err().to_string().contains("must call dev(...)"));
    assert!(dev_str("dev()\ndev()", "x86_64-linux").unwrap_err().to_string().contains("more than once"));
    let err = eval_str("dev()\nhosts = []", &[]).unwrap_err();
    assert!(format!("{err:#}").contains("belongs in a project's dev.star"), "{err:#}");
    let err = dev_str("dev(libs = [lib(\"x\", ubuntu = \"y\")])", "x86_64-linux").unwrap_err();
    assert!(format!("{err:#}").contains("unknown field `ubuntu`"), "{err:#}");
    let err = dev_str("dev(services = [service(\"pg\", restart = \"always\")])", "x86_64-linux").unwrap_err();
    assert!(format!("{err:#}").contains("restart"), "{err:#}");
}

#[test]
fn tumbleweed_declares_zypper_packages_and_repos() {
    let spec = eval_str(
        r#"
hosts = [host(
    name = "e1504g",
    platform = "x86_64-linux",
    system = tumbleweed(packages = ["NetworkManager"]),
    modules = [
        repo("1password", url = "https://downloads.1password.com/linux/rpm/stable/x86_64",
             key = "https://downloads.1password.com/linux/keys/1password.asc",
             fingerprint = "3FEF9748469ADBE15DA7CA80AC2D62742012EA22"),
        pkg("1password", source = zypper()),
        pkg("tools", source = zypper(["htop", "jq"])),
    ],
)]
"#,
        &[],
    )
    .unwrap();
    let host = spec.host("e1504g").unwrap();
    let Some(fspec_core::resource::System::Distro(d)) = &host.system else { panic!("{:?}", host.system) };
    assert_eq!(d.distro, "opensuse-tumbleweed");
    assert!(matches!(&host.resources[0], Resource::Repo(r) if r.name == "1password" && r.key.is_some()));
    let Resource::Pkg(tools) = &host.resources[2] else { panic!() };
    assert!(matches!(&tools.source, Source::Zypper(z) if z.names == ["htop", "jq"]));
    assert!(eval_str("x = repo(\"a\", url = \"u\", gpg = \"k\")\nhosts = []\n", &[]).is_err());
}

#[test]
fn recipients_come_from_secrets_star() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secrets.star");
    std::fs::write(&path, "k = \"age1x\"\nsecrets = {\"secrets/\" + n + \".age\": [k] for n in [\"a\", \"b\"]}\n")
        .unwrap();
    let all = evaluate_recipients(&path).unwrap();
    assert_eq!(all["secrets/a.age"], ["age1x"]);
    assert_eq!(all.len(), 2);
    std::fs::write(&path, "secrets = {\"a.age\": \"age1x\"}\n").unwrap();
    assert!(evaluate_recipients(&path).is_err());
}

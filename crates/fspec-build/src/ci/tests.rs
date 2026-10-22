use fspec_core::resource::Host;
use serde_json::json;

use std::collections::BTreeMap;

use super::*;

fn host(name: &str, platform: &str, pkgs: &[(&str, &str)]) -> Host {
    let resources: Vec<_> = pkgs
        .iter()
        .map(|(n, kind)| match *kind {
            "git" => {
                json!({"kind": "pkg", "name": n, "source": {"kind": "git", "url": "https://example/x", "rev": "main"}})
            }
            _ => json!({"kind": "pkg", "name": n, "source": {"kind": "aqua", "name": n}}),
        })
        .collect();
    serde_json::from_value(json!({"name": name, "platform": platform, "resources": resources})).unwrap()
}

#[test]
fn jobs_are_the_git_packages_per_declared_platform() {
    let hosts = [
        host("g815", "x86_64-linux", &[("librepods", "git"), ("ripgrep", "aqua")]),
        host("e1504g", "x86_64-linux", &[("librepods", "git")]),
        host("macbook", "aarch64-darwin", &[("librepods", "git"), ("other", "git")]),
    ];
    assert_eq!(
        jobs(&hosts),
        [
            ("librepods".to_string(), "aarch64-darwin".to_string()),
            ("librepods".to_string(), "x86_64-linux".to_string()),
            ("other".to_string(), "aarch64-darwin".to_string()),
        ]
    );
}

#[test]
fn workflow_golden() {
    let jobs = vec![
        ("librepods".to_string(), "aarch64-darwin".to_string()),
        ("librepods".to_string(), "x86_64-linux".to_string()),
    ];
    let expected = include_str!("golden.yml").replace("{{repo}}", env!("CARGO_PKG_REPOSITORY"));
    assert_eq!(workflow(&jobs, "spec.star", &BTreeMap::new()).unwrap(), expected);
    let nested = workflow(&jobs, "examples/kyan/spec.star", &BTreeMap::new()).unwrap();
    assert!(nested.contains(r#"paths: ["examples/kyan/fspec.lock", "examples/kyan/patches/**"]"#), "{nested}");
    assert!(nested.contains("fspec --spec examples/kyan/spec.star build --pkg"), "{nested}");
}

#[test]
fn build_deps_ride_on_their_matrix_entry() {
    let jobs = vec![("a".to_string(), "x86_64-linux".to_string()), ("b".to_string(), "x86_64-linux".to_string())];
    let deps = BTreeMap::from([("a".to_string(), vec!["libasound2-dev".to_string(), "pkg-config".to_string()])]);
    let text = workflow(&jobs, "spec.star", &deps).unwrap();
    assert!(
        text.contains(
            "- { pkg: a, platform: x86_64-linux, runner: ubuntu-24.04, deps: \"libasound2-dev pkg-config\" }"
        )
    );
    assert!(text.contains("- { pkg: b, platform: x86_64-linux, runner: ubuntu-24.04 }"));
}

#[test]
fn unknown_platforms_and_empty_job_lists_fail() {
    assert!(workflow(&[], "spec.star", &BTreeMap::new()).is_err());
    assert!(workflow(&[("p".into(), "riscv64-linux".into())], "spec.star", &BTreeMap::new()).is_err());
}

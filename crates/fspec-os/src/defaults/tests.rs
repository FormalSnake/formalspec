use fspec_core::Scope;
use fspec_core::apply::{Backend, apply_scope, plan_scope};
use fspec_core::facts::Init;
use fspec_core::plan::Action;
use fspec_core::runner::Output;
use fspec_core::state::State;
use serde_json::json;

use super::*;
use crate::testutil::{World, desired, resource};

fn def(domain: &str, key: &str, value: serde_json::Value) -> Resource {
    resource(json!({"kind": "defaults", "domain": domain, "key": key, "value": value}))
}

/// What `defaults export <domain> -` prints for a domain holding `body`.
fn export(body: &str) -> Output {
    let text = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>{body}</dict>\n</plist>\n"
    );
    Output { stdout: text, ..Output::default() }
}

fn args(v: serde_json::Value) -> Vec<String> {
    write_args(&to_plist(&v).unwrap()).unwrap()
}

#[test]
fn types_follow_the_json_value() {
    assert_eq!(args(json!(true)), ["-bool", "true"]);
    assert_eq!(args(json!(48)), ["-int", "48"]);
    assert_eq!(args(json!(0.5)), ["-float", "0.5"]);
    assert_eq!(args(json!("Nlsv")), ["-string", "Nlsv"]);
    assert_eq!(args(json!([1, "a", false])), ["-array", "<integer>1</integer>", "<string>a</string>", "<false/>"]);
    assert_eq!(
        args(json!({"b": 2.5, "a": {"x": true}})),
        ["-dict", "b", "<real>2.5</real>", "a", "<dict><key>x</key><true/></dict>"]
    );
}

#[test]
fn check_rejects_null_and_odd_domains() {
    let w = World::darwin();
    let cx = w.cx(Scope::User);
    assert!(DefaultsBackend.check(&cx, &def("com.apple.dock", "k", json!(null))).is_err());
    assert!(DefaultsBackend.check(&cx, &def("com.apple.dock", "k", json!([null]))).is_err());
    assert!(DefaultsBackend.check(&cx, &def("/tmp/x", "k", json!(1))).is_err());
    assert!(DefaultsBackend.check(&cx, &def("-g", "k", json!(1))).is_err());
    assert!(DefaultsBackend.check(&cx, &def("NSGlobalDomain", "k", json!(1))).is_ok());
}

#[test]
fn claims_only_darwin() {
    let r = def("com.apple.dock", "autohide", json!(true));
    assert!(DefaultsBackend.handles(&World::darwin().cx(Scope::User), &r));
    assert!(!DefaultsBackend.handles(&World::linux(Init::Systemd).cx(Scope::User), &r));
}

#[test]
fn writes_the_value_and_skips_a_matching_one() {
    let w = World::darwin();
    let backends: &[&dyn Backend] = &[&DefaultsBackend];
    w.runner.respond("defaults export com.apple.dock -", export("<key>autohide</key><false/>"));
    let want = vec![desired(def("com.apple.dock", "autohide", json!(true)))];
    apply_scope(&w.cx(Scope::User), backends, &want).unwrap();
    assert_eq!(
        w.runner.lines(),
        ["defaults export com.apple.dock -", "defaults write com.apple.dock autohide -bool true"]
    );

    let w = World::darwin();
    w.runner.respond("defaults export com.apple.dock -", export("<key>autohide</key><true/>"));
    apply_scope(&w.cx(Scope::User), backends, &want).unwrap();
    assert_eq!(w.runner.lines(), ["defaults export com.apple.dock -"]);
}

#[test]
fn rollback_restores_previous_values_and_deletes_new_keys() {
    let w = World::darwin();
    let backends: &[&dyn Backend] = &[&DefaultsBackend];
    w.runner.respond("defaults export com.a -", export("<key>k</key><false/>"));
    w.runner.respond("defaults export com.b -", export(""));
    w.runner.respond("defaults write com.b", Output { status: 1, ..Output::default() });
    let want = vec![desired(def("com.a", "k", json!(true))), desired(def("com.b", "k2", json!(["x"])))];
    let err = apply_scope(&w.cx(Scope::User), backends, &want).unwrap_err();
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    assert_eq!(
        w.runner.lines(),
        [
            "defaults export com.a -",
            "defaults write com.a k -bool true",
            "defaults export com.b -",
            "defaults write com.b k2 -array <string>x</string>",
            "defaults delete com.b k2",
            "defaults write com.a k -bool false",
        ]
    );
    assert!(State::load(&w.paths.user_state_dir().join("state.json")).unwrap().entries.is_empty());
}

#[test]
fn removal_deletes_only_our_value() {
    let w = World::darwin();
    let backends: &[&dyn Backend] = &[&DefaultsBackend];
    let want = vec![desired(def("com.apple.dock", "tilesize", json!(48)))];
    apply_scope(&w.cx(Scope::User), backends, &want).unwrap();
    w.runner.calls.lock().unwrap().clear();

    w.runner.respond("defaults export com.apple.dock -", export("<key>tilesize</key><integer>48</integer>"));
    apply_scope(&w.cx(Scope::User), backends, &[]).unwrap();
    assert_eq!(w.runner.lines(), ["defaults export com.apple.dock -", "defaults delete com.apple.dock tilesize"]);

    let w = World::darwin();
    apply_scope(&w.cx(Scope::User), backends, &want).unwrap();
    w.runner.calls.lock().unwrap().clear();
    w.runner.respond("defaults export com.apple.dock -", export("<key>tilesize</key><integer>64</integer>"));
    apply_scope(&w.cx(Scope::User), backends, &[]).unwrap();
    assert_eq!(w.runner.lines(), ["defaults export com.apple.dock -"]);
    assert!(State::load(&w.paths.user_state_dir().join("state.json")).unwrap().entries.is_empty());
}

#[test]
fn drift_compares_with_the_recorded_value() {
    let w = World::darwin();
    let backends: &[&dyn Backend] = &[&DefaultsBackend];
    let want = vec![desired(def("com.apple.dock", "tilesize", json!(48)))];
    apply_scope(&w.cx(Scope::User), backends, &want).unwrap();
    let plan = |w: &World| {
        let state = State::load(&w.paths.user_state_dir().join("state.json")).unwrap();
        plan_scope(&w.cx(Scope::User), backends, &want, &state).unwrap()
    };
    assert!(plan(&w).changes.is_empty());

    w.runner.respond("defaults export com.apple.dock -", export("<key>tilesize</key><integer>64</integer>"));
    let p = plan(&w);
    assert!(matches!(&p.changes[0].action, Action::Drift { notes } if notes == &["value is 64"]));

    let w2 = World::darwin();
    apply_scope(&w2.cx(Scope::User), backends, &want).unwrap();
    w2.runner.respond("defaults export com.apple.dock -", export(""));
    let p = plan(&w2);
    assert!(matches!(&p.changes[0].action, Action::Drift { notes } if notes == &["key is missing"]));
}

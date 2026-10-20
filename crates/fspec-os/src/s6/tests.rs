use std::fs;
use std::os::unix::fs::PermissionsExt;

use fspec_core::Scope;
use fspec_core::apply::{Backend, apply_scope, plan_scope};
use fspec_core::facts::Init;
use fspec_core::plan::Action;
use fspec_core::runner::Output;
use fspec_core::state::State;
use serde_json::json;

use super::*;
use crate::testutil::{World, desired, resource};

fn web(restart: &str) -> Resource {
    resource(json!({
        "kind": "service", "name": "web", "exec": "/usr/bin/webd", "restart": restart,
        "env": {"PORT": "8080"}, "after": ["network", "db"]
    }))
}

fn render(r: &Resource) -> Vec<(String, String)> {
    let w = World::linux(Init::S6);
    let Resource::Service(s) = r else { panic!() };
    S6.parts(&w.cx(Scope::System), s).unwrap().into_iter().map(|p| (p.path, p.content)).collect()
}

#[test]
fn renders_a_longrun_source_directory() {
    let got = render(&web("always"));
    let want = [
        ("/etc/s6/sv/fspec-web/type", "longrun\n"),
        ("/etc/s6/sv/fspec-web/run", "#!/bin/sh\nexec 2>&1\nexport PORT=8080\nexec /usr/bin/webd\n"),
        ("/etc/s6/sv/fspec-web/dependencies.d/fspec-db", ""),
        ("/etc/s6/sv/fspec/contents.d/fspec-web", ""),
    ];
    assert_eq!(got, want.map(|(p, c)| (p.to_string(), c.to_string())));

    let no = render(&web("no"));
    assert_eq!(no[2], ("/etc/s6/sv/fspec-web/finish".into(), "#!/bin/sh\nexit 125\n".into()));
    let on_failure = render(&web("on_failure"));
    assert_eq!(on_failure[2].1, "#!/bin/sh\nif [ \"$1\" = 0 ]; then exit 125; fi\nexit 0\n");
}

#[test]
fn check_rejects_user_services_and_systemd_extras() {
    let w = World::linux(Init::S6);
    let cx = w.cx(Scope::User);
    let user = resource(json!({"kind": "service", "name": "s", "exec": "x", "user": true}));
    assert!(S6Backend.check(&cx, &user).unwrap_err().to_string().contains("no per-user s6"));
    let extras = resource(json!({"kind": "service", "name": "s", "exec": "x", "systemd": {"unit": {}}}));
    let err = S6Backend.check(&cx, &extras).unwrap_err().to_string();
    assert!(err.contains("sets `systemd` (unit)") && err.contains("s6"), "{err}");
}

#[test]
fn claims_only_s6_hosts() {
    assert!(S6Backend.handles(&World::linux(Init::S6).cx(Scope::System), &web("always")));
    assert!(!S6Backend.handles(&World::linux(Init::Runit).cx(Scope::System), &web("always")));
}

fn compile_lines(w: &World, db: &str) -> [String; 2] {
    let root = w.paths.root.display();
    [format!("s6-rc-compile {root}/etc/s6/db/{db} {root}/etc/s6/sv"), format!("s6-rc-update {root}/etc/s6/db/{db}")]
}

#[test]
fn add_update_remove_command_sequences() {
    let w = World::linux(Init::S6);
    let backends: &[&dyn Backend] = &[&S6Backend];
    let sv = w.paths.root.join("etc/s6/sv");

    apply_scope(&w.cx(Scope::System), backends, &[desired(web("always"))]).unwrap();
    assert_eq!(fs::read_to_string(sv.join("fspec/type")).unwrap(), "bundle\n");
    assert_eq!(fs::metadata(sv.join("fspec-web/run")).unwrap().permissions().mode() & 0o777, 0o755);
    let [compile, update] = compile_lines(&w, "fspec-g1-a");
    assert_eq!(w.runner.lines(), [compile, update, "s6-rc -u change fspec".to_string()]);

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[desired(web("no"))]).unwrap();
    assert!(sv.join("fspec-web/finish").exists());
    let [compile, update] = compile_lines(&w, "fspec-g2-a");
    assert_eq!(w.runner.lines(), [compile, update, "s6-rc -u change fspec".to_string()]);

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[]).unwrap();
    assert_eq!(w.runner.lines(), compile_lines(&w, "fspec-g3-r"));
    assert!(!sv.join("fspec-web").exists() && !sv.join("fspec/contents.d/fspec-web").exists());
}

#[test]
fn a_dropped_dependency_leaves_no_stale_file() {
    let w = World::linux(Init::S6);
    let backends: &[&dyn Backend] = &[&S6Backend];
    apply_scope(&w.cx(Scope::System), backends, &[desired(web("always"))]).unwrap();
    let dep = w.paths.root.join("etc/s6/sv/fspec-web/dependencies.d/fspec-db");
    assert!(dep.exists());
    let alone = resource(json!({"kind": "service", "name": "web", "exec": "/usr/bin/webd"}));
    apply_scope(&w.cx(Scope::System), backends, &[desired(alone)]).unwrap();
    assert!(!dep.exists());
}

#[test]
fn failed_update_recompiles_the_restored_sources() {
    let w = World::linux(Init::S6);
    w.runner.respond("s6-rc-update", Output { status: 1, ..Output::default() });
    let err = apply_scope(&w.cx(Scope::System), &[&S6Backend], &[desired(web("always"))]).unwrap_err();
    assert!(format!("{err:#}").contains("apply failed"), "{err:#}");
    assert!(!w.paths.root.join("etc/s6/sv/fspec-web").exists());
    let [c1, u1] = compile_lines(&w, "fspec-g1-a");
    let [c2, u2] = compile_lines(&w, "fspec-g1-a-undo");
    assert_eq!(w.runner.lines(), [c1, u1, c2, u2]);
}

#[test]
fn drift_reports_an_edited_run_script() {
    let w = World::linux(Init::S6);
    let backends: &[&dyn Backend] = &[&S6Backend];
    let want = vec![desired(web("always"))];
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    fs::write(w.paths.root.join("etc/s6/sv/fspec-web/run"), "x").unwrap();
    let state = State::load(&w.paths.system_state_dir().join("state.json")).unwrap();
    let p = plan_scope(&w.cx(Scope::System), backends, &want, &state).unwrap();
    assert!(
        matches!(&p.changes[0].action, Action::Drift { notes } if notes == &["/etc/s6/sv/fspec-web/run was edited"])
    );
}

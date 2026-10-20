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

fn web(exec: &str, restart: &str) -> Resource {
    resource(json!({
        "kind": "service", "name": "web", "exec": exec, "restart": restart, "description": "Web server",
        "env": {"PORT": "8080"}, "after": ["network", "db"]
    }))
}

fn render(r: &Resource) -> String {
    let w = World::linux(Init::Openrc);
    let Resource::Service(s) = r else { panic!() };
    Openrc.parts(&w.cx(Scope::System), s).unwrap().remove(0).content
}

#[test]
fn renders_a_supervised_script() {
    assert_eq!(
        render(&web("webd --port 8080 --name 'a b'", "always")),
        "#!/sbin/openrc-run\n# Managed by fspec. Edits are overwritten on the next apply.\n\n\
         description='Web server'\nsupervisor=supervise-daemon\ncommand=/usr/bin/env\n\
         command_args='webd --port 8080 --name '\\''a b'\\'''\nexport PORT=8080\n\n\
         depend() {\n\tneed net\n\tafter fspec-db\n}\n"
    );
}

#[test]
fn no_restart_drops_the_supervisor() {
    let text = render(&resource(json!({"kind": "service", "name": "x", "exec": "/bin/seed", "restart": "no"})));
    assert_eq!(
        text,
        "#!/sbin/openrc-run\n# Managed by fspec. Edits are overwritten on the next apply.\n\n\
         command_background=true\npidfile=\"/run/${RC_SVCNAME}.pid\"\ncommand=/bin/seed\n"
    );
}

#[test]
fn check_rejects_user_services_and_systemd_extras() {
    let w = World::linux(Init::Openrc);
    let cx = w.cx(Scope::User);
    let user = resource(json!({"kind": "service", "name": "s", "exec": "x", "user": true}));
    let err = OpenrcBackend.check(&cx, &user).unwrap_err().to_string();
    assert_eq!(err, "service:s sets user = True, but OpenRC has no user services");

    let extras = resource(json!({"kind": "service", "name": "s", "exec": "x", "systemd": {"unit": {}}}));
    let err = OpenrcBackend.check(&cx, &extras).unwrap_err().to_string();
    assert!(err.contains("sets `systemd` (unit)") && err.contains("openrc"), "{err}");
}

#[test]
fn claims_only_openrc_hosts() {
    let r = web("x", "always");
    assert!(OpenrcBackend.handles(&World::linux(Init::Openrc).cx(Scope::System), &r));
    assert!(!OpenrcBackend.handles(&World::linux(Init::Dinit).cx(Scope::System), &r));
}

#[test]
fn add_update_remove_command_sequences() {
    let w = World::linux(Init::Openrc);
    let backends: &[&dyn Backend] = &[&OpenrcBackend];
    let script = w.paths.root.join("etc/init.d/fspec-web");

    apply_scope(&w.cx(Scope::System), backends, &[desired(web("/bin/web", "always"))]).unwrap();
    assert_eq!(fs::metadata(&script).unwrap().permissions().mode() & 0o777, 0o755);
    assert_eq!(w.runner.lines(), ["rc-update add fspec-web default", "rc-service fspec-web start"]);

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[desired(web("/bin/web2", "always"))]).unwrap();
    assert!(fs::read_to_string(&script).unwrap().contains("command=/bin/web2\n"));
    assert_eq!(w.runner.lines(), ["rc-service fspec-web restart"]);

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[]).unwrap();
    assert_eq!(w.runner.lines(), ["rc-service fspec-web stop", "rc-update del fspec-web default"]);
    assert!(!script.exists());
}

#[test]
fn failed_start_replays_the_undo_steps() {
    let w = World::linux(Init::Openrc);
    w.runner.respond("rc-service fspec-web start", Output { status: 1, ..Output::default() });
    let err = apply_scope(&w.cx(Scope::System), &[&OpenrcBackend], &[desired(web("/bin/web", "always"))]).unwrap_err();
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    assert!(!w.paths.root.join("etc/init.d/fspec-web").exists());
    assert_eq!(
        w.runner.lines(),
        [
            "rc-update add fspec-web default",
            "rc-service fspec-web start",
            "rc-service fspec-web stop",
            "rc-update del fspec-web default",
        ]
    );
}

#[test]
fn drift_reports_an_edited_script() {
    let w = World::linux(Init::Openrc);
    let backends: &[&dyn Backend] = &[&OpenrcBackend];
    let want = vec![desired(web("/bin/web", "always"))];
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    fs::write(w.paths.root.join("etc/init.d/fspec-web"), "x").unwrap();
    let state = State::load(&w.paths.system_state_dir().join("state.json")).unwrap();
    let p = plan_scope(&w.cx(Scope::System), backends, &want, &state).unwrap();
    assert!(matches!(&p.changes[0].action, Action::Drift { notes } if notes == &["/etc/init.d/fspec-web was edited"]));
}

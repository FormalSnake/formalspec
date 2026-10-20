use std::fs;

use fspec_core::Scope;
use fspec_core::apply::{Backend, apply_scope, plan_scope};
use fspec_core::facts::Init;
use fspec_core::plan::Action;
use fspec_core::runner::Output;
use fspec_core::state::State;
use serde_json::json;

use super::*;
use crate::testutil::{World, desired, resource};

fn web(restart: &str, user: bool) -> Resource {
    resource(json!({
        "kind": "service", "name": "web", "exec": "webd --root '/srv/a b'", "restart": restart, "user": user,
        "description": "Web server", "env": {"PORT": "8080"}, "after": ["network", "db"]
    }))
}

fn render(r: &Resource, scope: Scope) -> (String, String) {
    let w = World::linux(Init::Dinit);
    let Resource::Service(s) = r else { panic!() };
    let p = Dinit.parts(&w.cx(scope), s).unwrap().remove(0);
    (p.path, p.content)
}

#[test]
fn renders_a_process_service() {
    let (path, text) = render(&web("on_failure", false), Scope::System);
    assert_eq!(path, "/etc/dinit.d/fspec-web");
    assert_eq!(
        text,
        "# Managed by fspec. Edits are overwritten on the next apply.\n# Web server\ntype = process\n\
         command = /usr/bin/env 'PORT=8080' webd --root '/srv/a b'\nrestart = on-failure\n\
         depends-on = network\ndepends-on = fspec-db\n"
    );
    assert!(render(&web("always", false), Scope::System).1.contains("restart = true\n"));
    assert!(render(&web("no", false), Scope::System).1.contains("restart = false\n"));
}

#[test]
fn user_services_skip_the_network_service() {
    let (path, text) = render(&web("always", true), Scope::User);
    assert_eq!(path, "~/.config/dinit.d/fspec-web");
    assert!(!text.contains("depends-on = network") && text.contains("depends-on = fspec-db\n"));
}

#[test]
fn shell_syntax_goes_through_sh() {
    let r = resource(json!({"kind": "service", "name": "x", "exec": "tail -f /var/log/a | logger"}));
    assert!(render(&r, Scope::System).1.contains("command = /bin/sh -c 'tail -f /var/log/a | logger'\n"));
}

#[test]
fn check_rejects_systemd_extras_and_bad_schedules() {
    let w = World::linux(Init::Dinit);
    let cx = w.cx(Scope::System);
    let extras = resource(json!({"kind": "service", "name": "s", "exec": "x", "systemd": {"MemoryMax": "1G"}}));
    let err = DinitBackend.check(&cx, &extras).unwrap_err().to_string();
    assert!(err.contains("sets `systemd` (MemoryMax)") && err.contains("dinit"), "{err}");
    let timer = resource(json!({"kind": "timer", "name": "t", "exec": "x", "schedule": "0 0 1 * 1"}));
    assert!(DinitBackend.check(&cx, &timer).is_err());
}

#[test]
fn claims_only_dinit_hosts() {
    let r = web("always", false);
    assert!(DinitBackend.handles(&World::linux(Init::Dinit).cx(Scope::System), &r));
    assert!(!DinitBackend.handles(&World::linux(Init::S6).cx(Scope::System), &r));
}

#[test]
fn add_update_remove_command_sequences() {
    let w = World::linux(Init::Dinit);
    let backends: &[&dyn Backend] = &[&DinitBackend];
    let file = w.paths.root.join("etc/dinit.d/fspec-web");

    apply_scope(&w.cx(Scope::System), backends, &[desired(web("always", false))]).unwrap();
    assert!(file.exists());
    assert_eq!(w.runner.lines(), ["dinitctl enable fspec-web", "dinitctl start fspec-web"]);

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[desired(web("no", false))]).unwrap();
    assert_eq!(w.runner.lines(), ["dinitctl reload fspec-web", "dinitctl restart fspec-web"]);

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[]).unwrap();
    assert_eq!(w.runner.lines(), ["dinitctl stop fspec-web", "dinitctl disable fspec-web"]);
    assert!(!file.exists());
}

#[test]
fn user_services_use_the_user_instance() {
    let w = World::linux(Init::Dinit);
    apply_scope(&w.cx(Scope::User), &[&DinitBackend], &[desired(web("always", true))]).unwrap();
    assert!(w.paths.home.join(".config/dinit.d/fspec-web").exists());
    assert_eq!(w.runner.lines(), ["dinitctl --user enable fspec-web", "dinitctl --user start fspec-web"]);
}

#[test]
fn failed_restart_restores_the_old_file() {
    let w = World::linux(Init::Dinit);
    let backends: &[&dyn Backend] = &[&DinitBackend];
    apply_scope(&w.cx(Scope::System), backends, &[desired(web("always", false))]).unwrap();
    let file = w.paths.root.join("etc/dinit.d/fspec-web");
    let old = fs::read_to_string(&file).unwrap();
    w.runner.calls.lock().unwrap().clear();

    w.runner.respond("dinitctl restart", Output { status: 1, ..Output::default() });
    let err = apply_scope(&w.cx(Scope::System), backends, &[desired(web("no", false))]).unwrap_err();
    assert!(format!("{err:#}").contains("apply failed"), "{err:#}");
    assert_eq!(fs::read_to_string(&file).unwrap(), old);
    assert_eq!(
        w.runner.lines(),
        [
            "dinitctl reload fspec-web",
            "dinitctl restart fspec-web",
            "dinitctl reload fspec-web",
            "dinitctl restart fspec-web",
        ]
    );
}

#[test]
fn drift_reports_a_missing_file() {
    let w = World::linux(Init::Dinit);
    let backends: &[&dyn Backend] = &[&DinitBackend];
    let want = vec![desired(web("always", false))];
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    fs::remove_file(w.paths.root.join("etc/dinit.d/fspec-web")).unwrap();
    let state = State::load(&w.paths.system_state_dir().join("state.json")).unwrap();
    let p = plan_scope(&w.cx(Scope::System), backends, &want, &state).unwrap();
    assert!(matches!(&p.changes[0].action, Action::Drift { notes } if notes == &["/etc/dinit.d/fspec-web is missing"]));
}

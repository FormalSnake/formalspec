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
        "kind": "service", "name": "web", "exec": "/usr/bin/webd --port 8080", "restart": restart,
        "env": {"PORT": "8080", "GREETING": "hello world"}, "after": ["network", "db"]
    }))
}

fn render(r: &Resource, user: bool) -> Vec<(String, String)> {
    let w = World::linux(Init::Runit);
    let cx = w.cx(if user { Scope::User } else { Scope::System });
    let Resource::Service(s) = r else { panic!() };
    Runit.parts(&cx, s).unwrap().into_iter().map(|p| (p.path, p.content)).collect()
}

#[test]
fn renders_run_and_finish() {
    assert_eq!(
        render(&web("always"), false),
        [(
            "/etc/sv/fspec-web/run".to_string(),
            "#!/bin/sh\nexec 2>&1\nexport GREETING='hello world'\nexport PORT=8080\n\
             sv check ../fspec-db >/dev/null || exit 1\nexec /usr/bin/webd --port 8080\n"
                .to_string()
        )]
    );
    let no = render(&web("no"), false);
    assert_eq!(no[1], ("/etc/sv/fspec-web/finish".into(), "#!/bin/sh\nsv down .\n".into()));
    let on_failure = render(&web("on_failure"), false);
    assert_eq!(on_failure[1].1, "#!/bin/sh\nif [ \"$1\" = 0 ]; then sv down .; fi\n");
}

#[test]
fn user_services_live_in_the_scanned_directory() {
    let user = resource(json!({"kind": "service", "name": "syncthing", "exec": "syncthing serve", "user": true}));
    assert_eq!(render(&user, true)[0].0, "~/.local/service/fspec-syncthing/run");
}

#[test]
fn check_rejects_systemd_extras_and_bad_input() {
    let w = World::linux(Init::Runit);
    let cx = w.cx(Scope::System);
    let extras = resource(json!({"kind": "service", "name": "s", "exec": "x", "systemd": {"MemoryMax": "1G"}}));
    let err = RunitBackend.check(&cx, &extras).unwrap_err().to_string();
    assert!(err.contains("sets `systemd` (MemoryMax)") && err.contains("runit"), "{err}");

    let target = resource(json!({"kind": "service", "name": "s", "exec": "x", "after": ["docker.target"]}));
    assert!(format!("{:#}", RunitBackend.check(&cx, &target).unwrap_err()).contains("systemd target"));

    let bad_env = resource(json!({"kind": "service", "name": "s", "exec": "x", "env": {"A B": "1"}}));
    assert!(RunitBackend.check(&cx, &bad_env).is_err());
}

#[test]
fn claims_only_runit_hosts() {
    let r = web("always");
    assert!(RunitBackend.handles(&World::linux(Init::Runit).cx(Scope::System), &r));
    assert!(!RunitBackend.handles(&World::linux(Init::Openrc).cx(Scope::System), &r));
    assert!(!RunitBackend.handles(&World::linux(Init::Systemd).cx(Scope::System), &r));
}

fn sv(w: &World, rest: &str) -> String {
    let (verb, path) = rest.split_once(' ').unwrap();
    format!("sv {verb} {}", w.paths.root.join(path).display())
}

#[test]
fn add_update_remove_command_sequences() {
    let w = World::linux(Init::Runit);
    let backends: &[&dyn Backend] = &[&RunitBackend];
    let dir = w.paths.root.join("etc/sv/fspec-web");
    let link = w.paths.root.join("var/service/fspec-web");

    apply_scope(&w.cx(Scope::System), backends, &[desired(web("no"))]).unwrap();
    assert!(dir.join("finish").exists());
    assert_eq!(fs::metadata(dir.join("run")).unwrap().permissions().mode() & 0o777, 0o755);
    assert_eq!(fs::read_link(&link).unwrap().to_str().unwrap(), "/etc/sv/fspec-web");
    assert!(w.runner.lines().is_empty());

    apply_scope(&w.cx(Scope::System), backends, &[desired(web("always"))]).unwrap();
    assert!(!dir.join("finish").exists());
    assert_eq!(w.runner.lines(), [sv(&w, "restart var/service/fspec-web")]);

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[]).unwrap();
    assert_eq!(w.runner.lines(), [sv(&w, "down var/service/fspec-web")]);
    assert!(!dir.exists() && fs::symlink_metadata(&link).is_err());
}

#[test]
fn first_user_service_declares_the_runsvdir_service() {
    let w = World::linux(Init::Runit);
    let user = resource(json!({"kind": "service", "name": "syncthing", "exec": "syncthing serve", "user": true}));
    apply_scope(&w.cx(Scope::User), &[&RunitBackend], &[desired(user.clone())]).unwrap();

    let run = fs::read_to_string(w.paths.root.join("etc/sv/fspec-runsvdir-u/run")).unwrap();
    assert_eq!(run, "#!/bin/sh\nexec 2>&1\nexec chpst -u u env HOME=/home/u runsvdir /home/u/.local/service\n");
    assert!(fs::symlink_metadata(w.paths.root.join("var/service/fspec-runsvdir-u")).is_ok());
    assert!(w.paths.home.join(".local/service/fspec-syncthing/run").exists());
    assert_eq!(w.runner.lines(), ["id -un"]);

    w.runner.calls.lock().unwrap().clear();
    let second = resource(json!({"kind": "service", "name": "second", "exec": "x", "user": true}));
    apply_scope(&w.cx(Scope::User), &[&RunitBackend], &[desired(user), desired(second)]).unwrap();
    assert_eq!(w.runner.lines(), ["id -un"]);
}

#[test]
fn user_name_comes_from_id_when_it_answers() {
    let w = World::linux(Init::Runit);
    w.runner.respond("id -un", Output { stdout: "kyan\n".into(), ..Output::default() });
    let user = resource(json!({"kind": "service", "name": "s", "exec": "x", "user": true}));
    apply_scope(&w.cx(Scope::User), &[&RunitBackend], &[desired(user)]).unwrap();
    assert!(w.paths.root.join("etc/sv/fspec-runsvdir-kyan/run").exists());
}

#[test]
fn failed_restart_restores_the_old_files() {
    let w = World::linux(Init::Runit);
    let backends: &[&dyn Backend] = &[&RunitBackend];
    apply_scope(&w.cx(Scope::System), backends, &[desired(web("no"))]).unwrap();
    let old = fs::read_to_string(w.paths.root.join("etc/sv/fspec-web/run")).unwrap();

    w.runner.respond("sv restart", Output { status: 1, ..Output::default() });
    let changed = resource(json!({"kind": "service", "name": "web", "exec": "/bin/other", "restart": "no"}));
    let err = apply_scope(&w.cx(Scope::System), backends, &[desired(changed)]).unwrap_err();
    assert!(format!("{err:#}").contains("apply failed"), "{err:#}");
    assert_eq!(fs::read_to_string(w.paths.root.join("etc/sv/fspec-web/run")).unwrap(), old);
    assert!(w.paths.root.join("etc/sv/fspec-web/finish").exists());
}

#[test]
fn drift_reports_edited_and_missing_files() {
    let w = World::linux(Init::Runit);
    let backends: &[&dyn Backend] = &[&RunitBackend];
    let want = vec![desired(web("always"))];
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    let state = || State::load(&w.paths.system_state_dir().join("state.json")).unwrap();
    let plan = |cx: Cx| plan_scope(&cx, backends, &want, &state()).unwrap();
    assert!(plan(w.cx(Scope::System)).changes.is_empty());

    let run = w.paths.root.join("etc/sv/fspec-web/run");
    fs::write(&run, "edited").unwrap();
    let p = plan(w.cx(Scope::System));
    assert!(matches!(&p.changes[0].action, Action::Drift { notes } if notes == &["/etc/sv/fspec-web/run was edited"]));
    fs::remove_file(&run).unwrap();
    let p = plan(w.cx(Scope::System));
    assert!(matches!(&p.changes[0].action, Action::Drift { notes } if notes[0].ends_with("is missing")));
}

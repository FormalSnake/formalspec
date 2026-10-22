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
use crate::{DinitBackend, OpenrcBackend, RunitBackend, S6Backend};

fn timer(user: bool, schedule: &str) -> fspec_core::Resource {
    resource(
        json!({"kind": "timer", "name": "backup", "exec": "/usr/local/bin/backup --stamp %F", "schedule": schedule, "user": user}),
    )
}

fn t(r: &fspec_core::Resource) -> Timer {
    let fspec_core::Resource::Timer(t) = r else { panic!() };
    t.clone()
}

#[test]
fn renders_the_schedule_unchanged() {
    assert_eq!(
        render_file(&t(&timer(false, "30  2 * * 1-5"))).unwrap(),
        "# Managed by fspec. Edits are overwritten on the next apply.\n\
         30 2 * * 1-5 root /usr/local/bin/backup --stamp \\%F\n"
    );
    assert!(
        render_file(&t(&timer(false, "@daily"))).unwrap().ends_with("@daily root /usr/local/bin/backup --stamp \\%F\n")
    );
    assert_eq!(
        render_block(&t(&timer(true, "@weekly"))).unwrap(),
        "# BEGIN fspec:backup\n@weekly /usr/local/bin/backup --stamp \\%F\n# END fspec:backup\n"
    );
}

#[test]
fn check_rejects_what_cron_cannot_run() {
    assert!(format!("{:#}", check(&t(&timer(false, "0 0 1 * 1"))).unwrap_err()).contains("both day-of-month"));
    assert!(check(&t(&timer(false, "@reboot"))).is_err());
    let dotted = resource(json!({"kind": "timer", "name": "a.b", "exec": "x", "schedule": "@daily"}));
    assert!(check(&t(&dotted)).unwrap_err().to_string().contains("cron ignores"));
}

#[test]
fn splices_blocks_into_an_existing_crontab() {
    let a = render_block(&t(&timer(true, "@daily"))).unwrap();
    let base = "MAILTO=me\n0 1 * * * other\n";
    let with = splice(base, "backup", Some(&a));
    assert_eq!(with, format!("{base}{a}"));
    let again = splice(&with, "backup", Some("# BEGIN fspec:backup\n@hourly x\n# END fspec:backup\n"));
    assert_eq!(again, format!("{base}# BEGIN fspec:backup\n@hourly x\n# END fspec:backup\n"));
    assert_eq!(splice(&with, "backup", None), base);
    assert_eq!(splice("no newline", "x", Some("# BEGIN fspec:x\n")), "no newline\n# BEGIN fspec:x\n");
}

#[test]
fn every_non_systemd_init_runs_timers_as_cron_entries() {
    for (init, backend) in [
        (Init::Runit, &RunitBackend as &dyn Backend),
        (Init::Openrc, &OpenrcBackend),
        (Init::Dinit, &DinitBackend),
        (Init::S6, &S6Backend),
    ] {
        let w = World::linux(init);
        let r = timer(false, "30 2 * * 1-5");
        assert!(backend.handles(&w.cx(Scope::System), &r), "{init}");
        apply_scope(&w.cx(Scope::System), &[backend], &[desired(r.clone())]).unwrap();
        let file = w.paths.root.join("etc/cron.d/fspec-backup");
        assert!(fs::read_to_string(&file).unwrap().contains("30 2 * * 1-5 root /usr/local/bin/backup"), "{init}");
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o644);
        assert!(w.runner.lines().is_empty(), "{init}: {:?}", w.runner.lines());

        apply_scope(&w.cx(Scope::System), &[backend], &[]).unwrap();
        assert!(!file.exists(), "{init}");
    }
}

#[test]
fn user_timers_edit_the_users_crontab() {
    let w = World::linux(Init::Runit);
    let backends: &[&dyn Backend] = &[&RunitBackend];
    w.runner.respond("crontab -l", Output { stdout: "MAILTO=me\n".into(), ..Output::default() });
    let want = vec![desired(timer(true, "@daily"))];
    apply_scope(&w.cx(Scope::User), backends, &want).unwrap();

    let installed = "MAILTO=me\n# BEGIN fspec:backup\n@daily /usr/local/bin/backup --stamp \\%F\n# END fspec:backup\n";
    let calls = w.runner.calls.lock().unwrap().clone();
    assert_eq!(calls.iter().map(ToString::to_string).collect::<Vec<_>>(), ["crontab -l", "crontab -"]);
    assert_eq!(calls[1].stdin.as_deref(), Some(installed));

    w.runner.responses.lock().unwrap().clear();
    w.runner.respond("crontab -l", Output { stdout: installed.into(), ..Output::default() });
    let state = State::load(&w.paths.user_state_dir().join("state.json")).unwrap();
    assert!(plan_scope(&w.cx(Scope::User), backends, &want, &state).unwrap().changes.is_empty());

    w.runner.responses.lock().unwrap().clear();
    w.runner.respond("crontab -l", Output { stdout: "MAILTO=me\n".into(), ..Output::default() });
    let p = plan_scope(&w.cx(Scope::User), backends, &want, &state).unwrap();
    assert!(
        matches!(&p.changes[0].action, Action::Drift { notes } if notes == &["crontab entry fspec:backup is missing"])
    );

    w.runner.calls.lock().unwrap().clear();
    w.runner.responses.lock().unwrap().clear();
    w.runner.respond("crontab -l", Output { stdout: installed.into(), ..Output::default() });
    apply_scope(&w.cx(Scope::User), backends, &[]).unwrap();
    let calls = w.runner.calls.lock().unwrap().clone();
    assert_eq!(calls[1].stdin.as_deref(), Some("MAILTO=me\n"));
}

#[test]
fn a_user_without_a_crontab_gets_just_the_block() {
    let w = World::linux(Init::Openrc);
    w.runner.respond("crontab -l", Output { status: 1, ..Output::default() });
    apply_scope(&w.cx(Scope::User), &[&OpenrcBackend], &[desired(timer(true, "@daily"))]).unwrap();
    let calls = w.runner.calls.lock().unwrap().clone();
    let block = "# BEGIN fspec:backup\n@daily /usr/local/bin/backup --stamp \\%F\n# END fspec:backup\n";
    assert_eq!(calls[1].stdin.as_deref(), Some(block));
}

#[test]
fn system_timer_drift_reports_edits() {
    let w = World::linux(Init::Dinit);
    let backends: &[&dyn Backend] = &[&DinitBackend];
    let want = vec![desired(timer(false, "@daily"))];
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    fs::write(w.paths.root.join("etc/cron.d/fspec-backup"), "x").unwrap();
    let state = State::load(&w.paths.system_state_dir().join("state.json")).unwrap();
    let p = plan_scope(&w.cx(Scope::System), backends, &want, &state).unwrap();
    assert!(
        matches!(&p.changes[0].action, Action::Drift { notes } if notes == &["/etc/cron.d/fspec-backup was edited"])
    );
}

#[test]
fn a_timer_env_prefixes_the_command() {
    let r = resource(json!({"kind": "timer", "name": "scan", "exec": "/opt/scan", "schedule": "@hourly",
        "env": {"VAULT": "/home/k/My Vault"}}));
    assert!(render_file(&t(&r)).unwrap().ends_with("@hourly root env 'VAULT=/home/k/My Vault' /opt/scan\n"));
}

use std::fs;

use fspec_core::Scope;
use fspec_core::apply::{Backend, apply_scope, plan_scope};
use fspec_core::facts::Init;
use fspec_core::runner::Output;
use fspec_core::state::State;
use serde_json::json;

use super::*;
use crate::testutil::{World, desired, resource};

fn agent() -> Resource {
    resource(json!({
        "kind": "service", "name": "syncthing", "exec": "syncthing serve --home '/tmp/a b'", "user": true,
        "restart": "on_failure", "env": {"PATH": "/opt/homebrew/bin"}
    }))
}

#[test]
fn renders_a_service_plist() {
    assert_eq!(plist_path(&agent()), "~/Library/LaunchAgents/fspec.syncthing.plist");
    assert_eq!(
        render(&agent()).unwrap(),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>fspec.syncthing</string>
	<key>ProgramArguments</key>
	<array>
		<string>/usr/bin/env</string>
		<string>syncthing</string>
		<string>serve</string>
		<string>--home</string>
		<string>/tmp/a b</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>/opt/homebrew/bin</string>
	</dict>
</dict>
</plist>"#
    );
}

#[test]
fn shell_syntax_goes_through_sh() {
    let r =
        resource(json!({"kind": "service", "name": "x", "exec": "tail -f /var/log/a | logger", "restart": "always"}));
    let text = render(&r).unwrap();
    assert!(text.contains(
        "<string>/bin/sh</string>\n\t\t<string>-c</string>\n\t\t<string>tail -f /var/log/a | logger</string>"
    ));
    assert!(text.contains("<key>KeepAlive</key>\n\t<true/>"));
    assert_eq!(plist_path(&r), "/Library/LaunchDaemons/fspec.x.plist");

    let abs = resource(json!({"kind": "service", "name": "x", "exec": "/usr/bin/true"}));
    assert!(render(&abs).unwrap().contains("<array>\n\t\t<string>/usr/bin/true</string>\n\t</array>"));
}

#[test]
fn renders_a_timer_plist() {
    let r = resource(json!({"kind": "timer", "name": "backup", "exec": "/opt/backup.sh", "schedule": "0 9,17 * * 1"}));
    let text = render(&r).unwrap();
    assert!(!text.contains("RunAtLoad"));
    let expected = "<key>StartCalendarInterval</key>\n\t<array>\n\t\t<dict>\n\t\t\t<key>Minute</key>\n\t\t\t<integer>0</integer>\n\
        \t\t\t<key>Hour</key>\n\t\t\t<integer>9</integer>\n\t\t\t<key>Weekday</key>\n\t\t\t<integer>1</integer>\n\t\t</dict>\n\
        \t\t<dict>\n\t\t\t<key>Minute</key>\n\t\t\t<integer>0</integer>\n\t\t\t<key>Hour</key>\n\t\t\t<integer>17</integer>";
    assert!(text.contains(expected), "{text}");
}

#[test]
fn check_rejects_systemd_extras_and_bad_schedules() {
    let w = World::darwin();
    let cx = w.cx(Scope::System);
    let with_extras =
        resource(json!({"kind": "service", "name": "s", "exec": "x", "systemd": {"MemoryMax": "1G", "unit": {}}}));
    let err = LaunchdBackend.check(&cx, &with_extras).unwrap_err().to_string();
    assert_eq!(
        err,
        "service:s sets `systemd` (MemoryMax, unit), which only systemd supports, and this host uses launchd"
    );

    let runit = reject_systemd_extras(Init::Runit, &with_extras).unwrap_err().to_string();
    assert!(runit.contains("`systemd`") && runit.ends_with("this host uses runit"), "{runit}");
    assert!(reject_systemd_extras(Init::Runit, &agent()).is_ok());

    let range = resource(json!({"kind": "timer", "name": "t", "exec": "x", "schedule": "*/2 */2 * * *"}));
    let err = LaunchdBackend.check(&cx, &range).unwrap_err();
    assert!(format!("{err:#}").contains("calendar entries"), "{err:#}");
    let both = resource(json!({"kind": "timer", "name": "t", "exec": "x", "schedule": "0 0 1 * 1"}));
    assert!(LaunchdBackend.check(&cx, &both).is_err());
}

#[test]
fn daemon_command_sequences() {
    let w = World::darwin();
    let backends: &[&dyn Backend] = &[&LaunchdBackend];
    let plist = w.paths.root.join("Library/LaunchDaemons/fspec.web.plist");
    let p = plist.display();
    let svc = |exec: &str| resource(json!({"kind": "service", "name": "web", "exec": exec}));

    apply_scope(&w.cx(Scope::System), backends, &[desired(svc("/bin/web"))]).unwrap();
    assert!(plist.exists());
    assert_eq!(w.runner.lines(), [format!("launchctl bootstrap system {p}")]);

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[desired(svc("/bin/web2"))]).unwrap();
    assert_eq!(
        w.runner.lines(),
        ["launchctl bootout system/fspec.web".to_string(), format!("launchctl bootstrap system {p}")]
    );

    w.runner.calls.lock().unwrap().clear();
    apply_scope(&w.cx(Scope::System), backends, &[]).unwrap();
    assert_eq!(w.runner.lines(), ["launchctl bootout system/fspec.web"]);
    assert!(!plist.exists());
}

#[test]
fn user_agents_use_the_gui_domain() {
    let w = World::darwin();
    w.runner.respond("id -u", Output { stdout: "501\n".into(), ..Output::default() });
    apply_scope(&w.cx(Scope::User), &[&LaunchdBackend], &[desired(agent())]).unwrap();
    let plist = w.paths.home.join("Library/LaunchAgents/fspec.syncthing.plist");
    assert!(plist.exists());
    assert_eq!(w.runner.lines(), ["id -u".to_string(), format!("launchctl bootstrap gui/501 {}", plist.display())]);
}

#[test]
fn failed_bootstrap_restores_the_old_plist_and_job() {
    let w = World::darwin();
    let backends: &[&dyn Backend] = &[&LaunchdBackend];
    let plist = w.paths.root.join("Library/LaunchDaemons/fspec.web.plist");
    let svc = |exec: &str| resource(json!({"kind": "service", "name": "web", "exec": exec}));
    apply_scope(&w.cx(Scope::System), backends, &[desired(svc("/bin/web"))]).unwrap();
    let old = fs::read_to_string(&plist).unwrap();
    w.runner.calls.lock().unwrap().clear();

    w.runner.respond("launchctl bootstrap", Output { status: 5, ..Output::default() });
    let err = apply_scope(&w.cx(Scope::System), backends, &[desired(svc("/bin/web2"))]).unwrap_err();
    assert!(format!("{err:#}").contains("apply failed"), "{err:#}");
    assert_eq!(fs::read_to_string(&plist).unwrap(), old);
    // Forward: bootout, bootstrap (fails). Undo: bootout the new job, bootstrap the restored plist.
    let lines = w.runner.lines();
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(lines[0], "launchctl bootout system/fspec.web");
    assert!(lines[1].starts_with("launchctl bootstrap system"));
    assert_eq!(lines[2], "launchctl bootout system/fspec.web");
    assert!(lines[3].starts_with("launchctl bootstrap system"));
}

#[test]
fn drift_reports_an_edited_plist() {
    let w = World::darwin();
    let backends: &[&dyn Backend] = &[&LaunchdBackend];
    let want = vec![desired(resource(json!({"kind": "service", "name": "web", "exec": "/bin/web"})))];
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    fs::write(w.paths.root.join("Library/LaunchDaemons/fspec.web.plist"), "x").unwrap();
    let state = State::load(&w.paths.system_state_dir().join("state.json")).unwrap();
    let p = plan_scope(&w.cx(Scope::System), backends, &want, &state).unwrap();
    assert_eq!(p.counts(), (0, 1, 0));
}

use std::fs;

use super::*;
use crate::fetch::MapFetch;
use crate::journal::Undo;
use crate::resource::{File, FileMode, System};
use crate::runner::{Cmd, RecordingRunner};

/// Writes `content` for each file, journaled; fails on a path containing `fail`.
struct FakeFiles;

impl Backend for FakeFiles {
    fn name(&self) -> &'static str {
        "fake-files"
    }
    fn handles(&self, _cx: &Cx, r: &Resource) -> bool {
        matches!(r, Resource::File(_))
    }
    fn phase(&self) -> Phase {
        Phase::Files
    }
    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        let Resource::File(f) = &entry.resource else { return Ok(vec![]) };
        let on_disk = fs::read_to_string(cx.resolve(&f.path)?).unwrap_or_default();
        Ok(if Some(&Value::String(on_disk)) == f.content.as_ref() { vec![] } else { vec!["content changed".into()] })
    }
    fn commit(&self, cx: &Cx, ops: &[Op], _staging: &Path, journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        for op in ops {
            let Resource::File(f) = op.resource() else { unreachable!() };
            let path = cx.resolve(&f.path)?;
            journal.backup(&path)?;
            if op.is_removal() {
                remove_any(&path)?;
                continue;
            }
            if f.path.contains("fail") {
                bail!("refusing {}", f.path);
            }
            journal.record(Undo::Run { cmd: Cmd::new("echo").arg(format!("undo {}", f.path)) })?;
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(&path, f.content.as_ref().and_then(Value::as_str).unwrap_or(""))?;
            let mut rec = Map::new();
            rec.insert("written".into(), true.into());
            records.insert(op.key.clone(), rec);
        }
        Ok(records)
    }
}

fn file(path: &str, content: &str) -> Desired {
    let resource = Resource::File(File {
        path: path.into(),
        src: None,
        content: Some(content.into()),
        format: None,
        mode: FileMode::Managed,
        owns: Default::default(),
        on_conflict: Default::default(),
        on_drift: Default::default(),
        perms: None,
    });
    Desired { resource, owner: None }
}

struct World {
    _dir: tempfile::TempDir,
    paths: Paths,
    facts: Facts,
    lock: Lock,
    runner: RecordingRunner,
    fetch: MapFetch,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().into(), dir.path().join("home/u"));
        let system = System::Distro(crate::resource::Distro {
            distro: "fedora".into(),
            init: crate::facts::Init::Systemd,
            packages: vec![],
        });
        let facts = Facts::declared("h", "x86_64-linux", Some(&system)).unwrap();
        World {
            _dir: dir,
            paths,
            facts,
            lock: Lock::default(),
            runner: RecordingRunner::default(),
            fetch: MapFetch::default(),
        }
    }

    fn cx(&self, scope: Scope) -> Cx<'_> {
        let generation = State::load(&state_dir(&self.paths, scope).join("state.json")).unwrap().generation + 1;
        Cx {
            paths: &self.paths,
            facts: &self.facts,
            spec_dir: &self.paths.root,
            lock: &self.lock,
            runner: &self.runner,
            fetch: &self.fetch,
            generation,
            scope,
        }
    }

    fn state(&self, scope: Scope) -> State {
        State::load(&state_dir(&self.paths, scope).join("state.json")).unwrap()
    }
}

#[test]
fn apply_then_converged_then_drift_then_remove() {
    let w = World::new();
    let backends: &[&dyn Backend] = &[&FakeFiles];
    let want = vec![file("/etc/a", "A"), file("~/.b", "B")];

    let p = apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    assert_eq!(p.counts(), (1, 0, 0));
    apply_scope(&w.cx(Scope::User), backends, &want).unwrap();
    assert_eq!(fs::read_to_string(w.paths.root.join("etc/a")).unwrap(), "A");
    assert_eq!(fs::read_to_string(w.paths.home.join(".b")).unwrap(), "B");
    let state = w.state(Scope::System);
    assert_eq!(state.generation, 1);
    assert_eq!(state.entries[&want[0].resource.key()].record["written"], true);
    assert!(w.paths.system_state_dir().join("generations/1.json").exists());

    // Nothing to do: no new generation.
    assert!(apply_scope(&w.cx(Scope::System), backends, &want).unwrap().changes.is_empty());
    assert_eq!(w.state(Scope::System).generation, 1);

    fs::write(w.paths.root.join("etc/a"), "edited").unwrap();
    let p = plan_scope(&w.cx(Scope::System), backends, &want, &w.state(Scope::System)).unwrap();
    assert!(matches!(&p.changes[0].action, Action::Drift { notes } if notes == &["content changed"]));
    apply_scope(&w.cx(Scope::System), backends, &want).unwrap();
    assert_eq!(fs::read_to_string(w.paths.root.join("etc/a")).unwrap(), "A");

    apply_scope(&w.cx(Scope::System), backends, &[]).unwrap();
    assert!(!w.paths.root.join("etc/a").exists());
    assert!(w.state(Scope::System).entries.is_empty());
    assert_eq!(w.state(Scope::System).generation, 3);
}

#[test]
fn failure_rolls_back_everything() {
    let w = World::new();
    let backends: &[&dyn Backend] = &[&FakeFiles];
    apply_scope(&w.cx(Scope::System), backends, &[file("/etc/a", "A")]).unwrap();

    let err = apply_scope(&w.cx(Scope::System), backends, &[file("/etc/a", "A2"), file("/etc/fail", "x")]).unwrap_err();
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    assert_eq!(fs::read_to_string(w.paths.root.join("etc/a")).unwrap(), "A");
    assert!(!w.paths.root.join("etc/fail").exists());
    assert_eq!(w.state(Scope::System).generation, 1);
    assert!(w.runner.lines().contains(&"echo 'undo /etc/a'".to_string()));
    assert!(!w.paths.system_state_dir().join("journal.json").exists());
}

#[test]
fn unclaimed_resource_fails_the_plan() {
    let w = World::new();
    let pkg = Desired {
        resource: serde_json::from_value(
            serde_json::json!({"kind": "pkg", "name": "rg", "source": {"kind": "aqua", "name": "rg"}}),
        )
        .unwrap(),
        owner: None,
    };
    let p = plan_scope(&w.cx(Scope::System), &[&FakeFiles], std::slice::from_ref(&pkg), &State::default()).unwrap();
    assert_eq!(p.unsupported, ["no backend applies pkg:rg on h (fedora, systemd)"]);
    let err = apply_scope(&w.cx(Scope::System), &[&FakeFiles], &[pkg]).unwrap_err();
    assert!(err.to_string().contains("cannot apply"), "{err}");
}

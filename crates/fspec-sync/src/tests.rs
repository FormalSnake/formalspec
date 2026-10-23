use std::fs;
use std::path::Path;

use fspec_core::resource::SyncItem;
use fspec_core::runner::{Output, RecordingRunner};
use serde_json::json;

use crate::run::{Opts, Outcome, pair};
use crate::{Local, Side};

struct Two {
    _dir: tempfile::TempDir,
    a: RecordingRunner,
    b: RecordingRunner,
    root: std::path::PathBuf,
}

impl Two {
    fn new() -> Two {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let quiet = || {
            let r = RecordingRunner::default();
            // Nothing runs: pgrep finds no process.
            r.respond("pgrep", Output { status: 1, ..Output::default() });
            r
        };
        Two { _dir: dir, a: quiet(), b: quiet(), root }
    }

    fn side<'a>(&'a self, host: &str, runner: &'a RecordingRunner) -> Local<'a> {
        let home = self.root.join(host);
        Local { runner, host: host.into(), state: home.join(".local/state/fspec/sync"), home }
    }

    fn a(&self) -> Local<'_> {
        self.side("g815", &self.a)
    }

    fn b(&self) -> Local<'_> {
        self.side("e1504g", &self.b)
    }

    fn write(&self, host: &str, rel: &str, text: &str, mtime: u64) {
        let path = self.root.join(host).join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime);
        fs::File::options().write(true).open(&path).unwrap().set_modified(t).unwrap();
    }

    fn read(&self, host: &str, rel: &str) -> Option<String> {
        fs::read_to_string(self.root.join(host).join(rel)).ok()
    }

    fn sync(&self, item: &SyncItem) -> Outcome {
        pair(&self.a(), &self.b(), item, &Opts::default()).unwrap()
    }
}

fn item(v: serde_json::Value) -> SyncItem {
    let mut base = json!({"name": "it", "peers": ["g815", "e1504g"]});
    base.as_object_mut().unwrap().extend(v.as_object().unwrap().clone());
    let item: SyncItem = serde_json::from_value(base).unwrap();
    item.check().unwrap();
    item
}

fn conflicts(o: Outcome) -> Vec<String> {
    match o {
        Outcome::Done { conflicts } => conflicts,
        Outcome::Skipped => panic!("skipped"),
    }
}

#[test]
fn live_copies_each_change_and_deletion_to_the_other_side() {
    let t = Two::new();
    let it = item(json!({"paths": ["~/Pictures/Wallpapers/"], "exclude": ["*.tmp"]}));
    t.write("g815", "Pictures/Wallpapers/dark/a.png", "a", 100);
    t.write("g815", "Pictures/Wallpapers/x.tmp", "x", 100);
    t.write("e1504g", "Pictures/Wallpapers/b.png", "b", 100);
    assert!(conflicts(t.sync(&it)).is_empty());
    assert_eq!(t.read("e1504g", "Pictures/Wallpapers/dark/a.png").as_deref(), Some("a"));
    assert_eq!(t.read("g815", "Pictures/Wallpapers/b.png").as_deref(), Some("b"));
    assert_eq!(t.read("e1504g", "Pictures/Wallpapers/x.tmp"), None);
    let mtime = fs::metadata(t.root.join("e1504g/Pictures/Wallpapers/dark/a.png")).unwrap().modified().unwrap();
    assert_eq!(mtime, std::time::UNIX_EPOCH + std::time::Duration::from_secs(100));

    fs::remove_file(t.root.join("e1504g/Pictures/Wallpapers/b.png")).unwrap();
    t.write("e1504g", "Pictures/Wallpapers/dark/a.png", "a2", 200);
    t.sync(&it);
    assert_eq!(t.read("g815", "Pictures/Wallpapers/b.png"), None);
    assert_eq!(t.read("g815", "Pictures/Wallpapers/dark/a.png").as_deref(), Some("a2"));
    // The replaced and the deleted file wait in prev for a rollback.
    assert_eq!(t.read("g815", ".local/state/fspec/sync/it/prev/Pictures/Wallpapers/b.png").as_deref(), Some("b"));

    assert_eq!(t.a().rollback(&it).unwrap(), 2);
    assert_eq!(t.read("g815", "Pictures/Wallpapers/dark/a.png").as_deref(), Some("a"));
    assert_eq!(t.read("g815", "Pictures/Wallpapers/b.png").as_deref(), Some("b"));
}

#[test]
fn a_change_on_both_sides_goes_to_the_newest_or_stays_as_a_conflict() {
    let t = Two::new();
    let it = item(json!({"paths": ["~/notes.md"]}));
    t.write("g815", "notes.md", "one", 100);
    t.sync(&it);
    t.write("g815", "notes.md", "g815 edit", 300);
    t.write("e1504g", "notes.md", "e1504g edit", 200);
    let manual = item(json!({"paths": ["~/notes.md"], "conflict": "manual"}));
    assert_eq!(conflicts(t.sync(&manual)), ["~/notes.md"]);
    assert_eq!(t.read("e1504g", "notes.md").as_deref(), Some("e1504g edit"));
    t.sync(&it);
    assert_eq!(t.read("e1504g", "notes.md").as_deref(), Some("g815 edit"));
}

#[test]
fn seed_copies_only_to_a_host_without_it_and_hands_over_with_from() {
    let t = Two::new();
    let it = item(json!({"paths": ["~/.config/helium/"], "mode": "seed"}));
    t.write("g815", ".config/helium/Default/Cookies", "g", 100);
    t.sync(&it);
    assert_eq!(t.read("e1504g", ".config/helium/Default/Cookies").as_deref(), Some("g"));

    t.write("e1504g", ".config/helium/Default/Cookies", "e", 200);
    t.write("g815", ".config/helium/Default/Extra", "only g", 100);
    t.sync(&it);
    assert_eq!(t.read("g815", ".config/helium/Default/Cookies").as_deref(), Some("g"));

    let from = Opts { from: Some("e1504g".into()), ..Opts::default() };
    pair(&t.a(), &t.b(), &it, &from).unwrap();
    assert_eq!(t.read("g815", ".config/helium/Default/Cookies").as_deref(), Some("e"));
    assert_eq!(t.read("g815", ".config/helium/Default/Extra"), None);
}

#[test]
fn json_keys_sync_alone_and_on_receive_gets_the_value() {
    let t = Two::new();
    let it = item(json!({
        "keys": {"path": "~/.local/state/formalshell/state.json", "keys": ["wallpaper"]},
        "on_receive": "formalshell ipc call wallpaper set {wallpaper}",
    }));
    t.write("g815", ".local/state/formalshell/state.json", r#"{"wallpaper": "/w/a b.png", "bar": 1}"#, 100);
    t.write("e1504g", ".local/state/formalshell/state.json", r#"{"wallpaper": "/w/a b.png", "bar": 2}"#, 100);
    t.sync(&it);
    t.write("g815", ".local/state/formalshell/state.json", r#"{"wallpaper": "/w/c.png", "bar": 1}"#, 300);
    t.sync(&it);
    let state: serde_json::Value =
        serde_json::from_str(&t.read("e1504g", ".local/state/formalshell/state.json").unwrap()).unwrap();
    assert_eq!(state, json!({"wallpaper": "/w/c.png", "bar": 2}));
    assert!(
        t.b.lines().contains(&"sh -c 'formalshell ipc call wallpaper set /w/c.png'".to_string()),
        "{:?}",
        t.b.lines()
    );
    assert!(!t.a.lines().iter().any(|l| l.contains("formalshell")));
}

#[test]
fn keyring_items_move_with_their_secret_and_keep_the_old_one() {
    let t = Two::new();
    let it = item(
        json!({"keyring": [{"schema": "chrome_libsecret_os_crypt_password_v2", "attrs": {"application": "chromium"}}]}),
    );
    let found = "[/3]\nlabel = Chromium Safe Storage\nsecret = s3cret\nmodified = 2026-06-16 17:17:38\nschema = chrome_libsecret_os_crypt_password_v2\nattribute.application = chromium\n";
    t.a.respond("secret-tool search --all application chromium", Output { stdout: found.into(), ..Output::default() });
    t.b.respond("secret-tool search --all application chromium", Output { status: 1, ..Output::default() });
    t.sync(&it);
    let calls = t.b.calls.lock().unwrap();
    let store = calls.iter().find(|c| c.program == "secret-tool" && c.args[0] == "store").unwrap();
    assert_eq!(
        store.to_string(),
        "secret-tool store '--label=Chromium Safe Storage' application chromium xdg:schema chrome_libsecret_os_crypt_password_v2"
    );
    assert_eq!(store.stdin.as_deref(), Some("s3cret"));
    let base = fs::read_to_string(t.root.join("e1504g/.local/state/fspec/sync/it/peers/g815.json")).unwrap();
    assert!(!base.contains("s3cret"), "{base}");
}

#[test]
fn a_running_owner_skips_the_item() {
    let t = Two::new();
    let it = item(json!({"paths": ["~/.config/x/"], "unless_running": ["helium"]}));
    let busy = RecordingRunner::default();
    let b = t.side("e1504g", &busy);
    assert!(matches!(pair(&t.a(), &b, &it, &Opts::default()).unwrap(), Outcome::Skipped));
    assert!(!Path::new(&t.root.join("e1504g/.local/state/fspec/sync/it/peers")).exists());
    assert_eq!(b.host(), "e1504g");
}

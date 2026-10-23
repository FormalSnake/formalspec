//! This machine's side of a sync: scanning, packing and applying under its home.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use fspec_core::Paths;
use fspec_core::journal::remove_any;
use fspec_core::resource::SyncItem;
use fspec_core::runner::{Cmd, Runner};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::keyring::{self, Found};
use crate::{Apply, Manifest, Meta, Side};

pub struct Local<'a> {
    pub runner: &'a dyn Runner,
    pub host: String,
    pub home: PathBuf,
    /// `~/.local/state/fspec/sync`: bases, staged files and previous versions per item.
    pub state: PathBuf,
}

impl<'a> Local<'a> {
    pub fn new(runner: &'a dyn Runner, host: String, paths: &Paths) -> Local<'a> {
        Local { runner, host, home: paths.home.clone(), state: paths.user_state_dir().join("sync") }
    }

    fn dir(&self, item: &str) -> PathBuf {
        self.state.join(item)
    }

    fn base_path(&self, item: &str, peer: &str) -> PathBuf {
        self.dir(item).join("peers").join(format!("{peer}.json"))
    }

    /// The base this host shares with `peer` for `item`, empty before their first sync.
    pub fn base(&self, item: &str, peer: &str) -> Result<Manifest> {
        let path = self.base_path(item, peer);
        match fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).with_context(|| format!("reading {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn at(&self, key: &str) -> PathBuf {
        self.home.join(key.strip_prefix("~/").unwrap_or(key))
    }

    /// Puts the previous version of `item` back on this host, swapping it with the current one,
    /// and forgets the bases, so the next sync merges from scratch.
    pub fn rollback(&self, item: &SyncItem) -> Result<usize> {
        let dir = self.dir(&item.name);
        let prev = dir.join("prev");
        let swap = dir.join("swap");
        remove_any(&swap)?;
        let mut n = 0;
        for rel in walk(&prev, &prev)? {
            let (old, cur) = (prev.join(&rel), self.home.join(&rel));
            if cur.exists() {
                move_file(&cur, &swap.join(&rel))?;
            }
            move_file(&old, &cur)?;
            n += 1;
        }
        for f in keyring::prevs(self.runner, &item.name)? {
            keyring::store(self.runner, &f)?;
            n += 1;
        }
        keyring::clear_prevs(self.runner, &item.name)?;
        remove_any(&prev)?;
        if swap.exists() {
            fs::rename(&swap, &prev)?;
        }
        remove_any(&dir.join("peers"))?;
        Ok(n)
    }

    fn busy(&self, item: &SyncItem) -> Result<Option<String>> {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() }.to_string();
        for name in &item.unless_running {
            if self.runner.run(&Cmd::new("pgrep").args(["-x", "-U", &uid, name]))?.status == 0 {
                return Ok(Some(name.clone()));
            }
        }
        Ok(None)
    }

    fn json(&self, path: &str) -> Result<Option<(serde_json::Map<String, Value>, i64)>> {
        let file = self.at(path);
        let Ok(bytes) = fs::read(&file) else { return Ok(None) };
        let v: Value = serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", file.display()))?;
        let Value::Object(map) = v else { bail!("{} is not a JSON object", file.display()) };
        Ok(Some((map, mtime(&fs::metadata(&file)?))))
    }

    fn write_keys(&self, req: &Apply, prev: &Path) -> Result<()> {
        let mut by_file: BTreeMap<&str, Vec<(&str, &Option<Value>)>> = BTreeMap::new();
        for (k, v) in &req.changes.keys {
            let (file, key) = k.rsplit_once('#').context("a JSON key is <file>#<key>")?;
            by_file.entry(file).or_default().push((key, v));
        }
        for (file, keys) in by_file {
            let path = self.at(file);
            let mut map = self.json(file)?.map(|(m, _)| m).unwrap_or_default();
            if path.exists() {
                let to = prev.join(file.strip_prefix("~/").unwrap_or(file));
                fs::create_dir_all(to.parent().context("a path has a parent")?)?;
                fs::copy(&path, &to)?;
            }
            for (key, v) in keys {
                match v {
                    Some(v) => map.insert(key.to_string(), v.clone()),
                    None => map.remove(key),
                };
            }
            fs::create_dir_all(path.parent().context("a path has a parent")?)?;
            let mut text = serde_json::to_string_pretty(&Value::Object(map))?;
            text.push('\n');
            fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        }
        Ok(())
    }

    /// `on_receive` with each `{key}` replaced by its value here, shell-quoted.
    fn on_receive(&self, item: &SyncItem) -> Result<()> {
        let Some(cmd) = &item.on_receive else { return Ok(()) };
        let mut line = cmd.clone();
        if let Some(k) = &item.keys
            && let Some((map, _)) = self.json(&k.path)?
        {
            for key in &k.keys {
                let v = match map.get(key) {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => String::new(),
                };
                line = line.replace(&format!("{{{key}}}"), &shell_words::quote(&v));
            }
        }
        self.runner.check(&Cmd::new("sh").args(["-c", &line])).with_context(|| format!("{}: on_receive", item.name))?;
        Ok(())
    }
}

impl Side for Local<'_> {
    fn host(&self) -> &str {
        &self.host
    }

    fn scan(&self, item: &SyncItem, base: &Manifest) -> Result<Manifest> {
        let mut m = Manifest { busy: self.busy(item)?, ..Manifest::default() };
        for p in &item.paths {
            let root = self.at(p);
            if let Some(dir) = p.strip_suffix('/') {
                for rel in walk(&root, &root)? {
                    let rel_s = rel.to_string_lossy();
                    if wanted(item, &rel_s) {
                        let key = format!("{dir}/{rel_s}");
                        m.files.insert(key.clone(), file_meta(&root.join(&rel), base.files.get(&key))?);
                    }
                }
            } else if fs::symlink_metadata(&root).is_ok_and(|md| md.is_file()) {
                m.files.insert(p.clone(), file_meta(&root, base.files.get(p))?);
            }
        }
        for k in &item.keyring {
            for f in keyring::find(self.runner, k)? {
                let Some(secret) = &f.secret else {
                    bail!("the keyring on {} is locked; log in there first", self.host);
                };
                let meta = Meta { id: hex(secret.as_bytes()), time: f.time(), ..Meta::default() };
                m.keyring.insert(f.key(), meta);
            }
        }
        if let Some(k) = &item.keys
            && let Some((map, time)) = self.json(&k.path)?
        {
            for key in &k.keys {
                if let Some(v) = map.get(key) {
                    let meta = Meta { id: v.to_string(), time, value: Some(v.clone()), ..Meta::default() };
                    m.keys.insert(format!("{}#{key}", k.path), meta);
                }
            }
        }
        Ok(m)
    }

    fn pack(&self, _item: &SyncItem, files: &[String], tar: &Path) -> Result<()> {
        let mut b = tar::Builder::new(fs::File::create(tar)?);
        for key in files {
            let name = key.strip_prefix("~/").context("a synced path is under ~/")?;
            b.append_path_with_name(self.at(key), name).with_context(|| format!("packing {key}"))?;
        }
        b.finish()?;
        Ok(())
    }

    fn unpack(&self, item: &str, tar: &Path) -> Result<()> {
        let incoming = self.dir(item).join("incoming");
        remove_any(&incoming)?;
        fs::create_dir_all(&incoming)?;
        tar::Archive::new(fs::File::open(tar)?).unpack(&incoming).context("unpacking the files that arrived")
    }

    fn secrets(&self, item: &SyncItem, keys: &[String]) -> Result<Vec<Found>> {
        let mut out = Vec::new();
        for k in &item.keyring {
            out.extend(keyring::find(self.runner, k)?.into_iter().filter(|f| keys.contains(&f.key())));
        }
        Ok(out)
    }

    fn apply(&self, req: &Apply) -> Result<()> {
        let item = &req.item;
        let dir = self.dir(&item.name);
        let (incoming, prev) = (dir.join("incoming"), dir.join("prev"));
        let c = &req.changes;
        if !c.is_empty() {
            remove_any(&prev)?;
            fs::create_dir_all(&prev)?;
            if !c.keyring.is_empty() {
                keyring::clear_prevs(self.runner, &item.name)?;
            }
            for key in c.delete.iter().chain(&c.files) {
                let cur = self.at(key);
                if fs::symlink_metadata(&cur).is_ok() {
                    move_file(&cur, &prev.join(key.strip_prefix("~/").unwrap_or(key)))?;
                }
            }
            for key in &c.files {
                move_file(&incoming.join(key.strip_prefix("~/").unwrap_or(key)), &self.at(key))
                    .with_context(|| format!("placing {key}"))?;
            }
            self.write_keys(req, &prev)?;
            for f in &req.secrets {
                if let Some(old) = keyring::exact(self.runner, f.schema.as_deref(), &f.attrs)?
                    && old.secret.is_some()
                {
                    keyring::store_prev(self.runner, &item.name, &old)?;
                }
                keyring::store(self.runner, f)?;
            }
        }
        remove_any(&incoming)?;
        let base = self.base_path(&item.name, &req.peer);
        fs::create_dir_all(base.parent().context("a path has a parent")?)?;
        fs::write(&base, serde_json::to_vec(&req.base)?)?;
        if !c.is_empty() {
            self.on_receive(item)?;
        }
        Ok(())
    }
}

/// A file's meta, hashed only when its size or modification time moved since the base.
fn file_meta(path: &Path, base: Option<&Meta>) -> Result<Meta> {
    let md = fs::metadata(path)?;
    let (size, time) = (md.len(), mtime(&md));
    if let Some(b) = base.filter(|b| b.size == size && b.time == time) {
        return Ok(b.clone());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?.read_to_end(&mut bytes).with_context(|| format!("reading {}", path.display()))?;
    Ok(Meta { id: hex(&bytes), time, size, value: None })
}

/// Whole seconds, which is what a tar keeps, so a copy has the sender's time.
fn mtime(md: &fs::Metadata) -> i64 {
    md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64)
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Regular files under `dir`, relative to `root`, skipping symlinks and sockets.
fn walk(root: &Path, dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    for entry in entries {
        let entry = entry?;
        let t = entry.file_type()?;
        if t.is_dir() {
            out.extend(walk(root, &entry.path())?);
        } else if t.is_file() {
            out.push(entry.path().strip_prefix(root)?.to_path_buf());
        }
    }
    out.sort();
    Ok(out)
}

/// A pattern without `/` matches any component of `rel`; one with `/` matches `rel` or one of
/// its parent directories from the declared path.
fn matches(pattern: &str, rel: &str) -> bool {
    let Ok(p) = glob::Pattern::new(pattern) else { return false };
    if !pattern.contains('/') {
        return rel.split('/').any(|c| p.matches(c));
    }
    let opts = glob::MatchOptions { require_literal_separator: true, ..Default::default() };
    let mut prefix = String::new();
    for c in rel.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(c);
        if p.matches_with(&prefix, opts) {
            return true;
        }
    }
    false
}

fn wanted(item: &SyncItem, rel: &str) -> bool {
    (item.include.is_empty() || item.include.iter().any(|p| matches(p, rel)))
        && !item.exclude.iter().any(|p| matches(p, rel))
}

/// Renames `from` to `to`, creating `to`'s parent, and copies when they sit on two filesystems.
fn move_file(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to.parent().context("a path has a parent")?)?;
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    let md = fs::metadata(from).with_context(|| format!("reading {}", from.display()))?;
    fs::copy(from, to).with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    fs::File::options().write(true).open(to)?.set_modified(md.modified()?)?;
    fs::remove_file(from)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn patterns_match_components_or_paths() {
        assert!(matches("Cache", "Default/Cache/data_0"));
        assert!(!matches("Cache", "Default/Cached"));
        assert!(matches("Default/Code Cache", "Default/Code Cache/js/x"));
        assert!(!matches("Default/Code Cache", "Profile 1/Code Cache/x"));
        assert!(matches("*.log", "a/b/c.log"));
    }
}

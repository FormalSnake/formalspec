//! The `file` and `secret` resources: managed copies, symlinks, seeds, three-way patches and
//! age-decrypted secrets.

pub mod adopt;
mod live;
pub mod patch;
mod render;
mod secret;
pub mod watch;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fspec_core::apply::{Op, Records};
use fspec_core::journal::{Journal, remove_any};
use fspec_core::resource::{File, FileMode, Format, OnDrift, Resource, Secret};
use fspec_core::runner::Cmd;
use fspec_core::state::Entry;
use fspec_core::{Backend, Cx, Phase};
use serde_json::{Map, Value};

pub use live::{Live, drift_notes, read_live, sha256};

use patch::Record;

pub struct FileBackend;

impl Backend for FileBackend {
    fn name(&self) -> &'static str {
        "files"
    }

    fn handles(&self, _cx: &Cx, r: &Resource) -> bool {
        matches!(r, Resource::File(_) | Resource::Secret(_))
    }

    fn phase(&self) -> Phase {
        Phase::Files
    }

    fn check(&self, cx: &Cx, r: &Resource) -> Result<()> {
        match r {
            Resource::File(f) => check_file(cx, f).with_context(|| f.path.clone()),
            Resource::Secret(s) => {
                parse_perms(&s.perms).with_context(|| s.path.clone())?;
                require_src(cx, &s.src).with_context(|| s.path.clone())
            }
            _ => Ok(()),
        }
    }

    fn drift(&self, cx: &Cx, entry: &Entry) -> Result<Vec<String>> {
        let Some(path) = target_path(cx, &entry.resource)? else { return Ok(vec![]) };
        let mut notes = drift_notes(&entry.resource, &entry.record, &read_live(&path)?);
        let recorded = live::recorded(&entry.record, "sha256");
        match &entry.resource {
            Resource::File(f) if f.mode == FileMode::Managed => match render_bytes(cx, f) {
                Ok(bytes) if recorded.is_some_and(|r| r != sha256(&bytes)) => notes.push("source changed".into()),
                Ok(_) => {}
                Err(e) => notes.push(format!("source unreadable: {e:#}")),
            },
            Resource::Secret(s) => {
                let src = fs::read(cx.spec_dir.join(&s.src)).map(|b| sha256(&b)).ok();
                if src.is_some() && live::recorded(&entry.record, "src_sha256") != src.as_deref() {
                    notes.push("source changed".into());
                }
            }
            _ => {}
        }
        Ok(notes)
    }

    fn prepare(&self, cx: &Cx, ops: &[Op], staging: &Path) -> Result<()> {
        for op in ops.iter().filter(|o| !o.is_removal()) {
            let staged = staged_path(staging, op);
            match op.resource() {
                Resource::File(f) => match f.mode {
                    FileMode::Managed | FileMode::Seed => stage(&staged, &render_bytes(cx, f)?)?,
                    FileMode::Link => {
                        require_src(cx, f.src.as_deref().unwrap_or_default()).with_context(|| f.path.clone())?
                    }
                    FileMode::Patch => {
                        patch_result(cx, op, f).with_context(|| f.path.clone())?;
                    }
                },
                Resource::Secret(s) => {
                    let key = cx.paths.home.join(".config/fspec/age.key");
                    let plain = secret::decrypt(&cx.spec_dir.join(&s.src), &key).with_context(|| s.path.clone())?;
                    stage(&staged, &plain)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn commit(&self, cx: &Cx, ops: &[Op], staging: &Path, journal: &mut Journal) -> Result<Records> {
        let mut records = Records::new();
        for op in ops {
            let record = match op.resource() {
                Resource::File(f) => commit_file(cx, op, f, staging, journal).with_context(|| f.path.clone())?,
                Resource::Secret(s) => commit_secret(cx, op, s, staging, journal).with_context(|| s.path.clone())?,
                _ => continue,
            };
            if let Some(record) = record {
                records.insert(op.key.clone(), record);
            }
        }
        Ok(records)
    }
}

fn target_path(cx: &Cx, r: &Resource) -> Result<Option<PathBuf>> {
    match r {
        Resource::File(f) => cx.resolve(&f.path).map(Some),
        Resource::Secret(s) => cx.resolve(&s.path).map(Some),
        _ => Ok(None),
    }
}

fn parse_perms(s: &str) -> Result<u32> {
    u32::from_str_radix(s.trim_start_matches("0o"), 8)
        .ok()
        .filter(|m| *m <= 0o7777)
        .with_context(|| format!("perms `{s}` is not an octal mode like \"0644\""))
}

fn require_src(cx: &Cx, src: &str) -> Result<()> {
    let path = cx.spec_dir.join(src);
    if !path.is_file() {
        bail!("src {} does not exist", path.display());
    }
    Ok(())
}

fn check_file(cx: &Cx, f: &File) -> Result<()> {
    if let Some(p) = &f.perms {
        parse_perms(p)?;
    }
    match f.mode {
        FileMode::Managed | FileMode::Seed => {
            match (&f.src, &f.content) {
                (Some(src), None) => require_src(cx, src)?,
                (None, Some(Value::String(_))) => {}
                (None, Some(v)) => {
                    let format = render::format_of(f).context("cannot infer a format from the path; set format")?;
                    render::serialize(v, format)?;
                }
                _ => bail!("set exactly one of src and content"),
            }
            Ok(())
        }
        FileMode::Link => {
            if f.content.is_some() {
                bail!("a link takes src, not content");
            }
            require_src(cx, f.src.as_deref().context("a link needs src")?)
        }
        FileMode::Patch => {
            if f.src.is_some() || f.content.is_some() {
                bail!("a patch takes owns, not src or content");
            }
            match render::format_of(f) {
                Some(Format::Json | Format::Toml | Format::Yaml) => {}
                _ => bail!("patch mode edits json, toml and yaml files; set format or use such an extension"),
            }
            for (key, v) in &f.owns {
                patch::Want::parse(key, v)?;
            }
            Ok(())
        }
    }
}

pub(crate) fn render_bytes(cx: &Cx, f: &File) -> Result<Vec<u8>> {
    match (&f.src, &f.content) {
        (Some(src), None) => {
            let path = cx.spec_dir.join(src);
            fs::read(&path).with_context(|| format!("reading {}", path.display()))
        }
        (None, Some(Value::String(s))) => Ok(s.clone().into_bytes()),
        (None, Some(v)) => {
            let format = render::format_of(f).context("cannot infer a format from the path; set format")?;
            Ok(render::serialize(v, format)?.into_bytes())
        }
        _ => bail!("set exactly one of src and content"),
    }
}

fn staged_path(staging: &Path, op: &Op) -> PathBuf {
    staging.join("files").join(&sha256(op.key.to_string().as_bytes())[..16])
}

fn stage(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::create_dir_all(path.parent().context("staged path has no parent")?)?;
    write_private(path, bytes)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

/// Write-then-rename `bytes` over `dest`, journaling the previous content first. The temporary
/// file is created 0600 so a secret is never world-readable, even briefly.
fn install(journal: &mut Journal, dest: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    journal.backup(dest)?;
    let dir = dest.parent().context("path has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = dest.file_name().context("path has no file name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.fspec-tmp"));
    let mode = mode.or_else(|| fs::metadata(dest).ok().map(|m| m.permissions().mode() & 0o7777)).unwrap_or(0o644);
    let result = write_private(&tmp, bytes)
        .and_then(|()| fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).map_err(Into::into))
        .and_then(|()| fs::rename(&tmp, dest).with_context(|| format!("replacing {}", dest.display())));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn warn_kept(path: &Path) {
    eprintln!("fspec: keeping {}: it changed since fspec wrote it", path.display());
}

fn hash_record(bytes: &[u8]) -> Map<String, Value> {
    let mut rec = Map::new();
    rec.insert("sha256".into(), sha256(bytes).into());
    rec
}

fn commit_file(
    cx: &Cx,
    op: &Op,
    f: &File,
    staging: &Path,
    journal: &mut Journal,
) -> Result<Option<Map<String, Value>>> {
    let path = cx.resolve(&f.path)?;
    if op.is_removal() {
        remove_file_resource(cx, op, f.mode, &path, journal)?;
        return Ok(None);
    }
    let staged = staged_path(staging, op);
    match f.mode {
        FileMode::Managed => {
            let live = read_live(&path)?;
            let drifted = !live::drift_notes(op.resource(), &op.record(), &live).is_empty();
            if f.on_drift == OnDrift::Fail && drifted {
                bail!(
                    "{} was changed outside fspec; run `fspec adopt {}` to keep the change, or set on_drift = \"overwrite\"",
                    path.display(),
                    f.path
                );
            }
            let bytes = fs::read(&staged)?;
            install(journal, &path, &bytes, file_mode(cx, f)?)?;
            Ok(Some(hash_record(&bytes)))
        }
        FileMode::Seed => {
            if fs::symlink_metadata(&path).is_ok() {
                return Ok(Some(op.record()));
            }
            let bytes = fs::read(&staged)?;
            install(journal, &path, &bytes, file_mode(cx, f)?)?;
            Ok(Some(hash_record(&bytes)))
        }
        FileMode::Link => commit_link(cx, op, f, &path, journal).map(Some),
        FileMode::Patch => {
            let (text, record) = patch_result(cx, op, f)?;
            if let Some(text) = text {
                install(journal, &path, text.as_bytes(), None)?;
            }
            Ok(Some(record))
        }
    }
}

fn file_mode(cx: &Cx, f: &File) -> Result<Option<u32>> {
    if let Some(p) = &f.perms {
        return parse_perms(p).map(Some);
    }
    Ok(f.src.as_ref().and_then(|s| fs::metadata(cx.spec_dir.join(s)).ok()).map(|m| m.permissions().mode() & 0o7777))
}

fn commit_link(cx: &Cx, op: &Op, f: &File, path: &Path, journal: &mut Journal) -> Result<Map<String, Value>> {
    let src = f.src.as_deref().context("a link needs src")?;
    let target = std::path::absolute(cx.spec_dir.join(src))?;
    let old = op.record();
    if let (Some(prev), Live::File(_)) = (live::recorded(&old, "target"), read_live(path)?) {
        // The app replaced our symlink with a file; its content belongs in the repo.
        let prev = Path::new(prev);
        journal.backup(prev)?;
        fs::copy(path, prev).with_context(|| format!("copying {} back to {}", path.display(), prev.display()))?;
    }
    let already = matches!(read_live(path)?, Live::Link(t) if t == target);
    if !already {
        journal.backup(path)?;
        let dir = path.parent().context("path has no parent")?;
        fs::create_dir_all(dir)?;
        let name = path.file_name().context("path has no file name")?.to_string_lossy();
        let tmp = dir.join(format!(".{name}.fspec-tmp"));
        remove_any(&tmp)?;
        std::os::unix::fs::symlink(&target, &tmp)?;
        fs::rename(&tmp, path).with_context(|| format!("linking {}", path.display()))?;
    }
    let mut rec = Map::new();
    rec.insert("target".into(), target.to_string_lossy().into_owned().into());
    rec.insert("src".into(), src.into());
    Ok(rec)
}

/// Remove what fspec wrote, but only while it still is what fspec wrote.
fn remove_file_resource(cx: &Cx, op: &Op, mode: FileMode, path: &Path, journal: &mut Journal) -> Result<()> {
    let record = op.record();
    let live = read_live(path)?;
    let remove = match (mode, &live) {
        (_, Live::Missing) => return Ok(()),
        (FileMode::Patch, _) => return retract_patch(cx, op, journal),
        (FileMode::Link, Live::Link(t)) => {
            Some(t.as_path() == Path::new(live::recorded(&record, "target").unwrap_or_default()))
        }
        (FileMode::Link, _) => Some(false),
        (_, Live::File(bytes)) => live::recorded(&record, "sha256").map(|h| h == sha256(bytes)),
        (_, Live::Link(_)) => Some(false),
    };
    match remove {
        Some(true) => {
            journal.backup(path)?;
            remove_any(path)
        }
        Some(false) => {
            warn_kept(path);
            Ok(())
        }
        None => Ok(()),
    }
}

fn retract_patch(cx: &Cx, op: &Op, journal: &mut Journal) -> Result<()> {
    let Resource::File(f) = op.resource() else { return Ok(()) };
    let path = cx.resolve(&f.path)?;
    if let (Some(text), _) = patch_result(cx, op, f)? {
        install(journal, &path, text.as_bytes(), None)?;
    }
    Ok(())
}

/// The new text of a patched file, `None` when nothing changes, and the record to keep.
fn patch_result(cx: &Cx, op: &Op, f: &File) -> Result<(Option<String>, Map<String, Value>)> {
    let path = cx.resolve(&f.path)?;
    let format = render::format_of(f).context("patch mode needs a format")?;
    let doc = match fs::read_to_string(&path) {
        Ok(text) => render::parse(&text, format).with_context(|| format!("parsing {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Default::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let (old_owns, record) = match &op.current {
        Some(Entry { resource: Resource::File(old), record, .. }) => (old.owns.clone(), Record::from_map(record)),
        _ => (Default::default(), Record::default()),
    };
    let owns = if op.is_removal() { Default::default() } else { f.owns.clone() };
    let merged = patch::merge(&doc, &owns, &old_owns, &record, f.on_conflict)?;
    if !merged.conflicts.is_empty() {
        bail!(
            "{} changed since fspec last wrote {} and on_conflict is \"fail\"",
            path.display(),
            merged.conflicts.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join(", ")
        );
    }
    let text = if merged.changed { Some(render::serialize(&merged.doc, format)?) } else { None };
    Ok((text, merged.record.to_map()))
}

fn commit_secret(
    cx: &Cx,
    op: &Op,
    s: &Secret,
    staging: &Path,
    journal: &mut Journal,
) -> Result<Option<Map<String, Value>>> {
    let path = cx.resolve(&s.path)?;
    if op.is_removal() {
        remove_file_resource(cx, op, FileMode::Managed, &path, journal)?;
        return Ok(None);
    }
    let plain = fs::read(staged_path(staging, op))?;
    install(journal, &path, &plain, Some(parse_perms(&s.perms)?))?;
    if let Some(owner) = &s.owner {
        cx.runner.check(&Cmd::new("chown").arg(owner).arg(path.to_string_lossy()))?;
    }
    let mut rec = hash_record(&plain);
    rec.insert("src_sha256".into(), sha256(&fs::read(cx.spec_dir.join(&s.src))?).into());
    Ok(Some(rec))
}

#[cfg(test)]
mod tests;

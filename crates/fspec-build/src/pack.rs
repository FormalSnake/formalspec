use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Extensions of files a build leaves next to its executables.
const NOT_BINS: &[&str] = &["d", "a", "so", "dylib", "rlib", "rmeta", "dll", "pdb", "o"];

/// The executables to ship as `(file name, path)`: `names` when given, else every executable
/// regular file in the first directory that exists.
pub fn executables(dirs: &[PathBuf], names: &[String]) -> Result<Vec<(String, PathBuf)>> {
    if !names.is_empty() {
        return names
            .iter()
            .map(|n| {
                let path = dirs.iter().map(|d| d.join(n)).find(|p| p.is_file());
                path.map(|p| (n.clone(), p)).with_context(|| format!("the build produced no executable named `{n}`"))
            })
            .collect();
    }
    let Some(dir) = dirs.iter().find(|d| d.is_dir()) else { bail!("the build produced no bin directory") };
    let mut found: Vec<(String, PathBuf)> = fs::read_dir(dir)?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = fs::metadata(e.path()).ok()?;
            let ext = Path::new(&name).extension().and_then(|x| x.to_str()).unwrap_or("");
            (meta.is_file()
                && meta.permissions().mode() & 0o111 != 0
                && !name.starts_with('.')
                && !NOT_BINS.contains(&ext))
            .then_some((name, e.path()))
        })
        .collect();
    found.sort();
    if found.is_empty() {
        bail!("no executables in {}", dir.display());
    }
    Ok(found)
}

/// `.tar.zst` of the whole tree under `dir`: directories, files with their modes, and links as
/// links. Paths are sorted and owners and times zeroed, like [`tar_zst`].
pub fn tree_zst(dir: &Path) -> Result<Vec<u8>> {
    let mut paths = Vec::new();
    walk(dir, Path::new(""), &mut paths)?;
    if paths.is_empty() {
        bail!("the build installed nothing into {}", dir.display());
    }
    let mut tar = tar::Builder::new(Vec::new());
    for rel in paths {
        let path = dir.join(&rel);
        let meta = fs::symlink_metadata(&path)?;
        let mut h = tar::Header::new_gnu();
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mode(meta.permissions().mode() & 0o7777);
        if meta.file_type().is_symlink() {
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            tar.append_link(&mut h, &rel, fs::read_link(&path)?)?;
        } else if meta.is_dir() {
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            tar.append_data(&mut h, &rel, std::io::empty())?;
        } else {
            let body = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            h.set_size(body.len() as u64);
            tar.append_data(&mut h, &rel, body.as_slice())?;
        }
    }
    let tar = tar.into_inner()?;
    Ok(ruzstd::encoding::compress_to_vec(tar.as_slice(), ruzstd::encoding::CompressionLevel::Fastest))
}

/// Every path under `dir`, parents before children, siblings sorted.
fn walk(dir: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir.join(rel))?.flatten().map(|e| e.file_name()).collect();
    entries.sort();
    for name in entries {
        let child = rel.join(name);
        out.push(child.clone());
        if fs::symlink_metadata(dir.join(&child))?.is_dir() {
            walk(dir, &child, out)?;
        }
    }
    Ok(())
}

/// `.tar.zst` with the executables at the top. Names are sorted and owners and times zeroed, so
/// the same build packs to the same bytes.
pub fn tar_zst(files: &[(String, PathBuf)]) -> Result<Vec<u8>> {
    let mut tar = tar::Builder::new(Vec::new());
    for (name, path) in files {
        let body = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(0o755);
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        tar.append_data(&mut h, name, body.as_slice())?;
    }
    let tar = tar.into_inner()?;
    Ok(ruzstd::encoding::compress_to_vec(tar.as_slice(), ruzstd::encoding::CompressionLevel::Fastest))
}

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};

/// A patch path as written in the spec, checked to stay inside the spec directory.
pub fn patch_path(spec_dir: &Path, rel: &str) -> Result<PathBuf> {
    let path = Path::new(rel);
    if path.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        bail!("patch `{rel}` must be a path inside the spec's directory");
    }
    Ok(spec_dir.join(path))
}

/// `sha256-<base64>` over each patch's name and contents, in the order the spec lists them.
/// `fspec update` stores it in the lock and the store refuses a package whose patches differ.
pub fn hash(spec_dir: &Path, patches: &[String]) -> Result<String> {
    let mut sha = Sha256::new();
    for rel in patches {
        let text = fs::read(patch_path(spec_dir, rel)?).with_context(|| format!("reading patch {rel}"))?;
        sha.update((rel.len() as u64).to_le_bytes());
        sha.update(rel.as_bytes());
        sha.update((text.len() as u64).to_le_bytes());
        sha.update(&text);
    }
    Ok(format!("sha256-{}", STANDARD.encode(sha.finalize())))
}

/// Refuse a package whose spec patches no longer match the lock.
pub fn check(spec_dir: &Path, patches: &[String], locked: Option<&str>) -> Result<()> {
    let want = if patches.is_empty() { None } else { Some(hash(spec_dir, patches)?) };
    if want.as_deref() != locked {
        bail!("patches changed, run fspec update");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patches(files: &[(&str, &str)]) -> (tempfile::TempDir, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let p = dir.path().join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }
        let names = files.iter().map(|(n, _)| n.to_string()).collect();
        (dir, names)
    }

    #[test]
    fn hash_follows_names_contents_and_order() {
        let (dir, names) = patches(&[("patches/a.patch", "one"), ("patches/b.patch", "two")]);
        let base = hash(dir.path(), &names).unwrap();
        assert!(base.starts_with("sha256-"));
        assert_eq!(base, hash(dir.path(), &names).unwrap());
        let reversed: Vec<String> = names.iter().rev().cloned().collect();
        assert_ne!(base, hash(dir.path(), &reversed).unwrap());
        fs::write(dir.path().join("patches/b.patch"), "changed").unwrap();
        assert_ne!(base, hash(dir.path(), &names).unwrap());
    }

    #[test]
    fn check_refuses_a_stale_or_missing_hash() {
        let (dir, names) = patches(&[("p.patch", "one")]);
        let good = hash(dir.path(), &names).unwrap();
        check(dir.path(), &names, Some(&good)).unwrap();
        check(dir.path(), &[], None).unwrap();
        for locked in [None, Some("sha256-nope")] {
            let err = check(dir.path(), &names, locked).unwrap_err();
            assert!(err.to_string().contains("patches changed, run fspec update"), "{err}");
        }
        assert!(check(dir.path(), &[], Some(&good)).is_err());
    }

    #[test]
    fn patch_paths_stay_inside_the_spec() {
        assert!(patch_path(Path::new("/s"), "../x.patch").is_err());
        assert!(patch_path(Path::new("/s"), "/etc/passwd").is_err());
        assert!(patch_path(Path::new("/s"), "patches/x.patch").is_ok());
    }
}

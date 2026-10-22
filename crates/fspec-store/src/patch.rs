//! Patches on packages that ship as an unpacked tree (npm, release archives): applied in pure
//! Rust before the tree gets its store path. Git-style or plain unified diffs with `a/` and `b/`
//! prefixes; text changes, file creation, deletion and rename (of any file, binary included, when
//! the rename changes no content).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use diffy::patch_set::{FileMode, FileOperation, ParseOptions, PatchKind, PatchSet};
use fspec_core::patches::patch_path;

pub fn apply_all(tree: &Path, spec_dir: &Path, patches: &[String]) -> Result<()> {
    for rel in patches {
        let text = fs::read_to_string(patch_path(spec_dir, rel)?).with_context(|| format!("reading patch {rel}"))?;
        apply(tree, &text).with_context(|| format!("applying patch {rel}"))?;
    }
    Ok(())
}

fn inside(tree: &Path, rel: &str) -> Result<PathBuf> {
    let path = Path::new(rel);
    if path.components().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("`{rel}` leaves the package tree");
    }
    Ok(tree.join(path))
}

fn apply(tree: &Path, text: &str) -> Result<()> {
    for file in PatchSet::parse(text, ParseOptions::gitdiff()) {
        let file = file?;
        let PatchKind::Text(patch) = file.patch() else { bail!("binary patches are not supported") };
        let mode = file.new_mode().copied();
        // `rename from` and `copy from` lines carry no a/ b/ prefix, so only the rest is stripped.
        let op = match file.operation() {
            op @ (FileOperation::Rename { .. } | FileOperation::Copy { .. }) => op.strip_prefix(0),
            op => op.strip_prefix(1),
        };
        match op {
            FileOperation::Modify { modified, .. } => {
                let path = inside(tree, &modified)?;
                let base = read(&path)?;
                write(&path, &patched(&base, patch, &modified)?, mode)?;
            }
            FileOperation::Create(name) => {
                let path = inside(tree, &name)?;
                if path.exists() {
                    bail!("{name} already exists");
                }
                write(&path, &patched("", patch, &name)?, mode)?;
            }
            FileOperation::Delete(name) => {
                let path = inside(tree, &name)?;
                patched(&read(&path)?, patch, &name)?;
                fs::remove_file(&path).with_context(|| format!("removing {name}"))?;
            }
            FileOperation::Rename { from, to } => {
                let (src, dest) = (inside(tree, &from)?, inside(tree, &to)?);
                // A pure rename carries no hunks and may move a binary file (Discord's app.asar).
                if patch.hunks().is_empty() {
                    fs::rename(&src, &dest).with_context(|| format!("renaming {from} to {to}"))?;
                    continue;
                }
                write(&dest, &patched(&read(&src)?, patch, &from)?, mode)?;
                fs::remove_file(&src).with_context(|| format!("removing {from}"))?;
            }
            FileOperation::Copy { from, to } => {
                write(&inside(tree, &to)?, &patched(&read(&inside(tree, &from)?)?, patch, &from)?, mode)?;
            }
        }
    }
    Ok(())
}

fn read(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn patched(base: &str, patch: &diffy::Patch<'_, str>, name: &str) -> Result<String> {
    diffy::apply(base, patch).map_err(|e| anyhow::anyhow!("{name}: {e}"))
}

fn write(path: &Path, text: &str, mode: Option<FileMode>) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    match mode {
        Some(FileMode::Executable) => fs::set_permissions(path, fs::Permissions::from_mode(0o755))?,
        Some(FileMode::Regular) => fs::set_permissions(path, fs::Permissions::from_mode(0o644))?,
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODIFY: &str = "\
diff --git a/src/hello.txt b/src/hello.txt
--- a/src/hello.txt
+++ b/src/hello.txt
@@ -1,2 +1,2 @@
 hello
-world
+patched
";

    #[test]
    fn modifies_creates_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/hello.txt"), "hello\nworld\n").unwrap();
        fs::write(dir.path().join("gone.txt"), "bye\n").unwrap();
        let more = "\
diff --git a/new.txt b/new.txt
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+fresh
diff --git a/gone.txt b/gone.txt
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
";
        apply(dir.path(), &format!("{MODIFY}{more}")).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("src/hello.txt")).unwrap(), "hello\npatched\n");
        assert_eq!(fs::read_to_string(dir.path().join("new.txt")).unwrap(), "fresh\n");
        assert!(!dir.path().join("gone.txt").exists());
    }

    #[test]
    fn a_pure_rename_moves_a_binary_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("resources")).unwrap();
        fs::write(dir.path().join("resources/app.asar"), [0u8, 159, 146, 150]).unwrap();
        let rename = "\
diff --git a/resources/app.asar b/resources/_app.asar
similarity index 100%
rename from resources/app.asar
rename to resources/_app.asar
";
        apply(dir.path(), rename).unwrap();
        assert_eq!(fs::read(dir.path().join("resources/_app.asar")).unwrap(), [0u8, 159, 146, 150]);
        assert!(!dir.path().join("resources/app.asar").exists());
    }

    #[test]
    fn a_hunk_that_does_not_match_fails() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/hello.txt"), "different\ncontent\n").unwrap();
        assert!(apply(dir.path(), MODIFY).is_err());
    }

    #[test]
    fn paths_cannot_leave_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let evil = MODIFY.replace("src/hello.txt", "../escape.txt");
        assert!(apply(dir.path(), &evil).is_err());
    }
}

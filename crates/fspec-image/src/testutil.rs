use std::path::PathBuf;

/// Compare `actual` with `tests/golden/<name>`. `UPDATE_GOLDEN=1` rewrites the file.
pub fn golden(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    assert_eq!(actual, want, "{name} differs from its golden file; rerun with UPDATE_GOLDEN=1 to accept");
}

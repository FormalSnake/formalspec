use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Every filesystem location FormalSpec touches, rooted so tests and `--root` never reach the live system.
#[derive(Clone, Debug)]
pub struct Paths {
    /// `/` on a real host; a temp dir in tests.
    pub root: PathBuf,
    /// The applying user's home, already under `root` when `root` is not `/`.
    pub home: PathBuf,
}

fn passwd_home(passwd: &str, uid: u32) -> Option<PathBuf> {
    passwd
        .lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find_map(|f| (f.len() >= 6 && f[2].parse() == Ok(uid)).then(|| PathBuf::from(f[5])))
}

impl Paths {
    pub fn new(root: PathBuf, home: PathBuf) -> Paths {
        Paths { root, home }
    }

    /// `/` and `$HOME`, or the user's home in /etc/passwd when HOME is unset, as it is for a
    /// systemd system service.
    pub fn live() -> Result<Paths> {
        let home = match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home),
            None => {
                let passwd = std::fs::read_to_string("/etc/passwd").context("HOME is not set")?;
                passwd_home(&passwd, unsafe { libc::getuid() }).context("HOME is not set")?
            }
        };
        Ok(Paths::new(PathBuf::from("/"), home))
    }

    /// A path from the system root, e.g. `system("/etc/hosts")`.
    pub fn system(&self, abs: impl AsRef<Path>) -> PathBuf {
        let abs = abs.as_ref();
        self.root.join(abs.strip_prefix("/").unwrap_or(abs))
    }

    /// Resolve a path as written in the spec: `~/…` lands in `home`, `/…` under `root`.
    pub fn resolve(&self, spec_path: &str) -> Result<PathBuf> {
        if spec_path == "~" {
            Ok(self.home.clone())
        } else if let Some(rest) = spec_path.strip_prefix("~/") {
            Ok(self.home.join(rest))
        } else if spec_path.starts_with('/') {
            Ok(self.system(spec_path))
        } else {
            bail!("path `{spec_path}` must be absolute or start with `~/`")
        }
    }

    /// The inverse of [`Paths::resolve`]: `~/…` under the home, `/…` under the root.
    pub fn to_spec(&self, abs: &Path) -> String {
        if let Ok(rest) = abs.strip_prefix(&self.home) {
            return if rest.as_os_str().is_empty() { "~".into() } else { format!("~/{}", rest.display()) };
        }
        match abs.strip_prefix(&self.root) {
            Ok(rest) => format!("/{}", rest.display()),
            Err(_) => abs.display().to_string(),
        }
    }

    pub fn system_state_dir(&self) -> PathBuf {
        self.system("/var/lib/fspec")
    }

    pub fn user_state_dir(&self) -> PathBuf {
        self.home.join(".local/state/fspec")
    }

    /// `/opt/fspec`: the store, generations and `current`.
    pub fn fspec_root(&self) -> PathBuf {
        self.system("/opt/fspec")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_home_comes_from_the_passwd_line_of_the_uid() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\nkyan:x:1000:1000::/home/kyan:/usr/bin/fish\n";
        assert_eq!(passwd_home(passwd, 0), Some(PathBuf::from("/root")));
        assert_eq!(passwd_home(passwd, 1000), Some(PathBuf::from("/home/kyan")));
        assert_eq!(passwd_home(passwd, 7), None);
    }
}

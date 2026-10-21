//! Running AerynOS's moss without it taking boot. Every moss transaction syncs boot through
//! blsforme, which rewrites `loader.conf` to `default "aerynos*"`, so on a shared ESP any package
//! change would make AerynOS the root that boots.

use crate::runner::Cmd;

/// Plain sh and `cat`: AerynOS's base system has no grep.
const KEEP_DEFAULT: &str = r#"c=$1; shift; keep=
if [ -f "$c" ]; then
  keep=$(cat "$c")
  while read -r k v; do
    if [ "$k" = default ]; then case $v in aerynos* | '"aerynos'*) keep= ;; esac; fi
  done <"$c"
fi
moss "$@"; rc=$?
if [ -n "$keep" ]; then printf '%s\n' "$keep" >"$c"; fi
exit $rc"#;

/// `moss <args>`, putting `loader_conf` back afterwards unless AerynOS already owned boot.
pub fn moss<I, S>(loader_conf: &str, args: I) -> Cmd
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    Cmd::new("sh").args(["-c", KEEP_DEFAULT, "moss", loader_conf]).args(args)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn run(before: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("loader.conf");
        fs::write(&conf, before).unwrap();
        let fake = dir.path().join("moss");
        fs::write(&fake, format!("#!/bin/sh\nprintf 'default \"aerynos*\"\\n' >{}\n", conf.display())).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let cmd = super::moss(conf.to_str().unwrap(), ["-y", "install", "nano"]);
        let path = format!("{}:{}", dir.path().display(), std::env::var("PATH").unwrap_or_default());
        let status = std::process::Command::new(&cmd.program).args(&cmd.args).env("PATH", path).status().unwrap();
        assert!(status.success());
        fs::read_to_string(&conf).unwrap()
    }

    #[test]
    fn keeps_another_roots_default() {
        assert_eq!(run("timeout 3\ndefault fedora-*\n"), "timeout 3\ndefault fedora-*\n");
    }

    #[test]
    fn lets_moss_rewrite_its_own_default() {
        assert_eq!(run("default aerynos-*\n"), "default \"aerynos*\"\n");
    }
}

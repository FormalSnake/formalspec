use anyhow::{Context, Result, bail};
use fspec_core::Paths;
use fspec_core::runner::{Cmd, Runner};
use serde_json::Value;

use crate::Plan;

/// Never reset: fspec's own state, its store, and the user homes (`/home` is a symlink to
/// `/var/home` on bootc systems).
const ALWAYS: [&str; 6] = ["/var/lib/fspec", "/opt/fspec", "/var/opt/fspec", "/home", "/var/home", "/var/roothome"];

/// Logins live in these; they are reset like any other `/etc` file unless listed in `persist`.
const ACCOUNTS: [&str; 3] = ["/etc/passwd", "/etc/shadow", "/etc/group"];

const DIR: &str = "/var/lib/fspec/reset-var";
const UNIT: &str = "/etc/systemd/system/fspec-reset-var.service";

pub(crate) const SCRIPT: &str = r#"#!/bin/sh
# Staged by `fspec switch --reset-var`, run once from fspec-reset-var.service before /etc and /var
# are used. The marker is removed last, so an interrupted run repeats on the next boot.
set -eu
dir=/var/lib/fspec/reset-var
keep=$dir/keep

kept() { grep -Fxq -- "$1" "$keep"; }
holds() { awk -v p="$1/" 'index($0, p) == 1 { found = 1 } END { exit !found }' "$keep"; }

purge() {
	for entry in "$1"/* "$1"/.[!.]* "$1"/..?*; do
		[ -e "$entry" ] || [ -L "$entry" ] || continue
		if kept "$entry"; then continue; fi
		if holds "$entry"; then purge "$entry"; continue; fi
		rm -rf --one-file-system -- "$entry"
	done
}

purge /var
purge /etc
cp -a --update=none /usr/etc/. /etc/
rm -f "$dir/pending"
"#;

const SERVICE: &str = "[Unit]
Description=Reset /etc and /var to the image defaults (fspec switch --reset-var)
DefaultDependencies=no
ConditionPathExists=/var/lib/fspec/reset-var/pending
RequiresMountsFor=/var
After=local-fs.target
Before=sysinit.target systemd-tmpfiles-setup.service

[Service]
Type=oneshot
ExecStart=/bin/sh /var/lib/fspec/reset-var/reset.sh

[Install]
WantedBy=sysinit.target
";

pub struct ResetVar {
    pub plan: Plan,
    /// Paths the reset keeps, `ALWAYS` first and then the host's `persist`.
    pub keep: Vec<String>,
    pub warnings: Vec<String>,
}

/// Stage a reset of `/etc` and `/var` for the next boot. The running system cannot reset its own
/// `/etc`, so this only writes the keep list, the script and a oneshot unit; the marker goes in
/// last, and removing `/var/lib/fspec/reset-var/pending` cancels it.
pub fn reset_var(paths: &Paths, persist: &[String]) -> Result<ResetVar> {
    let mut keep: Vec<String> = ALWAYS.iter().map(|p| p.to_string()).collect();
    for p in persist {
        let clean = p.trim_end_matches('/');
        if !clean.starts_with('/') || clean.split('/').any(|c| c == "..") {
            bail!("persist entry `{p}` must be an absolute path without `..`");
        }
        if !keep.iter().any(|k| k == clean) {
            keep.push(clean.to_string());
        }
    }
    let warnings = ACCOUNTS
        .iter()
        .filter(|a| !keep.iter().any(|k| k == *a))
        .map(|a| format!("{a} is not in persist and will be reset"))
        .collect();

    let at = |p: &str| paths.system(p).display().to_string();
    let mut plan = Plan::default();
    plan.run(Cmd::new("mkdir").arg("-p").arg(at(DIR)));
    plan.run(Cmd::new("tee").arg(at(&format!("{DIR}/keep"))).stdin(keep.join("\n") + "\n"));
    plan.run(Cmd::new("tee").arg(at(&format!("{DIR}/reset.sh"))).stdin(SCRIPT));
    plan.run(Cmd::new("chmod").arg("755").arg(at(&format!("{DIR}/reset.sh"))));
    plan.run(Cmd::new("tee").arg(at(UNIT)).stdin(SERVICE));
    plan.run(Cmd::new("systemctl").args(["enable", "fspec-reset-var.service"]));
    plan.run(Cmd::new("tee").arg(at(&format!("{DIR}/pending"))).stdin("1\n"));
    Ok(ResetVar { plan, keep, warnings })
}

fn bootc_status(runner: &dyn Runner) -> Result<Option<Value>> {
    let out = runner.run(&Cmd::new("bootc").args(["status", "--json"]))?;
    let text = out.stdout.trim();
    if out.status != 0 || text.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(text).context("parsing `bootc status --json`")?))
}

/// A deployment is staged for the next boot, which is what `--reset-var` resets into.
pub fn has_staged(runner: &dyn Runner) -> Result<bool> {
    Ok(bootc_status(runner)?.is_some_and(|s| !s["status"]["staged"].is_null()))
}

/// The running system was deployed by bootc.
pub fn is_bootc(runner: &dyn Runner) -> Result<bool> {
    Ok(bootc_status(runner)?.is_some_and(|s| !s["status"]["booted"].is_null()))
}

/// Convert this classic system into the bootc image `image` (a `repo@sha256:...` from the lock)
/// with `bootc install to-existing-root`. It reinitialises `/boot`, so the caller must have
/// confirmed it.
pub fn to_image(paths: &Paths, image: &str) -> Plan {
    let mut plan = Plan::default();
    plan.run(
        Cmd::new("podman")
            .args(["run", "--rm", "--privileged", "--pid=host"])
            .args(["-v", "/dev:/dev", "-v", "/var/lib/containers:/var/lib/containers", "-v"])
            .arg(format!("{}:/target", paths.root.display()))
            .args(["--security-opt", "label=type:unconfined_t"])
            .arg(image)
            .args(["bootc", "install", "to-existing-root", "--acknowledge-destructive"]),
    );
    plan
}

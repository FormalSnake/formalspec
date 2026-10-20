use anyhow::{Result, bail};
use fspec_core::Paths;
use fspec_core::runner::Cmd;

use crate::classic::{Mounts, mounts, remove_boot_entry};
use crate::machine::Machine;
use crate::{DISTROS, Plan, subvol};

/// Delete the root subvolume `fspec switch --distro` created for `distro`, and its boot entry.
/// The subvolume is separate, so every system file that distro left behind goes with it.
pub fn forget(paths: &Paths, m: &Machine, distro: &str) -> Result<Plan> {
    if !DISTROS.contains(&distro) {
        bail!("`{distro}` is not a distro fspec switch creates; expected one of {}", DISTROS.join(", "));
    }
    let new = subvol(distro);
    if m.running == distro || m.root_subvol == format!("/{new}") {
        bail!("{distro} is the system you booted; boot another distro before forgetting it");
    }
    let mut plan = Plan::default();
    let Mounts { top, .. } = mounts(paths, distro);
    plan.run(Cmd::new("mkdir").arg("-p").arg(&top));
    plan.run(Cmd::new("mount").args(["-o", "subvolid=5"]).arg(format!("UUID={}", m.uuid)).arg(&top));
    plan.cleanup.push(Cmd::new("umount").arg(&top));
    plan.expect(
        Cmd::new("test").arg("-e").arg(format!("{top}/{new}")),
        true,
        format!("there is no {new} subvolume; fspec switch --distro {distro} never created one here"),
    );
    remove_boot_entry(&mut plan, paths, m, distro);
    plan.run(Cmd::new("btrfs").args(["subvolume", "delete", "--recursive"]).arg(format!("{top}/{new}")));
    Ok(plan)
}

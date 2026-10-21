use anyhow::{Result, bail};
use fspec_core::Paths;
use fspec_core::runner::Cmd;

use crate::classic::{Mounts, mounts, remove_boot_entry, require_owner};
use crate::machine::{Bootloader, Machine, Owner};
use crate::{DISTROS, Plan, subvol};

/// Delete the root subvolume `fspec switch --distro` created for `distro`, and its boot entry.
/// The subvolume is separate, so every system file that distro left behind goes with it. Only the
/// root that owns boot may forget another, so the bootloader never goes with a deleted root.
pub fn forget(paths: &Paths, m: &Machine, owner: Option<&Owner>, distro: &str) -> Result<Plan> {
    if !DISTROS.contains(&distro) {
        bail!("`{distro}` is not a distro fspec switch creates; expected one of {}", DISTROS.join(", "));
    }
    let new = subvol(distro);
    if m.running == distro || m.root_subvol == format!("/{new}") {
        bail!("{distro} is the system you booted; boot another distro before forgetting it");
    }
    require_owner(m, owner)?;
    if let Bootloader::SystemdBoot { default: Some(entry), .. } = &m.bootloader
        && entry.split_whitespace().any(|w| w == format!("rootflags=subvol={new}"))
    {
        bail!(
            "the default systemd-boot entry boots {new}; run `fspec switch --adopt-boot` from {} first",
            m.root_subvol
        );
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
    if let Bootloader::Grub { .. } = m.bootloader {
        // Every root keeps its own GRUB scripts, and with no separate /boot its own kernel copies.
        // A root that adopts boot later must not list the deleted one.
        plan.run(
            Cmd::new("sh")
                .args([
                    "-c",
                    r#"rm -rf "$1"/etc/grub.d/42_fspec_"$2" "$1"/*/etc/grub.d/42_fspec_"$2" "$1"/boot/fspec/"$2" "$1"/*/boot/fspec/"$2""#,
                    "sh",
                ])
                .arg(&top)
                .arg(distro),
        );
    }
    plan.run(Cmd::new("btrfs").args(["subvolume", "delete", "--recursive"]).arg(format!("{top}/{new}")));
    Ok(plan)
}

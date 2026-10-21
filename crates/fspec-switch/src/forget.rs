use anyhow::{Result, bail};
use fspec_core::Paths;
use fspec_core::runner::Cmd;

use crate::classic::{Mounts, Sources, WORK, grub_menu, mounts, require_owner, s};
use crate::machine::{Layout, Machine};
use crate::{DISTROS, Plan, subvol};

/// Delete the root `fspec switch --distro` created for `distro`, with its kernels and boot
/// entries. The root is separate, so every system file that distro left behind goes with it. Only
/// the root that owns boot may forget another, so the bootloader's default never goes with a
/// deleted root.
pub fn forget(paths: &Paths, m: &Machine, owner: Option<&str>, distro: &str) -> Result<Plan> {
    if !DISTROS.contains(&distro) {
        bail!("`{distro}` is not a distro fspec switch creates; expected one of {}", DISTROS.join(", "));
    }
    if m.running == distro {
        bail!("{distro} is the system you booted; boot another distro before forgetting it");
    }
    require_owner(m, owner)?;
    let src = Sources::running(paths);
    let mut plan = Plan::default();
    let Mounts { top, .. } = mounts(paths, distro);
    match &m.layout {
        Layout::Zfs { pool, .. } => {
            let ds = format!("{pool}/ROOT/{distro}");
            plan.expect(
                Cmd::new("zfs").args(["list", "-H", "-o", "name", &ds]),
                true,
                format!("there is no dataset {ds}; fspec switch --distro {distro} never created one here"),
            );
            plan.run(Cmd::new("zfs").args(["destroy", "-r", &ds]));
            return Ok(plan);
        }
        Layout::Default { btrfs, .. } | Layout::Bios { btrfs, .. } => {
            let new = subvol(distro);
            plan.run(Cmd::new("mkdir").arg("-p").arg(&top));
            plan.run(Cmd::new("mount").args(["-o", "subvolid=5"]).arg(format!("UUID={}", btrfs.uuid)).arg(&top));
            plan.cleanup.push(Cmd::new("umount").arg(&top));
            plan.expect(
                Cmd::new("test").arg("-e").arg(format!("{top}/{new}")),
                true,
                format!("there is no {new} subvolume; fspec switch --distro {distro} never created one here"),
            );
            if let Layout::Bios { boot, .. } = &m.layout {
                plan.run(Cmd::new("rm").arg("-rf").arg(format!("{}/{distro}", src.shared_boot)));
                plan.run(grub_menu(&src.shared_boot, &boot.uuid, &m.running));
            } else {
                let clear = r#"rm -rf "$1/$2" "$1"/EFI/Linux/"$2"-*.efi "$1"/loader/entries/"$2"-*.conf"#;
                plan.run(Cmd::new("sh").args(["-c", clear, "sh", &src.esp, distro]));
                if distro == "aerynos" {
                    // blsforme keeps AerynOS's kernels under EFI/aerynos.
                    plan.run(Cmd::new("rm").arg("-rf").arg(format!("{}/EFI/aerynos", src.esp)));
                }
            }
            plan.run(Cmd::new("btrfs").args(["subvolume", "delete", "--recursive"]).arg(format!("{top}/{new}")));
        }
        Layout::Unsupported { reasons } => {
            bail!("this disk is not on a supported layout: {}; run `fspec disk plan` for the steps", reasons.join("; "))
        }
    }
    Ok(plan.private(&s(&paths.system(WORK))))
}

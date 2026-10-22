use anyhow::{Result, bail};
use fspec_core::Paths;
use fspec_core::runner::Cmd;

use crate::beside::{esp, valid_name};
use crate::classic::{Mounts, Sources, WORK, grub_menu, mounts, require_owner, s};
use crate::machine::{BTRFS_OPTS, Firmware, Layout, Machine};
use crate::{DISTROS, Plan, subvol};

/// Removes `$2`'s kernels and entries from the ESP at `$1`: its folder there, the folder its own
/// bootloader setup keeps under `EFI/` (NixOS's systemd-boot, AerynOS's blsforme), its UKIs and
/// its `$2-*.conf` entries.
const CLEAR_ESP: &str = r#"rm -rf "$1/$2" "$1/EFI/$2" "$1"/EFI/Linux/"$2"-*.efi "$1"/loader/entries/"$2"-*.conf"#;

/// Delete the root `fspec switch --distro` created for `distro`, or the NixOS a migration moved
/// into `@nixos`, with its kernels and boot entries. The root is separate, so every system file
/// that distro left behind goes with it. Only the root that owns boot may forget another, so the
/// bootloader's default never goes with a deleted root. Off the default layout, a root installed
/// beside another (`--beside`) may forget any `@<name>` but its own.
pub fn forget(paths: &Paths, m: &Machine, owner: Option<&str>, distro: &str) -> Result<Plan> {
    if m.running == distro || m.root == format!("/{}", subvol(distro)) {
        bail!("{distro} is the system you booted; boot another distro before forgetting it");
    }
    if let Layout::Unsupported { reasons } = &m.layout {
        return match esp(m) {
            Some((_, at)) if m.firmware == Firmware::Efi && m.fstype == "btrfs" => forget_beside(paths, m, at, distro),
            _ => bail!(
                "this disk is not on a supported layout: {}; run `fspec disk plan` for the steps",
                reasons.join("; ")
            ),
        };
    }
    if !DISTROS.contains(&distro) && distro != "nixos" {
        bail!("`{distro}` is not a distro fspec switch creates; expected nixos or one of {}", DISTROS.join(", "));
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
            plan.run(
                Cmd::new("mount")
                    .arg("-o")
                    .arg(format!("subvolid=5,{BTRFS_OPTS}"))
                    .arg(format!("UUID={}", btrfs.uuid))
                    .arg(&top),
            );
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
                plan.run(Cmd::new("sh").args(["-c", CLEAR_ESP, "sh", &src.esp, distro]));
            }
            plan.run(Cmd::new("btrfs").args(["subvolume", "delete", "--recursive"]).arg(format!("{top}/{new}")));
        }
        Layout::Unsupported { .. } => unreachable!("handled above"),
    }
    Ok(plan.private(&s(&paths.system(WORK))))
}

/// The running root keeps booting on its own entries, and loader.conf does not prefer the root
/// that goes, since nothing records which root owns boot here.
fn forget_beside(paths: &Paths, m: &Machine, esp_at: &str, distro: &str) -> Result<Plan> {
    valid_name(distro)?;
    let own = m.root.trim_start_matches("/@");
    let esp_dir = s(&paths.system(esp_at));
    let Mounts { top, .. } = mounts(paths, distro);
    let new = subvol(distro);
    let mut plan = Plan::default();
    plan.expect(
        Cmd::new("sh").args(["-c", r#"ls "$1"/loader/entries/"$2"-*.conf"#, "sh", &esp_dir, own]),
        true,
        format!("{esp_dir}/loader/entries has no {own}-*.conf entry; the running root would not boot after this"),
    );
    plan.expect(
        Cmd::new("grep")
            .args(["-Eqs", &format!(r#"^(default|preferred)[[:space:]]+"?{distro}"#)])
            .arg(format!("{esp_dir}/loader/loader.conf")),
        false,
        format!("{esp_dir}/loader/loader.conf still picks {distro}; point it at {own} first"),
    );
    plan.run(Cmd::new("mkdir").arg("-p").arg(&top));
    plan.run(
        Cmd::new("mount")
            .arg("-o")
            .arg(format!("subvolid=5,{BTRFS_OPTS}"))
            .arg(format!("UUID={}", m.fs_uuid))
            .arg(&top),
    );
    plan.cleanup.push(Cmd::new("umount").arg(&top));
    plan.expect(
        Cmd::new("test").arg("-e").arg(format!("{top}/{new}")),
        true,
        format!("there is no {new} subvolume on this disk"),
    );
    plan.run(Cmd::new("sh").args(["-c", CLEAR_ESP, "sh", &esp_dir, distro]));
    plan.run(Cmd::new("btrfs").args(["subvolume", "delete", "--recursive"]).arg(format!("{top}/{new}")));
    Ok(plan.private(&s(&paths.system(WORK))))
}

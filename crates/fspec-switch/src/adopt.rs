use anyhow::{Result, bail};
use fspec_core::Paths;
use fspec_core::runner::Cmd;

use crate::Plan;
use crate::classic::{Entry, boot_entry, mounts};
use crate::machine::{Bootloader, Firmware, Machine, OWNER_FILE, Owner, owner_record};

/// Make the running root own boot, so the root that owned it before can be forgotten. With
/// systemd-boot this root's entry becomes the default; with GRUB this root installs its own GRUB
/// and config, listing the previous owner as a fallback entry. `here` is this root's distro, which
/// names its entry, and `arch` is `x86_64` or `aarch64`.
pub fn adopt_boot(paths: &Paths, m: &Machine, owner: Option<&Owner>, here: &str, arch: &str) -> Result<Plan> {
    let root = paths.root.display().to_string();
    let top = mounts(paths, here).top;
    let mut plan = Plan::default();
    plan.run(Cmd::new("mkdir").arg("-p").arg(&top));
    plan.run(Cmd::new("mount").args(["-o", "subvolid=5"]).arg(format!("UUID={}", m.uuid)).arg(&top));
    plan.cleanup.push(Cmd::new("umount").arg(&top));

    match &m.bootloader {
        Bootloader::SystemdBoot { esp, .. } => {
            boot_entry(
                &mut plan,
                paths,
                m,
                &Entry { distro: here, kernels: &root, subvol: &m.root_subvol, cmdline: &m.cmdline },
            );
            let name = format!("fspec-{here}.conf");
            let conf = paths.system(format!("{esp}/loader/loader.conf")).display().to_string();
            plan.run(
                Cmd::new("sh")
                    .args(["-c", r#"touch "$1" && sed -i '/^default[[:space:]]/d' "$1" && echo "default $2" >> "$1""#])
                    .args(["sh", &conf, &name]),
            );
            // An EFI variable set by `bootctl set-default` outranks loader.conf; Void has no bootctl.
            plan.run(
                Cmd::new("sh")
                    .args(["-c", r#"if command -v bootctl >/dev/null; then bootctl set-default "$1"; fi"#])
                    .args(["sh", &name]),
            );
        }
        Bootloader::Grub { mkconfig, cfg } => {
            let install = if *mkconfig == "grub2-mkconfig" { "grub2-install" } else { "grub-install" };
            plan.run(grub_install(paths, m, install, here, arch)?);
            match owner.filter(|o| o.subvol != m.root_subvol) {
                Some(old) => {
                    if old.distro.is_empty() {
                        bail!("could not read ID= from the os-release of {}, the root that owns boot now", old.subvol);
                    }
                    // A separate /boot is shared, so the old root's kernels are already in ours.
                    let kernels = if m.boot_mounts.iter().any(|b| b.target == "/boot") {
                        root.clone()
                    } else {
                        format!("{top}/{}", old.subvol.trim_matches('/'))
                    };
                    // The old root's own options, recorded when it took boot: this root's kernel
                    // was started by an fspec entry and need not have them.
                    let cmdline = if old.cmdline.is_empty() { &m.cmdline } else { &old.cmdline };
                    let entry = Entry { distro: &old.distro, kernels: &kernels, subvol: &old.subvol, cmdline };
                    boot_entry(&mut plan, paths, m, &entry);
                }
                None => plan.run(Cmd::new(*mkconfig).arg("-o").arg(paths.system(cfg).display().to_string())),
            }
        }
    }
    plan.run(Cmd::new("tee").arg(format!("{top}/{OWNER_FILE}")).stdin(owner_record(m)));
    Ok(plan)
}

fn grub_install(paths: &Paths, m: &Machine, install: &str, here: &str, arch: &str) -> Result<Cmd> {
    Ok(match &m.firmware {
        Firmware::Efi => {
            let Some(esp) = m.esp() else {
                bail!("this machine boots through EFI but no ESP is mounted at /boot, /efi or /boot/efi");
            };
            let target = if arch == "aarch64" { "arm64-efi" } else { "x86_64-efi" };
            let cmd = Cmd::new(install)
                .arg(format!("--target={target}"))
                .arg(format!("--efi-directory={}", paths.system(&esp.target).display()))
                .arg(format!("--bootloader-id=fspec-{here}"));
            // Fedora's grub2-install refuses EFI unless forced, since it bypasses shim.
            if install == "grub2-install" { cmd.arg("--force") } else { cmd }
        }
        Firmware::Bios { disk } => {
            if disk.is_empty() {
                bail!("could not find the disk holding the root filesystem, where GRUB's boot code goes");
            }
            Cmd::new(install).arg("--target=i386-pc").arg(disk)
        }
    })
}

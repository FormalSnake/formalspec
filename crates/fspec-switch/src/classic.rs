use std::path::Path;

use anyhow::{Result, bail};
use fspec_core::Paths;
use fspec_core::runner::Cmd;

use crate::machine::{Bootloader, Machine};
use crate::{DISTROS, Plan, subvol};

/// Where fspec stages the kernel and initramfs of a switched-to distro, relative to the boot
/// directory (the ESP, or the host's `/boot` under GRUB).
const STAGE: &str = "fspec";

/// What `switch --distro` builds. The host's `system` already declares `distro`, so the apply
/// inside the new root installs the packages that block names.
pub struct Target<'a> {
    pub distro: &'a str,
    pub host: &'a str,
    /// `x86_64` or `aarch64`.
    pub arch: &'a str,
    /// Fedora release number or Debian suite. Fedora needs it, Debian defaults to `stable`.
    pub release: Option<&'a str>,
    pub spec_dir: &'a Path,
    pub spec_file: &'a str,
    /// The running `fspec`, copied into the new root to run the apply there.
    pub exe: &'a Path,
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

pub(crate) struct Mounts {
    pub top: String,
    pub root: String,
}

pub(crate) fn mounts(paths: &Paths, distro: &str) -> Mounts {
    Mounts {
        top: s(&paths.system("/run/fspec/switch/top")),
        root: s(&paths.system(format!("/run/fspec/switch/{distro}"))),
    }
}

fn stage_dir(paths: &Paths, m: &Machine, distro: &str) -> String {
    let base = match &m.bootloader {
        Bootloader::SystemdBoot { esp } => esp.as_str(),
        Bootloader::Grub { .. } => "/boot",
    };
    s(&paths.system(format!("{base}/{STAGE}/{distro}")))
}

pub(crate) fn grub_script(paths: &Paths, distro: &str) -> String {
    s(&paths.system(format!("/etc/grub.d/42_fspec_{distro}")))
}

fn entry_file(paths: &Paths, esp: &str, distro: &str) -> String {
    s(&paths.system(format!("{esp}/loader/entries/fspec-{distro}.conf")))
}

fn bootstrap(t: &Target, root: &str) -> Result<Vec<Cmd>> {
    let arch = t.arch;
    Ok(match t.distro {
        "fedora" => {
            let Some(rel) = t.release else { bail!("fedora needs --release <number>, the Fedora release to install") };
            vec![
                Cmd::new("dnf")
                    .args(["--installroot", root, "--releasever", rel, "-y", "--setopt=install_weak_deps=False"])
                    .args(["install", "systemd", "dnf", "fedora-release", "passwd", "btrfs-progs", "kernel"]),
            ]
        }
        "arch" => vec![Cmd::new("pacstrap").args(["-K", root, "base", "linux", "linux-firmware", "btrfs-progs"])],
        "debian" => {
            let deb = if arch == "aarch64" { "arm64" } else { "amd64" };
            let include = format!("--include=systemd-sysv,btrfs-progs,linux-image-{deb}");
            vec![Cmd::new("debootstrap").args(["--arch", deb, &include, t.release.unwrap_or("stable"), root])]
        }
        "void" => vec![
            Cmd::new("xbps-install")
                .args(["-S", "-y", "-r", root, "-R", "https://repo-default.voidlinux.org/current"])
                .args(["base-system", "linux", "btrfs-progs"])
                .env("XBPS_ARCH", arch),
        ],
        other => bail!("cannot bootstrap `{other}`; --distro takes one of {}", DISTROS.join(", ")),
    })
}

/// Kernel and initramfs globs inside the new root, newest version wins.
fn kernel_globs(distro: &str) -> (&'static str, &'static str) {
    match distro {
        "arch" => ("boot/vmlinuz-linux", "boot/initramfs-linux.img"),
        "debian" => ("boot/vmlinuz-*", "boot/initrd.img-*"),
        _ => ("boot/vmlinuz-*", "boot/initramfs-*.img"),
    }
}

fn title(distro: &str) -> String {
    let mut c = distro.chars();
    let first = c.next().map(|f| f.to_uppercase().collect::<String>()).unwrap_or_default();
    format!("{first}{} (fspec)", c.as_str())
}

fn boot_entry(plan: &mut Plan, paths: &Paths, m: &Machine, distro: &str, root: &str) {
    let stage = stage_dir(paths, m, distro);
    let (kernel, initrd) = kernel_globs(distro);
    plan.run(Cmd::new("mkdir").arg("-p").arg(&stage));
    for (glob, name) in [(kernel, "vmlinuz"), (initrd, "initrd")] {
        plan.run(
            Cmd::new("sh")
                .args(["-c", r#"cp "$(ls -1 $1 | sort -V | tail -n 1)" "$2""#, "sh"])
                .arg(format!("{root}/{glob}"))
                .arg(format!("{stage}/{name}")),
        );
    }
    let options = format!("root=UUID={} rootflags=subvol={} rw", m.uuid, subvol(distro));
    match &m.bootloader {
        Bootloader::SystemdBoot { esp } => {
            let entry = format!(
                "title {}\nlinux /{STAGE}/{distro}/vmlinuz\ninitrd /{STAGE}/{distro}/initrd\noptions {options}\n",
                title(distro)
            );
            plan.run(Cmd::new("mkdir").arg("-p").arg(s(&paths.system(format!("{esp}/loader/entries")))));
            plan.run(Cmd::new("tee").arg(entry_file(paths, esp, distro)).stdin(entry));
        }
        Bootloader::Grub { mkconfig, cfg } => {
            let prefix = &m.boot_prefix;
            let script = format!(
                "#!/bin/sh\nexec tail -n +3 $0\nmenuentry '{}' {{\n\tinsmod btrfs\n\tsearch --no-floppy --fs-uuid \
                 --set=root {}\n\tlinux {prefix}/boot/{STAGE}/{distro}/vmlinuz {options}\n\tinitrd \
                 {prefix}/boot/{STAGE}/{distro}/initrd\n}}\n",
                title(distro),
                m.uuid
            );
            let path = grub_script(paths, distro);
            plan.run(Cmd::new("tee").arg(&path).stdin(script));
            plan.run(Cmd::new("chmod").arg("755").arg(&path));
            plan.run(Cmd::new(*mkconfig).arg("-o").arg(s(&paths.system(cfg))));
        }
    }
}

/// Install `t.distro` into a new btrfs subvolume next to the running root, apply the spec in it
/// and add a boot entry. The old root, and `/home`, are untouched.
pub fn switch_distro(paths: &Paths, m: &Machine, t: &Target) -> Result<Plan> {
    let mut plan = Plan::default();
    let Mounts { top, root } = mounts(paths, t.distro);
    let (uuid, new) = (format!("UUID={}", m.uuid), subvol(t.distro));
    let bootstrap = bootstrap(t, &root)?;

    plan.run(Cmd::new("mkdir").arg("-p").arg(&top));
    plan.run(Cmd::new("mount").args(["-o", "subvolid=5"]).arg(&uuid).arg(&top));
    plan.cleanup.push(Cmd::new("umount").arg(&top));
    plan.expect(
        Cmd::new("test").arg("-e").arg(format!("{top}/{new}")),
        false,
        format!("the subvolume {new} already exists; run `fspec forget {}` first", t.distro),
    );
    plan.run(Cmd::new("btrfs").args(["subvolume", "create"]).arg(format!("{top}/{new}")));
    plan.run(Cmd::new("mkdir").arg("-p").arg(&root));
    plan.run(Cmd::new("mount").arg("-o").arg(format!("subvol={new}")).arg(&uuid).arg(&root));
    plan.cleanup.insert(0, Cmd::new("umount").arg("-R").arg(&root));

    bootstrap.into_iter().for_each(|c| plan.run(c));
    plan.run(Cmd::new("cp").arg("-L").arg(s(&paths.system("/etc/resolv.conf"))).arg(format!("{root}/etc/resolv.conf")));
    let home = m.home_subvol.trim_start_matches('/');
    let fstab =
        format!("{uuid} / btrfs subvol={new},compress=zstd 0 0\n{uuid} /home btrfs subvol={home},compress=zstd 0 0\n");
    plan.run(Cmd::new("tee").arg(format!("{root}/etc/fstab")).stdin(fstab));

    for (src, dst) in [("/dev", "dev"), ("/sys", "sys")] {
        let dst = format!("{root}/{dst}");
        plan.run(Cmd::new("mount").arg("--rbind").arg(s(&paths.system(src))).arg(&dst));
        plan.run(Cmd::new("mount").arg("--make-rslave").arg(&dst));
    }
    plan.run(Cmd::new("mount").args(["-t", "proc", "proc"]).arg(format!("{root}/proc")));

    let spec_copy = format!("{root}/var/lib/fspec/spec");
    plan.run(Cmd::new("mkdir").arg("-p").arg(&spec_copy).arg(format!("{root}/usr/local/bin")));
    plan.run(Cmd::new("cp").arg(s(t.exe)).arg(format!("{root}/usr/local/bin/fspec")));
    plan.run(Cmd::new("cp").arg("-a").arg(format!("{}/.", s(t.spec_dir))).arg(&spec_copy));
    plan.run(
        Cmd::new("chroot")
            .arg(&root)
            .args(["/usr/local/bin/fspec", "--spec"])
            .arg(format!("/var/lib/fspec/spec/{}", t.spec_file))
            .args(["apply", "--local", "--scope", "system", "--host", t.host])
            .env("HOME", "/root"),
    );

    boot_entry(&mut plan, paths, m, t.distro, &root);
    Ok(plan)
}

/// Commands that undo [`boot_entry`] for `distro`.
pub(crate) fn remove_boot_entry(plan: &mut Plan, paths: &Paths, m: &Machine, distro: &str) {
    plan.run(Cmd::new("rm").arg("-rf").arg(stage_dir(paths, m, distro)));
    match &m.bootloader {
        Bootloader::SystemdBoot { esp } => {
            plan.run(Cmd::new("rm").arg("-f").arg(entry_file(paths, esp, distro)));
        }
        Bootloader::Grub { mkconfig, cfg } => {
            plan.run(Cmd::new("rm").arg("-f").arg(grub_script(paths, distro)));
            plan.run(Cmd::new(*mkconfig).arg("-o").arg(s(&paths.system(cfg))));
        }
    }
}

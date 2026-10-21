use std::path::Path;

use anyhow::{Result, bail};
use fspec_core::Paths;
use fspec_core::runner::Cmd;

use crate::machine::{Bootloader, Firmware, Machine, OWNER_FILE, Owner, owner_record, subvol_arg};
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
        Bootloader::SystemdBoot { esp, .. } => esp.as_str(),
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

/// The packages that let a root run `grub-install` and `grub-mkconfig` for `switch --adopt-boot`.
fn grub_packages(distro: &str, arch: &str, firmware: &Firmware) -> Vec<String> {
    let efi = matches!(firmware, Firmware::Efi);
    let arm = arch == "aarch64";
    let names: Vec<&str> = match (distro, efi) {
        ("arch", true) => vec!["grub", "efibootmgr"],
        ("arch", false) => vec!["grub"],
        ("debian", true) => vec![if arm { "grub-efi-arm64" } else { "grub-efi-amd64" }],
        ("debian", false) => vec!["grub-pc"],
        ("fedora", true) => vec!["grub2-tools", if arm { "grub2-efi-aa64" } else { "grub2-efi-x64" }],
        ("fedora", false) => vec!["grub2-tools", "grub2-pc"],
        ("void", true) => vec![if arm { "grub-arm64-efi" } else { "grub-x86_64-efi" }],
        _ => vec!["grub"],
    };
    names.into_iter().map(String::from).collect()
}

fn bootstrap(t: &Target, m: &Machine, root: &str) -> Result<Vec<Cmd>> {
    let arch = t.arch;
    let grub = match m.bootloader {
        Bootloader::Grub { .. } => grub_packages(t.distro, arch, &m.firmware),
        Bootloader::SystemdBoot { .. } => Vec::new(),
    };
    Ok(match t.distro {
        "fedora" => {
            let Some(rel) = t.release else { bail!("fedora needs --release <number>, the Fedora release to install") };
            // dnf reads repositories from the installroot, which has none yet, so they are named here.
            // Arch's dnf is dnf 4 and Fedora's is dnf5; both take these options.
            let mirror = "https://download.fedoraproject.org/pub/fedora/linux";
            let key = format!(
                "https://src.fedoraproject.org/rpms/fedora-repos/raw/rawhide/f/RPM-GPG-KEY-fedora-{rel}-primary"
            );
            let mut dnf = Cmd::new("dnf").args([
                "--installroot",
                root,
                "--releasever",
                rel,
                "-y",
                "--setopt=install_weak_deps=False",
            ]);
            for (id, path) in [
                ("fedora", format!("releases/{rel}/Everything/{arch}/os")),
                ("updates", format!("updates/{rel}/Everything/{arch}")),
            ] {
                dnf = dnf
                    .arg(format!("--repofrompath={id},{mirror}/{path}"))
                    .arg(format!("--setopt={id}.gpgcheck=1"))
                    .arg(format!("--setopt={id}.gpgkey={key}"));
            }
            vec![
                dnf.args([
                    "install",
                    "systemd",
                    "dnf",
                    "fedora-release",
                    "fedora-repos",
                    "passwd",
                    "btrfs-progs",
                    "kernel",
                ])
                .args(&grub),
            ]
        }
        "arch" => {
            vec![Cmd::new("pacstrap").args(["-K", root, "base", "linux", "linux-firmware", "btrfs-progs"]).args(&grub)]
        }
        "debian" => {
            let deb = if arch == "aarch64" { "arm64" } else { "amd64" };
            let mut include = format!("--include=systemd-sysv,btrfs-progs,linux-image-{deb}");
            grub.iter().for_each(|g| include.push_str(&format!(",{g}")));
            vec![Cmd::new("debootstrap").args(["--arch", deb, &include, t.release.unwrap_or("stable"), root])]
        }
        "void" => vec![
            Cmd::new("xbps-install")
                .args(["-S", "-y", "-r", root, "-R", "https://repo-default.voidlinux.org/current"])
                .args(["base-system", "linux", "btrfs-progs"])
                .args(&grub)
                .env("XBPS_ARCH", arch)
                // A fresh root has no repository key yet, and `-y` does not answer the import prompt.
                .stdin("y\n"),
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

/// The boot menu name of `distro`'s entry, e.g. `Void (fspec)`.
pub fn title(distro: &str) -> String {
    let mut c = distro.chars();
    let first = c.next().map(|f| f.to_uppercase().collect::<String>()).unwrap_or_default();
    format!("{first}{} (fspec)", c.as_str())
}

/// A root to boot: `distro` names the entry and the kernel globs, `kernels` is the directory the
/// kernels are copied from, `subvol` the root subvolume as findmnt prints it, and `cmdline` the
/// kernel command line whose options the entry keeps.
pub(crate) struct Entry<'a> {
    pub distro: &'a str,
    pub kernels: &'a str,
    pub subvol: &'a str,
    pub cmdline: &'a str,
}

/// `cmdline` with its root swapped for `subvol`. Mount options in its `rootflags=`, like
/// `compress=zstd:1`, stay.
fn kernel_options(uuid: &str, cmdline: &str, subvol: &str) -> String {
    let mut flags = vec![format!("subvol={}", subvol_arg(subvol))];
    let mut rest = Vec::new();
    for arg in cmdline.split_whitespace() {
        let key = arg.split('=').next().unwrap_or(arg);
        match key {
            "BOOT_IMAGE" | "root" | "initrd" | "ro" | "rw" => {}
            "rootflags" => flags.extend(
                arg["rootflags=".len()..]
                    .split(',')
                    .filter(|f| !f.is_empty() && !f.starts_with("subvol=") && !f.starts_with("subvolid="))
                    .map(String::from),
            ),
            _ => rest.push(arg),
        }
    }
    let mut options = format!("root=UUID={uuid} rootflags={} rw", flags.join(","));
    for arg in rest {
        options.push(' ');
        options.push_str(arg);
    }
    options
}

pub(crate) fn boot_entry(plan: &mut Plan, paths: &Paths, m: &Machine, e: &Entry) {
    let stage = stage_dir(paths, m, e.distro);
    let (kernel, initrd) = kernel_globs(e.distro);
    plan.run(Cmd::new("mkdir").arg("-p").arg(&stage));
    for (glob, name) in [(kernel, "vmlinuz"), (initrd, "initrd")] {
        plan.run(
            Cmd::new("sh")
                .args(["-c", r#"cp "$(ls -1 $1 | sort -V | tail -n 1)" "$2""#, "sh"])
                .arg(format!("{}/{glob}", e.kernels.trim_end_matches('/')))
                .arg(format!("{stage}/{name}")),
        );
    }
    let options = kernel_options(&m.uuid, e.cmdline, e.subvol);
    let distro = e.distro;
    match &m.bootloader {
        Bootloader::SystemdBoot { esp, .. } => {
            let entry = format!(
                "title {}\nlinux /{STAGE}/{distro}/vmlinuz\ninitrd /{STAGE}/{distro}/initrd\noptions {options}\n",
                title(distro)
            );
            plan.run(Cmd::new("mkdir").arg("-p").arg(s(&paths.system(format!("{esp}/loader/entries")))));
            plan.run(Cmd::new("tee").arg(entry_file(paths, esp, distro)).stdin(entry));
        }
        Bootloader::Grub { mkconfig, cfg } => {
            let (uuid, prefix) = m.grub_boot();
            let script = format!(
                "#!/bin/sh\nexec tail -n +3 $0\nmenuentry '{}' {{\n\tinsmod btrfs\n\tsearch --no-floppy --fs-uuid \
                 --set=root {uuid}\n\tlinux {prefix}/{STAGE}/{distro}/vmlinuz {options}\n\tinitrd \
                 {prefix}/{STAGE}/{distro}/initrd\n}}\n",
                title(distro),
            );
            let path = grub_script(paths, distro);
            plan.run(Cmd::new("tee").arg(&path).stdin(script));
            plan.run(Cmd::new("chmod").arg("755").arg(&path));
            plan.run(Cmd::new(*mkconfig).arg("-o").arg(s(&paths.system(cfg))));
        }
    }
}

/// Refuse to touch boot from a root that does not own it: its GRUB config is not the one the
/// firmware reads, and changing the owner's would need its files.
pub(crate) fn require_owner(m: &Machine, owner: Option<&Owner>) -> Result<()> {
    match owner {
        Some(o) if o.subvol == m.root_subvol => Ok(()),
        Some(o) => bail!(
            "boot is owned by the root {}, not this one ({}); run `fspec switch --adopt-boot` from {} first",
            o.subvol,
            m.root_subvol,
            m.root_subvol
        ),
        None => {
            bail!("no root is recorded as owning boot; run `fspec switch --adopt-boot` from {} first", m.root_subvol)
        }
    }
}

fn fstab(m: &Machine, new: &str) -> String {
    let uuid = &m.uuid;
    let home = m.home_subvol.trim_start_matches('/');
    let mut text = format!(
        "UUID={uuid} / btrfs subvol={new},compress=zstd 0 0\nUUID={uuid} /home btrfs subvol={home},compress=zstd 0 0\n"
    );
    for b in &m.boot_mounts {
        let opts = if b.fstype == "vfat" { "umask=0077" } else { "defaults" };
        text.push_str(&format!("UUID={} {} {} {opts} 0 2\n", b.uuid, b.target, b.fstype));
    }
    text
}

/// Install `t.distro` into a new btrfs subvolume next to the running root, apply the spec in it
/// and add a boot entry. The old root, and `/home`, are untouched.
pub fn switch_distro(paths: &Paths, m: &Machine, owner: Option<&Owner>, t: &Target) -> Result<Plan> {
    if owner.is_some() && matches!(m.bootloader, Bootloader::Grub { .. }) {
        require_owner(m, owner)?;
    }
    let mut plan = Plan::default();
    let Mounts { top, root } = mounts(paths, t.distro);
    let (uuid, new) = (format!("UUID={}", m.uuid), subvol(t.distro));
    let bootstrap = bootstrap(t, m, &root)?;

    plan.run(Cmd::new("mkdir").arg("-p").arg(&top));
    plan.run(Cmd::new("mount").args(["-o", "subvolid=5"]).arg(&uuid).arg(&top));
    plan.cleanup.push(Cmd::new("umount").arg(&top));
    plan.expect(
        Cmd::new("test").arg("-e").arg(format!("{top}/{new}")),
        false,
        format!("the subvolume {new} already exists; run `fspec forget {}` first", t.distro),
    );
    if owner.is_none() {
        plan.run(Cmd::new("tee").arg(format!("{top}/{OWNER_FILE}")).stdin(owner_record(m)));
    }
    plan.run(Cmd::new("btrfs").args(["subvolume", "create"]).arg(format!("{top}/{new}")));
    plan.run(Cmd::new("mkdir").arg("-p").arg(&root));
    plan.run(Cmd::new("mount").arg("-o").arg(format!("subvol={new}")).arg(&uuid).arg(&root));
    plan.cleanup.insert(0, Cmd::new("umount").arg("-R").arg(&root));

    bootstrap.into_iter().for_each(|c| plan.run(c));
    plan.run(Cmd::new("cp").arg("-L").arg(s(&paths.system("/etc/resolv.conf"))).arg(format!("{root}/etc/resolv.conf")));
    plan.run(Cmd::new("tee").arg(format!("{root}/etc/fstab")).stdin(fstab(m, &new)));

    // The shared home goes in before the apply, so a declared user's home lands in it.
    for (src, dst) in [("/home", "home"), ("/dev", "dev"), ("/sys", "sys")] {
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
            .env("HOME", "/root")
            // The running distro's PATH can miss the new root's tool dirs: Arch has no /usr/sbin.
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"),
    );

    let subvol = format!("/{new}");
    boot_entry(&mut plan, paths, m, &Entry { distro: t.distro, kernels: &root, subvol: &subvol, cmdline: &m.cmdline });
    Ok(plan)
}

/// Commands that undo [`boot_entry`] for `distro`.
pub(crate) fn remove_boot_entry(plan: &mut Plan, paths: &Paths, m: &Machine, distro: &str) {
    plan.run(Cmd::new("rm").arg("-rf").arg(stage_dir(paths, m, distro)));
    match &m.bootloader {
        Bootloader::SystemdBoot { esp, .. } => {
            plan.run(Cmd::new("rm").arg("-f").arg(entry_file(paths, esp, distro)));
        }
        Bootloader::Grub { mkconfig, cfg } => {
            plan.run(Cmd::new("rm").arg("-f").arg(grub_script(paths, distro)));
            plan.run(Cmd::new(*mkconfig).arg("-o").arg(s(&paths.system(cfg))));
        }
    }
}

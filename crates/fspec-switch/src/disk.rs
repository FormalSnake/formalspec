//! `fspec disk`: put a disk onto one of the layouts `switch --distro` installs roots on, from
//! scratch (`init`) or in place (`migrate`).

use std::path::Path;

use anyhow::{Context, Result, bail};
use fspec_core::resource::System;
use fspec_core::runner::{Cmd, Runner};
use fspec_core::{Host, Paths};

use crate::classic::{Sources, Target, create_root, fstab, grub_menu, install_root, kernel_options, s, sd_default};
use crate::machine::{Btrfs, ESP_TYPE, Firmware, GIB, Layout, MIB, Machine, Part, SHARED_BOOT, SHARED_SUBVOLS, probe};
use crate::recipe::{Boot, boot_files, populate, recipe};
use crate::{Plan, subvol};

/// The ESP size the default layout gives a disk.
pub const ESP_SIZE: u64 = 2 * GIB;
/// The shared boot partition of the BIOS layout.
pub const BOOT_SIZE: u64 = 2 * GIB;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Default,
    Zfs,
    Bios,
}

/// Identifiers chosen before formatting, so the whole plan, `fstab` included, is known up front.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ids {
    /// The btrfs filesystem UUID.
    pub fs: String,
    pub luks: String,
    /// FAT volume ID, 8 hex digits.
    pub serial: String,
    /// The ext4 UUID of the BIOS layout's boot partition.
    pub boot: String,
}

impl Ids {
    /// The FAT volume ID as blkid and `fstab` spell it.
    pub fn esp_uuid(&self) -> String {
        format!("{}-{}", &self.serial[..4], &self.serial[4..])
    }
}

/// `/dev/sda` partition 2 is `/dev/sda2`, `/dev/nvme0n1` partition 2 is `/dev/nvme0n1p2`.
pub fn partition(disk: &str, n: u64) -> String {
    if disk.ends_with(|c: char| c.is_ascii_digit()) { format!("{disk}p{n}") } else { format!("{disk}{n}") }
}

pub struct Init<'a> {
    pub device: &'a str,
    pub kind: Kind,
    pub luks: bool,
    pub ids: &'a Ids,
    pub swap_gib: u64,
    /// The running system booted through EFI, so bootctl and efibootmgr can write boot entries.
    pub efi_vars: bool,
    /// The first root to install, as `switch --distro` would.
    pub target: Option<&'a Target<'a>>,
    /// Kernel options to carry over, like `console=`.
    pub cmdline: &'a str,
    /// Run after the target's root is installed and before anything is unmounted, with the root
    /// mounted at [`root_dir`]. Ignored without a target.
    pub in_root: &'a [Cmd],
    /// The LUKS passphrase, passed on stdin. Without one, cryptsetup asks on the terminal.
    pub passphrase: Option<&'a str>,
}

/// Where `init` mounts the root of `distro` while it installs it.
pub fn root_dir(paths: &Paths, distro: &str) -> String {
    s(&paths.system(format!("{DISK}/{distro}")))
}

/// The new root `switch --distro` or `disk init --distro` builds from the host's declaration.
pub fn target<'a>(
    host: &'a Host,
    distro: &'a str,
    release: Option<&'a str>,
    spec_dir: &'a Path,
    spec_file: &'a str,
    exe: &'a Path,
) -> Result<Target<'a>> {
    match &host.system {
        Some(System::Distro(d)) if d.distro == distro => {}
        _ => bail!(
            "host {} does not declare {distro} as its system. Edit `system =` to {distro}(...) first: the new root \
             is built from that block",
            host.name
        ),
    }
    let arch = host.platform.split('-').next().unwrap_or_default();
    Ok(Target { distro, host: &host.name, arch, release, spec_dir, spec_file, exe })
}

/// A random UUID from the kernel.
fn new_uuid() -> Result<String> {
    Ok(std::fs::read_to_string("/proc/sys/kernel/random/uuid").context("reading a random UUID")?.trim().to_string())
}

/// Fresh identifiers for the filesystems `init` creates.
pub fn new_ids() -> Result<Ids> {
    let serial = new_uuid()?.replace('-', "")[..8].to_uppercase();
    Ok(Ids { fs: new_uuid()?, luks: new_uuid()?, serial, boot: new_uuid()? })
}

/// Check that `device` is a whole disk with nothing mounted and room for a layout, and answer
/// whether it holds anything at all.
pub fn blank_disk(runner: &dyn Runner, device: &str) -> Result<bool> {
    let out =
        runner.check(&Cmd::new("lsblk").args(["-J", "-b", "-p", "-o", "NAME,TYPE,FSTYPE,SIZE,MOUNTPOINTS", device]))?;
    let json: serde_json::Value = serde_json::from_str(&out.stdout)?;
    let disk = &json["blockdevices"][0];
    if disk["type"].as_str() != Some("disk") {
        bail!("{device} is not a whole disk");
    }
    fn mounted(v: &serde_json::Value) -> bool {
        v["mountpoints"].as_array().is_some_and(|m| m.iter().any(|x| x.is_string()))
            || v["children"].as_array().is_some_and(|c| c.iter().any(mounted))
    }
    if mounted(disk) {
        bail!("{device} has mounted filesystems; unmount them first");
    }
    if disk["size"].as_u64().unwrap_or_default() < 16 << 30 {
        bail!("{device} is smaller than 16 GiB, too small for a 2 GiB ESP and a root");
    }
    Ok(disk["children"].as_array().is_none_or(|c| c.is_empty()) && disk["fstype"].is_null())
}

const DISK: &str = "/run/fspec/disk";

/// Partition and format `i.device` from scratch with the layout `i.kind`, create the shared
/// subvolumes or datasets, install the bootloader, and with a target install its root.
pub fn init(paths: &Paths, i: &Init) -> Result<Plan> {
    if i.luks && i.kind == Kind::Zfs {
        bail!("--luks does not apply to the ZFS layout; ZFS encrypts datasets itself");
    }
    let dir = |x: &str| s(&paths.system(format!("{DISK}/{x}")));
    let (top, efi, boot, zfs) = (dir("top"), dir("efi"), dir("boot"), dir("zfs"));
    let dev = i.device;
    let ids = i.ids;
    let mut plan = Plan::default();
    let mut cleanup = Vec::new();

    plan.run(Cmd::new("wipefs").args(["-a", dev]));
    plan.run(Cmd::new("sgdisk").args(["--zap-all", dev]));
    let (data, layout) = match i.kind {
        Kind::Default | Kind::Zfs => {
            let code = if i.kind == Kind::Zfs {
                "bf00"
            } else if i.luks {
                "8309"
            } else {
                "8300"
            };
            plan.run(
                Cmd::new("sgdisk")
                    .args(["-n1:0:+2G", "-t1:ef00", "-c1:EFI", "-n2:0:0"])
                    .arg(format!("-t2:{code}"))
                    .args(["-c2:root", dev]),
            );
            (partition(dev, 2), i.kind)
        }
        Kind::Bios => {
            let code = if i.luks { "8309" } else { "8300" };
            plan.run(
                Cmd::new("sgdisk")
                    .args(["-n1:0:+1M", "-t1:ef02", "-c1:bios", "-n2:0:+2G", "-t2:8300", "-c2:boot", "-n3:0:0"])
                    .arg(format!("-t3:{code}"))
                    .args(["-c3:root", dev]),
            );
            (partition(dev, 3), i.kind)
        }
    };
    plan.run(Cmd::new("udevadm").arg("settle"));
    // The new partitions start where the old ones did, so their old signatures are still there.
    let parts = if i.kind == Kind::Bios { 3 } else { 2 };
    plan.run(Cmd::new("wipefs").arg("-a").args((1..=parts).map(|n| partition(dev, n))));

    let esp_part = Part { uuid: ids.esp_uuid(), fstype: "vfat".into(), size: ESP_SIZE, ..Part::default() };
    let boot_part = Part { uuid: ids.boot.clone(), fstype: "ext4".into(), size: BOOT_SIZE, ..Part::default() };
    if layout != Kind::Bios {
        plan.run(Cmd::new("mkfs.vfat").args(["-F", "32", "-n", "EFI", "-i", &ids.serial]).arg(partition(dev, 1)));
        plan.run(Cmd::new("mkdir").arg("-p").arg(&efi));
        plan.run(Cmd::new("mount").arg(partition(dev, 1)).arg(&efi));
        cleanup.push(Cmd::new("umount").arg(&efi));
    }

    let layout = match layout {
        Kind::Zfs => {
            plan.expect(
                Cmd::new("zpool").args(["list", "-H", "rpool"]),
                false,
                "a pool named rpool is already imported; export it first",
            );
            // The pool records the creating host's ID, which every root's initramfs must match.
            plan.run(Cmd::new("sh").args(["-c", "test -e /etc/hostid || zgenhostid"]));
            plan.run(
                Cmd::new("zpool")
                    .args(["create", "-f", "-o", "ashift=12", "-O", "compression=zstd", "-O", "acltype=posixacl"])
                    .args(["-O", "xattr=sa", "-O", "relatime=on", "-O", "mountpoint=none", "-R", &zfs, "rpool", &data]),
            );
            cleanup.push(Cmd::new("zpool").args(["export", "rpool"]));
            plan.run(Cmd::new("zfs").args(["create", "-o", "mountpoint=none", "rpool/ROOT"]));
            plan.run(Cmd::new("zfs").args(["create", "-o", "mountpoint=/home", "rpool/home"]));
            plan.run(Cmd::new("zfs").args(["create", "-o", "mountpoint=/opt/fspec", "rpool/fspec"]));
            let zbm = format!("{efi}/EFI/zbm");
            plan.run(Cmd::new("mkdir").arg("-p").arg(&zbm).arg(format!("{efi}/EFI/BOOT")));
            plan.run(
                Cmd::new("curl")
                    .args(["-fsSL", "-o"])
                    .arg(format!("{zbm}/zfsbootmenu.efi"))
                    .arg("https://get.zfsbootmenu.org/efi"),
            );
            // The removable path boots it even where no firmware entry could be written.
            plan.run(Cmd::new("cp").arg(format!("{zbm}/zfsbootmenu.efi")).arg(format!("{efi}/EFI/BOOT/BOOTX64.EFI")));
            if i.efi_vars {
                plan.run(
                    Cmd::new("efibootmgr")
                        .args(["--create", "--disk", dev, "--part", "1", "--label", "ZFSBootMenu", "--loader"])
                        .arg("\\EFI\\zbm\\zfsbootmenu.efi"),
                );
            }
            Layout::Zfs { esp: esp_part, pool: "rpool".into() }
        }
        kind => {
            let mut fsdev = data.clone();
            let luks = i.luks.then(|| ids.luks.clone());
            if let Some(luks) = &luks {
                let name = format!("luks-{luks}");
                let format = Cmd::new("cryptsetup").args(["luksFormat", "--type", "luks2", "--uuid", luks]);
                let open = Cmd::new("cryptsetup").arg("open");
                // With --key-file=- cryptsetup takes all of stdin as the key, so both commands
                // get exactly the same bytes.
                let (format, open) = match i.passphrase {
                    Some(p) => {
                        (format.args(["--batch-mode", "--key-file=-"]).stdin(p), open.arg("--key-file=-").stdin(p))
                    }
                    None => (format.interactive(), open.interactive()),
                };
                plan.run(format.arg(&data));
                plan.run(open.args([&data, &name]));
                fsdev = format!("/dev/mapper/{name}");
            }
            if kind == Kind::Bios {
                plan.run(Cmd::new("mkfs.ext4").args(["-F", "-L", "boot", "-U", &ids.boot]).arg(partition(dev, 2)));
            }
            plan.run(Cmd::new("mkfs.btrfs").args(["-f", "-L", "root", "-U", &ids.fs, &fsdev]));
            plan.run(Cmd::new("mkdir").arg("-p").arg(&top));
            plan.run(Cmd::new("mount").arg(&fsdev).arg(&top));
            cleanup.push(Cmd::new("umount").arg(&top));
            if let Some(luks) = &luks {
                // Services socket-activated during the install can hold the filesystem in their own
                // mount namespaces for a while, so the mapping goes when the last of them lets go.
                cleanup.push(Cmd::new("cryptsetup").args(["close", "--deferred", &format!("luks-{luks}")]));
            }
            for v in std::iter::once("@home").chain(SHARED_SUBVOLS) {
                plan.run(Cmd::new("btrfs").args(["subvolume", "create"]).arg(format!("{top}/{v}")));
            }
            plan.run(swapfile(&top, i.swap_gib));
            let btrfs = Btrfs { uuid: ids.fs.clone(), luks, home: "/@home".into() };
            match kind {
                Kind::Bios => {
                    plan.run(Cmd::new("mkdir").arg("-p").arg(&boot));
                    plan.run(Cmd::new("mount").arg(partition(dev, 2)).arg(&boot));
                    cleanup.insert(0, Cmd::new("umount").arg(&boot));
                    plan.run(
                        Cmd::new("grub-install")
                            .arg("--target=i386-pc")
                            .arg(format!("--boot-directory={boot}"))
                            .arg(dev),
                    );
                    Layout::Bios { boot: boot_part, btrfs }
                }
                _ => {
                    let mut bootctl = Cmd::new("bootctl").arg(format!("--esp-path={efi}")).arg("install");
                    if !i.efi_vars {
                        bootctl = bootctl.arg("--no-variables");
                    }
                    plan.run(bootctl);
                    Layout::Default { esp: esp_part, btrfs }
                }
            }
        }
    };

    if let Some(t) = i.target {
        let firmware = if i.kind == Kind::Bios { Firmware::Bios } else { Firmware::Efi };
        if let Some(why) = crate::recipe::misfit(recipe(t.distro)?, &layout, &firmware) {
            bail!("cannot install {} on this layout: {why}", t.distro);
        }
        let root = root_dir(paths, t.distro);
        let mut roots = Plan::default();
        create_root(&mut roots, &layout, t.distro, &top, &root);
        let src = Sources {
            home: if i.kind == Kind::Zfs { format!("{zfs}/home") } else { format!("{top}/@home") },
            fspec: if i.kind == Kind::Zfs { format!("{zfs}/opt/fspec") } else { format!("{top}/@fspec") },
            esp: efi.clone(),
            shared_boot: boot.clone(),
            ..Sources::running(paths)
        };
        install_root(&mut roots, &layout, t, &root, &src, i.cmdline)?;
        plan.steps.extend(roots.steps);
        i.in_root.iter().for_each(|c| plan.run(c.clone()));
        cleanup.splice(0..0, roots.cleanup);
        match &layout {
            Layout::Default { .. } => sd_default(&efi, t.distro).into_iter().for_each(|c| plan.run(c)),
            Layout::Zfs { pool, .. } => {
                plan.run(Cmd::new("zpool").arg("set").arg(format!("bootfs={pool}/ROOT/{}", t.distro)).arg(pool))
            }
            Layout::Bios { boot: b, .. } => plan.run(grub_menu(&boot, &b.uuid, t.distro)),
            Layout::Unsupported { .. } => {}
        }
    }
    plan.cleanup = cleanup;
    Ok(plan.private(&s(&paths.system(DISK))))
}

fn swapfile(top: &str, gib: u64) -> Cmd {
    Cmd::new("btrfs")
        .args(["filesystem", "mkswapfile", "--size"])
        .arg(format!("{gib}g"))
        .arg(format!("{top}/@swap/swapfile"))
}

/// One step of `fspec disk migrate`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub what: String,
    /// Asks for the device name and a recent backup before it runs.
    pub destructive: bool,
    pub plan: Plan,
}

/// What `disk migrate` reads beyond [`Machine`]: leftovers it can only see on the btrfs top level.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Leftovers {
    /// The running root moved into `@<distro>` and the old one is still there: `""` for the top
    /// level itself, or the old subvolume's name.
    pub old_root: Option<String>,
    /// `/home` and `/opt/fspec` were copied into their subvolumes and the originals still sit
    /// under the mounts in this root.
    pub shadowed: Vec<String>,
}

/// Read the [`Leftovers`] by mounting the btrfs top level read-only.
pub fn leftovers(runner: &dyn Runner, paths: &Paths, m: &Machine) -> Result<Leftovers> {
    if m.fstype != "btrfs" || m.root != format!("/@{}", m.running) {
        return Ok(Leftovers::default());
    }
    let top = s(&paths.system("/run/fspec/disk/look"));
    runner.check(&Cmd::new("mkdir").arg("-p").arg(&top))?;
    runner.check(&Cmd::new("mount").args(["-o", "ro,subvolid=5"]).arg(format!("UUID={}", m.fs_uuid)).arg(&top))?;
    let read = || -> Result<Leftovers> {
        let mut found = Leftovers::default();
        let has = |p: &str| -> Result<bool> {
            Ok(probe(runner, Cmd::new("test").arg("-e").arg(format!("{top}/{p}")))?.is_some())
        };
        if has("etc/os-release")? {
            found.old_root = Some(String::new());
        } else {
            for v in &m.facts.volumes {
                if !v.starts_with('@') && has(&format!("{v}/etc/os-release"))? {
                    found.old_root = Some(v.clone());
                }
            }
        }
        for dir in ["home", "opt/fspec"] {
            let listing = probe(runner, Cmd::new("ls").arg("-A").arg(format!("{top}/@{}/{dir}", m.running)))?;
            if listing.is_some_and(|l| !l.is_empty()) {
                found.shadowed.push(format!("/{dir}"));
            }
        }
        Ok(found)
    };
    let found = read();
    runner.run(&Cmd::new("umount").arg(&top))?;
    found
}

/// The steps that bring the running system onto a supported layout, in order. `ids` names the
/// filesystems a step creates.
pub fn migrate(paths: &Paths, m: &Machine, left: &Leftovers, ids: &Ids, swap_gib: u64) -> Result<Vec<Step>> {
    match m.fstype.as_str() {
        "btrfs" => {}
        "ext4" => bail!(
            "the root filesystem is ext4, which converts to btrfs only while unmounted: boot a live system and run \
             `fspec disk migrate {}`",
            m.root_part.dev
        ),
        "zfs" => match &m.layout {
            Layout::Unsupported { reasons } => {
                bail!("fspec does not migrate within ZFS; fix these by hand: {}", reasons.join("; "))
            }
            _ => return Ok(Vec::new()),
        },
        other => bail!("the root filesystem is {other}; only ext4 converts to btrfs in place"),
    }
    let d = m.running.as_str();
    if d.is_empty() {
        bail!("could not read ID= from /etc/os-release, which names this root's subvolume");
    }
    recipe(d)?;
    let top = s(&paths.system("/run/fspec/disk/top"));
    let mount_top = |plan: &mut Plan| {
        plan.run(Cmd::new("mkdir").arg("-p").arg(&top));
        plan.run(Cmd::new("mount").args(["-o", "subvolid=5"]).arg(format!("UUID={}", m.fs_uuid)).arg(&top));
        plan.cleanup.push(Cmd::new("umount").arg(&top));
    };
    let mut steps = Vec::new();

    // The shared subvolumes, with copies of what this root holds at their mount points. A
    // reflink copy shares the blocks, so it is quick and takes no room.
    let f = &m.facts;
    let home_subvol = !f.home.is_empty() && f.home != m.root;
    let missing: Vec<&str> = SHARED_SUBVOLS.iter().copied().filter(|v| !f.volumes.iter().any(|x| x == v)).collect();
    if !home_subvol || !missing.is_empty() {
        let mut plan = Plan::default();
        mount_top(&mut plan);
        if !home_subvol {
            plan.run(Cmd::new("btrfs").args(["subvolume", "create"]).arg(format!("{top}/@home")));
            plan.run(reflink_copy(&s(&paths.system("/home")), &format!("{top}/@home")));
        }
        for v in &missing {
            plan.run(Cmd::new("btrfs").args(["subvolume", "create"]).arg(format!("{top}/{v}")));
        }
        if missing.contains(&"@swap") {
            plan.run(swapfile(&top, swap_gib));
        }
        if missing.contains(&"@fspec") {
            plan.run(reflink_copy(&s(&paths.system("/opt/fspec")), &format!("{top}/@fspec")));
        }
        let what = format!(
            "create the shared subvolumes{}{}",
            if missing.is_empty() { String::new() } else { format!(" {}", missing.join(", ")) },
            if home_subvol { "" } else { " and @home, copying /home into it" }
        );
        steps.push(Step { what, destructive: false, plan });
    }
    let home = if home_subvol { f.home.clone() } else { "/@home".into() };
    let btrfs = Btrfs { uuid: m.fs_uuid.clone(), luks: m.luks.clone(), home };

    // The boot partition the layout needs, carved from the end of the btrfs partition when the
    // disk has none big enough.
    let target = match m.firmware {
        Firmware::Efi => {
            let esp = m.parts.iter().filter(|p| p.parttype == ESP_TYPE).max_by_key(|p| p.size);
            // A 2 GiB ESP an earlier run created but did not finish has no filesystem yet.
            let made = esp.filter(|e| e.size >= ESP_SIZE && e.fstype.is_empty() && e.label == "EFI");
            let new = match esp {
                Some(e) if e.size >= ESP_SIZE && made.is_none() => {
                    if !e.mounts.iter().any(|x| x == "/efi") {
                        let mut plan = Plan::default();
                        let efi = s(&paths.system("/efi"));
                        for old in &e.mounts {
                            plan.run(Cmd::new("umount").arg(s(&paths.system(old))));
                        }
                        plan.run(Cmd::new("mkdir").arg("-p").arg(&efi));
                        plan.run(Cmd::new("mount").arg(&e.dev).arg(&efi));
                        steps.push(Step { what: format!("mount the ESP {} at /efi", e.dev), destructive: false, plan });
                    }
                    e.clone()
                }
                _ => {
                    let old = m
                        .parts
                        .iter()
                        .filter(|p| p.parttype == ESP_TYPE && !p.fstype.is_empty())
                        .max_by_key(|p| p.size);
                    let (step, part) = carve_esp(paths, m, old, made, ids)?;
                    steps.push(step);
                    part
                }
            };
            Layout::Default { esp: new, btrfs }
        }
        Firmware::Bios => {
            let gpt_without_grub = m.parts.iter().any(|p| !p.partuuid.is_empty())
                && !m.parts.iter().any(|p| p.parttype == crate::machine::BIOS_GRUB_TYPE);
            if gpt_without_grub {
                bail!(
                    "the GPT disk has no bios_grub partition, which GRUB needs to boot through BIOS; fspec does not add one"
                );
            }
            let boot = match m.parts.iter().find(|p| p.mounts.iter().any(|x| x == SHARED_BOOT)) {
                Some(b) => b.clone(),
                None => {
                    let made = m.parts.iter().find(|p| p.label == "boot" && p.fstype.is_empty());
                    let (step, part) = carve_boot(paths, m, made, ids)?;
                    steps.push(step);
                    part
                }
            };
            Layout::Bios { boot, btrfs }
        }
    };

    // This root moves into @<distro>, and its kernels go where the layout boots them.
    let own = format!("/@{d}");
    let needs_root = m.root != own || matches!(m.layout, Layout::Unsupported { .. });
    if needs_root {
        let mut plan = Plan::default();
        mount_top(&mut plan);
        let new = format!("{top}/{}", subvol(d));
        if m.root != own {
            let from = if m.root == "/" { top.clone() } else { format!("{top}{}", m.root) };
            plan.expect(
                Cmd::new("test").arg("-e").arg(&new),
                false,
                format!("{} already exists; rename or delete it first", subvol(d)),
            );
            plan.run(Cmd::new("btrfs").args(["subvolume", "snapshot", &from, &new]));
        }
        boot_setup(paths, &mut plan, m, &target, &new)?;
        let what = if m.root == own {
            format!("set up {d}'s boot on the {} layout", target.name())
        } else {
            format!("copy this root into {} as a snapshot and set up its boot there", subvol(d))
        };
        steps.push(Step { what, destructive: false, plan });
        return Ok(private(paths, steps));
    }

    // After a reboot into @<distro>, what the move left behind.
    if let Some(old) = &left.old_root {
        let mut plan = Plan::default();
        mount_top(&mut plan);
        let what = if old.is_empty() {
            // Everything at the top level except the subvolumes the layout uses. The old root's own
            // subvolumes (Arch's /var/lib/machines) go first, deepest first, since `rm` stays on
            // one filesystem.
            let script = r#"btrfs subvolume list "$1" | sed 's/.* path //' | grep -v '^@' | sort -r | while read -r v; do btrfs subvolume delete "$1/$v"; done
for e in "$1"/* "$1"/.[!.]*; do case "${e##*/}" in @*) ;; *) rm -rf --one-file-system "$e" ;; esac; done"#;
            plan.run(Cmd::new("sh").args(["-c", script, "sh", &top]));
            "delete the old root's files from the btrfs top level".to_string()
        } else {
            plan.run(Cmd::new("btrfs").args(["subvolume", "delete", "--recursive"]).arg(format!("{top}/{old}")));
            format!("delete the old root subvolume {old}")
        };
        steps.push(Step { what, destructive: true, plan });
    }
    if !left.shadowed.is_empty() {
        let mut plan = Plan::default();
        mount_top(&mut plan);
        for dir in &left.shadowed {
            let under = format!("{top}/@{d}{dir}");
            plan.run(Cmd::new("sh").args([
                "-c",
                r#"find "$1" -mindepth 1 -maxdepth 1 -exec rm -rf --one-file-system {} +"#,
                "sh",
                &under,
            ]));
        }
        let what = format!("delete the copies of {} left under their mounts in @{d}", left.shadowed.join(" and "));
        steps.push(Step { what, destructive: true, plan });
    }
    Ok(private(paths, steps))
}

fn private(paths: &Paths, steps: Vec<Step>) -> Vec<Step> {
    let dir = s(&paths.system(DISK));
    steps.into_iter().map(|st| Step { plan: st.plan.private(&dir), ..st }).collect()
}

fn reflink_copy(from: &str, to: &str) -> Cmd {
    Cmd::new("sh").args(["-c", r#"if [ -d "$1" ]; then cp -a --reflink=always "$1/." "$2/"; fi"#, "sh", from, to])
}

/// Shrink the btrfs partition from its end by `size` (plus alignment) and return the commands
/// and the first sector of the freed space.
fn shrink_btrfs(paths: &Paths, m: &Machine, size: u64, plan: &mut Plan) -> Result<u64> {
    if m.luks.is_some() {
        bail!(
            "the btrfs filesystem sits in LUKS, and fspec does not shrink a LUKS container; use `fspec disk init` on another disk"
        );
    }
    let p = &m.root_part;
    let sectors = |b: u64| b / 512;
    let room = size + 2 * MIB;
    if p.size < room + 8 * GIB {
        bail!("the btrfs partition {} is too small to give up {} MiB", p.dev, room / MIB);
    }
    let new_size = p.size - room;
    plan.run(
        Cmd::new("btrfs").args(["filesystem", "resize"]).arg(format!("{}", new_size - MIB)).arg(s(&paths.system("/"))),
    );
    plan.run(
        Cmd::new("sfdisk")
            .args(["--no-reread", "-N"])
            .arg(p.partn.to_string())
            .arg(&m.disk)
            .stdin(format!(", {}\n", sectors(new_size))),
    );
    plan.run(Cmd::new("partx").args(["-u", &m.disk]));
    // Align the new partition to 1 MiB.
    let end = p.start + sectors(new_size);
    Ok(end.div_ceil(2048) * 2048)
}

fn next_partn(m: &Machine) -> u64 {
    m.parts.iter().map(|p| p.partn).max().unwrap_or_default() + 1
}

/// The partition a carve step makes: `made` when an earlier run already shrank the btrfs
/// partition and created it, else a new one at the end of the disk.
fn carved(
    paths: &Paths,
    m: &Machine,
    made: Option<&Part>,
    code: &str,
    label: &str,
    plan: &mut Plan,
) -> Result<(String, u64)> {
    if let Some(p) = made {
        return Ok((p.dev.clone(), p.partn));
    }
    let start = shrink_btrfs(paths, m, ESP_SIZE, plan)?;
    let n = next_partn(m);
    plan.run(
        Cmd::new("sgdisk")
            .arg(format!("-n{n}:{start}:+2G"))
            .arg(format!("-t{n}:{code}"))
            .arg(format!("-c{n}:{label}"))
            .arg(&m.disk),
    );
    // Adding every partition fails on the ones the kernel already has.
    plan.run(Cmd::new("partx").args(["-a", "-n"]).arg(format!("{n}:{n}")).arg(&m.disk));
    plan.run(Cmd::new("udevadm").arg("settle"));
    Ok((partition(&m.disk, n), n))
}

/// Carve a 2 GiB ESP out of the btrfs partition and copy the old one into it. The new ESP takes
/// the old one's partition GUID and FAT serial, so firmware entries and every `fstab` follow it;
/// the old one is retyped as Linux data with new identifiers and stays as a copy.
fn carve_esp(paths: &Paths, m: &Machine, old: Option<&Part>, made: Option<&Part>, ids: &Ids) -> Result<(Step, Part)> {
    let mut plan = Plan::default();
    let (dev, n) = carved(paths, m, made, "ef00", "EFI", &mut plan)?;
    let serial = old.map(|o| o.uuid.replace('-', "")).unwrap_or_else(|| ids.serial.clone());
    let new_mnt = s(&paths.system(format!("{DISK}/esp")));
    let efi = s(&paths.system("/efi"));
    plan.run(Cmd::new("mkfs.vfat").args(["-F", "32", "-n", "EFI", "-i", &serial, &dev]));
    if let Some(o) = old {
        plan.run(Cmd::new("mkdir").arg("-p").arg(&new_mnt));
        plan.run(Cmd::new("mount").arg(&dev).arg(&new_mnt));
        let src = match o.mounts.first() {
            Some(at) => s(&paths.system(at)),
            None => {
                let tmp = s(&paths.system(format!("{DISK}/oldesp")));
                plan.run(Cmd::new("mkdir").arg("-p").arg(&tmp));
                plan.run(Cmd::new("mount").arg(&o.dev).arg(&tmp));
                tmp
            }
        };
        plan.run(Cmd::new("cp").arg("-a").arg(format!("{src}/.")).arg(format!("{new_mnt}/")));
        plan.run(Cmd::new("umount").arg(&new_mnt));
        plan.run(Cmd::new("umount").arg(&src));
        for extra in o.mounts.iter().skip(1) {
            plan.run(Cmd::new("umount").arg(s(&paths.system(extra))));
        }
        plan.run(Cmd::new("fatlabel").args(["-r", "-i", &o.dev]));
        plan.run(
            Cmd::new("sgdisk")
                .arg(format!("-u{}:R", o.partn))
                .arg(format!("-t{}:8300", o.partn))
                .arg(format!("-c{}:old-esp", o.partn))
                .arg(&m.disk),
        );
        plan.run(Cmd::new("sgdisk").arg(format!("-u{n}:{}", o.partuuid)).arg(&m.disk));
        plan.run(Cmd::new("partx").args(["-u", &m.disk]));
        plan.run(Cmd::new("udevadm").arg("settle"));
    }
    plan.run(Cmd::new("mkdir").arg("-p").arg(&efi));
    plan.run(Cmd::new("mount").arg(&dev).arg(&efi));
    let shrink =
        if made.is_some() { "format".to_string() } else { "shrink the btrfs partition by 2 GiB and create".into() };
    let what = match old {
        Some(o) => format!("{shrink} a 2 GiB ESP at {dev} and move the {} MiB ESP {} into it", o.size / MIB, o.dev),
        None => format!("{shrink} a 2 GiB ESP at {dev}"),
    };
    let uuid = format!("{}-{}", &serial[..4], &serial[4..]);
    let part = Part { dev, uuid, parttype: ESP_TYPE.into(), fstype: "vfat".into(), size: ESP_SIZE, ..Part::default() };
    Ok((Step { what, destructive: true, plan }, part))
}

/// Carve the BIOS layout's shared ext4 boot partition out of the btrfs partition.
fn carve_boot(paths: &Paths, m: &Machine, made: Option<&Part>, ids: &Ids) -> Result<(Step, Part)> {
    let mut plan = Plan::default();
    let (dev, _) = carved(paths, m, made, "8300", "boot", &mut plan)?;
    let shared = s(&paths.system(SHARED_BOOT));
    plan.run(Cmd::new("mkfs.ext4").args(["-F", "-L", "boot", "-U", &ids.boot, &dev]));
    plan.run(Cmd::new("mkdir").arg("-p").arg(&shared));
    plan.run(Cmd::new("mount").arg(&dev).arg(&shared));
    let what = match made {
        Some(_) => format!("format the shared boot partition at {dev}"),
        None => format!("shrink the btrfs partition by 2 GiB and create the shared boot partition at {dev}"),
    };
    let part = Part { dev, uuid: ids.boot.clone(), fstype: "ext4".into(), size: BOOT_SIZE, ..Part::default() };
    Ok((Step { what, destructive: true, plan }, part))
}

/// Make the running distro's root at `new` (the mounted `@<distro>` path) boot on `layout`: its
/// `fstab`, kernel tooling, kernels, and the bootloader with this root as the default.
fn boot_setup(paths: &Paths, plan: &mut Plan, m: &Machine, layout: &Layout, new: &str) -> Result<()> {
    let d = m.running.as_str();
    let Some(boot) = Boot::of(layout) else { bail!("no layout to set up boot on") };
    let luks = m.luks.is_some();
    let src = Sources::running(paths);
    let root = s(&paths.system(format!("{DISK}/{d}")));
    plan.run(Cmd::new("mkdir").arg("-p").arg(&root));
    plan.run(Cmd::new("mount").arg("--bind").arg(new).arg(&root));
    plan.cleanup.insert(0, Cmd::new("umount").arg("-R").arg(&root));
    let options = kernel_options(layout, d, &m.cmdline);
    let tee = |plan: &mut Plan, path: String, text: String| {
        plan.run(Cmd::new("mkdir").arg("-p").arg(std::path::Path::new(&path).parent().map(s).unwrap_or_default()));
        plan.run(Cmd::new("tee").arg(path).stdin(text));
    };
    tee(plan, format!("{root}/etc/fstab"), fstab(layout, d));
    if let Some(l) = &m.luks {
        let opts = if matches!(d, "debian" | "ubuntu") { "luks,discard,initramfs" } else { "luks,discard" };
        tee(plan, format!("{root}/etc/crypttab"), format!("luks-{l} UUID={l} none {opts}\n"));
    }
    tee(plan, format!("{root}/etc/kernel/cmdline"), format!("{options}\n"));
    for f in boot_files(d, boot, luks) {
        tee(plan, format!("{root}{}", f.path), f.text);
        if f.exec {
            plan.run(Cmd::new("chmod").arg("755").arg(format!("{root}{}", f.path)));
        }
    }
    let shared = format!("{}/{d}", src.shared_boot);
    if boot == Boot::Grub {
        plan.run(Cmd::new("mkdir").arg("-p").arg(&shared));
        plan.run(Cmd::new("cp").arg("-a").arg(format!("{root}/boot/.")).arg(format!("{shared}/")));
        tee(plan, format!("{shared}/fspec-cmdline"), format!("{options}\n"));
    }
    let mut binds = vec![(src.home.clone(), "home"), ("/dev".into(), "dev"), ("/sys".into(), "sys")];
    match boot {
        Boot::Grub => binds.push((shared.clone(), "boot")),
        _ => binds.push((src.esp.clone(), "efi")),
    }
    for (from, to) in binds {
        let to = format!("{root}/{to}");
        plan.run(Cmd::new("mkdir").arg("-p").arg(&to));
        plan.run(Cmd::new("mount").arg("--rbind").arg(from).arg(&to));
        plan.run(Cmd::new("mount").arg("--make-rslave").arg(&to));
    }
    plan.run(Cmd::new("mount").args(["-t", "proc", "proc"]).arg(format!("{root}/proc")));
    let chroot = |cmd: &str| {
        Cmd::new("chroot")
            .arg(&root)
            .args(["sh", "-c", cmd])
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
    };
    if let Some(install) = install_packages(d, boot, luks) {
        plan.run(chroot(&install));
    }
    plan.run(chroot(&populate(d, std::env::consts::ARCH, boot, luks)));
    match layout {
        Layout::Default { .. } => {
            plan.run(Cmd::new("bootctl").arg(format!("--esp-path={}", src.esp)).arg("install"));
            sd_default(&src.esp, d).into_iter().for_each(|c| plan.run(c));
        }
        Layout::Bios { boot: b, .. } => {
            let install = if d == "fedora" { "grub2-install" } else { "grub-install" };
            plan.run(
                Cmd::new(install)
                    .arg("--target=i386-pc")
                    .arg(format!("--boot-directory={}", src.shared_boot))
                    .arg(&m.disk),
            );
            plan.run(grub_menu(&src.shared_boot, &b.uuid, d));
        }
        _ => {}
    }
    Ok(())
}

/// The running distro's own package manager installing the boot packages [`populate`] does not
/// install itself.
fn install_packages(distro: &str, boot: Boot, luks: bool) -> Option<String> {
    let pkgs = crate::recipe::boot_packages(distro, boot, luks).join(" ");
    if pkgs.is_empty() {
        return None;
    }
    Some(match distro {
        "arch" => format!("pacman -S --needed --noconfirm {pkgs}"),
        "fedora" => format!("dnf install -y {pkgs}"),
        "void" => format!("xbps-install -y {pkgs}"),
        _ => return None,
    })
}

/// The `sed` expressions that turn the ext4 root line of an `fstab` into a btrfs one: the type,
/// no fsck pass (btrfs has none), and without ext4's `errors=`, which btrfs refuses to mount with.
pub(crate) const FSTAB_SED: [&str; 3] = [
    r"s#^\([^[:space:]]\{1,\}[[:space:]]\{1,\}/[[:space:]]\{1,\}\)ext4\([[:space:]]\{1,\}[^[:space:]]\{1,\}[[:space:]]\{1,\}[^[:space:]]\{1,\}[[:space:]]\{1,\}\)[0-9]#\1btrfs\20#",
    r"/^[^[:space:]]\{1,\}[[:space:]]\{1,\}\/[[:space:]]\{1,\}btrfs[[:space:]]/s/,\{0,1\}errors=[^,[:space:]]*//",
    r"s#^\([^[:space:]]\{1,\}[[:space:]]\{1,\}/[[:space:]]\{1,\}btrfs[[:space:]]\{1,\}\),#\1#",
];

/// The ext4 root partition `device` (unmounted) converted to btrfs in place, keeping its UUID, with
/// its `fstab` and initramfs updated and GRUB's config rewritten so it boots as before. `disk
/// migrate` then runs on the converted system for the rest.
pub fn convert(paths: &Paths, device: &str, disk: &str, firmware: &Firmware) -> Plan {
    let mnt = s(&paths.system(format!("{DISK}/convert")));
    let mut plan = Plan::default();
    plan.run(Cmd::new("e2fsck").args(["-fy", device]));
    plan.run(Cmd::new("btrfs-convert").args(["--uuid", "copy", device]));
    plan.run(Cmd::new("mkdir").arg("-p").arg(&mnt));
    plan.run(Cmd::new("mount").arg(device).arg(&mnt));
    plan.cleanup.push(Cmd::new("umount").arg("-R").arg(&mnt));
    plan.run(Cmd::new("sed").arg("-i").args(FSTAB_SED.iter().flat_map(|e| ["-e", e])).arg(format!("{mnt}/etc/fstab")));
    for (from, to) in [("/dev", "dev"), ("/sys", "sys"), ("/proc", "proc")] {
        plan.run(Cmd::new("mount").arg("--rbind").arg(from).arg(format!("{mnt}/{to}")));
        plan.run(Cmd::new("mount").arg("--make-rslave").arg(format!("{mnt}/{to}")));
    }
    let grub = match firmware {
        Firmware::Bios => format!("grub-install --target=i386-pc {disk} || grub2-install --target=i386-pc {disk}; "),
        Firmware::Efi => String::new(),
    };
    let script = format!(
        "mount -a -O no_netdev 2>/dev/null || true\n\
         if ! command -v btrfs >/dev/null; then\n\
         if command -v apt-get >/dev/null; then apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y btrfs-progs\n\
         elif command -v dnf >/dev/null; then dnf install -y btrfs-progs\n\
         elif command -v pacman >/dev/null; then pacman -S --noconfirm btrfs-progs\n\
         else xbps-install -Sy btrfs-progs; fi; fi\n\
         if command -v mkinitcpio >/dev/null; then mkinitcpio -P\n\
         elif command -v update-initramfs >/dev/null; then update-initramfs -u -k all\n\
         else dracut --regenerate-all --force; fi\n\
         {grub}if command -v grub-mkconfig >/dev/null; then grub-mkconfig -o /boot/grub/grub.cfg\n\
         else grub2-mkconfig -o /boot/grub2/grub.cfg; fi\n\
         umount -a -t vfat 2>/dev/null || true\n"
    );
    // The root's own resolv.conf can point at a resolver that is not running here; it goes back
    // afterwards, whether or not the chroot succeeded.
    let src = Sources::running(paths);
    let resolv = format!("{mnt}/etc/resolv.conf");
    plan.run(Cmd::new("sh").args([
        "-c",
        r#"mv "$3" "$3.fspec" 2>/dev/null || true; src=$1; if grep -qs '^nameserver 127\.0\.0\.53' "$src" && [ -s "$2" ]; then src=$2; fi; cp -L "$src" "$3""#,
        "sh",
        &src.resolv,
        &src.resolved,
        &resolv,
    ]));
    plan.cleanup.insert(
        0,
        Cmd::new("sh").args([
            "-c",
            r#"if [ -e "$1.fspec" ] || [ -L "$1.fspec" ]; then mv -f "$1.fspec" "$1"; fi"#,
            "sh",
            &resolv,
        ]),
    );
    plan.run(
        Cmd::new("chroot")
            .arg(&mnt)
            .args(["sh", "-ec", &script])
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"),
    );
    plan.private(&s(&paths.system(DISK)))
}

use anyhow::{Result, bail};
use fspec_core::Paths;
use fspec_core::facts::os_release_id;
use fspec_core::runner::{Cmd, Runner};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bootloader {
    /// Kernels are copied onto the EFI system partition, which systemd-boot can read. `default`
    /// is the text of the entry `loader.conf` boots by default, when it names one that exists.
    SystemdBoot { esp: String, default: Option<String> },
    /// GRUB reads the kernel from the owning root's `/boot`. `mkconfig` is `grub-mkconfig` or
    /// `grub2-mkconfig`, and `cfg` the file it writes.
    Grub { mkconfig: &'static str, cfg: &'static str },
}

/// A filesystem mounted at `/boot`, `/efi` or `/boot/efi`. Every root of the machine shares it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootMount {
    pub target: String,
    pub uuid: String,
    pub fstype: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Firmware {
    Efi,
    /// GRUB goes into the boot sector of `disk`, the disk holding the root filesystem.
    Bios {
        disk: String,
    },
}

/// What the plans need to know about the running system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    /// Filesystem UUID shared by the root and `/home` subvolumes.
    pub uuid: String,
    /// The booted root subvolume as findmnt prints it, e.g. `/@`, or `/` for the top level.
    pub root_subvol: String,
    pub home_subvol: String,
    pub bootloader: Bootloader,
    /// `ID=` of the running `/etc/os-release`.
    pub running: String,
    pub boot_mounts: Vec<BootMount>,
    pub firmware: Firmware,
    /// The running kernel's command line. Entries fspec writes keep its options, like `console=`
    /// and `net.ifnames=0`, which the machine's network and serial setup can depend on.
    pub cmdline: String,
}

impl Machine {
    /// The filesystem UUID GRUB finds the kernels on, and where this root's `/boot` sits inside it.
    pub fn grub_boot(&self) -> (&str, String) {
        match self.boot_mounts.iter().find(|b| b.target == "/boot") {
            Some(b) => (&b.uuid, String::new()),
            None => (&self.uuid, format!("{}/boot", self.root_subvol.trim_end_matches('/'))),
        }
    }

    pub fn esp(&self) -> Option<&BootMount> {
        self.boot_mounts.iter().find(|b| b.fstype == "vfat")
    }
}

/// `rootflags=subvol=` for a root as findmnt prints it.
pub(crate) fn subvol_arg(fsroot: &str) -> &str {
    match fsroot.trim_start_matches('/') {
        "" => "/",
        s => s,
    }
}

/// A tool that is not installed answers no, like one that fails: Debian ships no `grub-mkconfig`
/// unless GRUB is installed.
fn probe(runner: &dyn Runner, cmd: Cmd) -> Result<Option<String>> {
    let out = match runner.run(&cmd) {
        Ok(out) => out,
        Err(e) if crate::not_installed(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok((out.status == 0).then(|| out.stdout.trim().to_string()))
}

const BOOT_TARGETS: [&str; 3] = ["/boot", "/efi", "/boot/efi"];

/// Read the running system through `runner`. Refuses a root that is not btrfs, a `/home` that is
/// not its own subvolume, and a root with neither systemd-boot nor GRUB.
pub fn detect(runner: &dyn Runner, paths: &Paths) -> Result<Machine> {
    let sys = |p: &str| paths.system(p).display().to_string();
    let findmnt = |fields: &str, path: &str| Cmd::new("findmnt").args(["-no", fields, "-T"]).arg(sys(path));

    let Some(root) = probe(runner, findmnt("FSTYPE,UUID,FSROOT", "/"))? else {
        bail!("could not read the root filesystem with findmnt");
    };
    let mut f = root.split_whitespace();
    let (Some(fstype), Some(uuid), Some(root_subvol)) = (f.next(), f.next(), f.next()) else {
        bail!("findmnt answered `{root}` for the root filesystem, expected <fstype> <uuid> <subvolume>");
    };
    if fstype != "btrfs" {
        bail!(
            "the root filesystem is {fstype}, not btrfs. fspec switch --distro keeps the old system bootable in a \
             second btrfs subvolume, so it needs a btrfs root; reinstall on btrfs or move to a bootc image"
        );
    }
    let home_subvol = probe(runner, findmnt("FSROOT", "/home"))?.unwrap_or_default();
    if home_subvol.is_empty() || home_subvol == root_subvol {
        bail!("/home is not its own btrfs subvolume, so it cannot be shared with the new root; move it to one first");
    }

    // Without -T, findmnt only answers for a mount point.
    let mut boot_mounts = Vec::new();
    for target in BOOT_TARGETS {
        let found = probe(runner, Cmd::new("findmnt").args(["-no", "UUID,FSTYPE"]).arg(sys(target)))?;
        if let Some((uuid, fstype)) = found.as_deref().and_then(|l| l.split_once(char::is_whitespace)) {
            boot_mounts.push(BootMount { target: target.into(), uuid: uuid.into(), fstype: fstype.trim().into() });
        }
    }

    let sd_boot =
        boot_mounts.iter().find(|b| b.fstype == "vfat" && paths.system(format!("{}/EFI/systemd", b.target)).is_dir());
    let bootloader = if let Some(esp) = sd_boot {
        Bootloader::SystemdBoot { esp: esp.target.clone(), default: default_entry(paths, &esp.target) }
    } else if probe(runner, Cmd::new("test").arg("-d").arg(sys("/boot/grub2")))?.is_some()
        || probe(runner, Cmd::new("grub2-mkconfig").arg("--version"))?.is_some()
    {
        Bootloader::Grub { mkconfig: "grub2-mkconfig", cfg: "/boot/grub2/grub.cfg" }
    } else if probe(runner, Cmd::new("test").arg("-d").arg(sys("/boot/grub")))?.is_some()
        || probe(runner, Cmd::new("grub-mkconfig").arg("--version"))?.is_some()
    {
        Bootloader::Grub { mkconfig: "grub-mkconfig", cfg: "/boot/grub/grub.cfg" }
    } else {
        bail!(
            "found neither systemd-boot (EFI/systemd on a mounted ESP) nor GRUB (/boot/grub, /boot/grub2, grub-mkconfig)"
        );
    };

    let firmware = if probe(runner, Cmd::new("test").arg("-d").arg(sys("/sys/firmware/efi")))?.is_some() {
        Firmware::Efi
    } else {
        let source = probe(runner, Cmd::new("findmnt").args(["-nvo", "SOURCE", "-T"]).arg(sys("/")))?;
        let disk = match source {
            Some(dev) => probe(runner, Cmd::new("lsblk").args(["-ndpo", "PKNAME", &dev]))?.unwrap_or_default(),
            None => String::new(),
        };
        Firmware::Bios { disk }
    };

    let running = std::fs::read_to_string(paths.system("/etc/os-release"))
        .ok()
        .and_then(|s| os_release_id(&s))
        .unwrap_or_default();
    let cmdline = std::fs::read_to_string(paths.system("/proc/cmdline")).unwrap_or_default().trim().to_string();
    Ok(Machine {
        uuid: uuid.into(),
        root_subvol: root_subvol.into(),
        home_subvol,
        bootloader,
        running,
        boot_mounts,
        firmware,
        cmdline,
    })
}

/// The entry `loader.conf` names as `default`, when it is a plain file name.
fn default_entry(paths: &Paths, esp: &str) -> Option<String> {
    let conf = std::fs::read_to_string(paths.system(format!("{esp}/loader/loader.conf"))).ok()?;
    let name = conf.lines().find_map(|l| l.trim().strip_prefix("default"))?.trim();
    std::fs::read_to_string(paths.system(format!("{esp}/loader/entries/{name}"))).ok()
}

/// What a `--root` run assumes, since its commands are only logged and answer nothing:
/// systemd-boot on an ESP at `/boot`, root in `@`, home in `@home`.
pub fn assumed() -> Machine {
    let uuid = "00000000-0000-0000-0000-000000000000".to_string();
    Machine {
        uuid,
        root_subvol: "/@".into(),
        home_subvol: "/@home".into(),
        bootloader: Bootloader::SystemdBoot { esp: "/boot".into(), default: None },
        running: String::new(),
        boot_mounts: vec![BootMount { target: "/boot".into(), uuid: "0000-0000".into(), fstype: "vfat".into() }],
        firmware: Firmware::Efi,
        cmdline: String::new(),
    }
}

/// The root that owns boot: its bootloader is the one the firmware starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    /// As findmnt prints it, like [`Machine::root_subvol`].
    pub subvol: String,
    /// `ID=` of that root's os-release.
    pub distro: String,
    /// Its kernel command line when it took boot, empty when none was recorded.
    pub cmdline: String,
}

/// Kept on the btrfs top level, which every root can mount. Its first line is the owner's
/// subvolume and its second the owner's kernel command line.
pub(crate) const OWNER_FILE: &str = ".fspec-boot-owner";

pub(crate) fn owner_record(m: &Machine) -> String {
    format!("{}\n{}\n", m.root_subvol, m.cmdline)
}

/// Read which root owns boot. `switch --distro` records the running root the first time, and
/// `switch --adopt-boot` moves it. `None` when nothing was recorded.
pub fn boot_owner(runner: &dyn Runner, paths: &Paths, m: &Machine) -> Result<Option<Owner>> {
    let top = paths.system("/run/fspec/switch/owner").display().to_string();
    runner.check(&Cmd::new("mkdir").arg("-p").arg(&top))?;
    runner.check(&Cmd::new("mount").args(["-o", "ro,subvolid=5"]).arg(format!("UUID={}", m.uuid)).arg(&top))?;
    let read = || -> Result<Option<Owner>> {
        let Some(record) = probe(runner, Cmd::new("cat").arg(format!("{top}/{OWNER_FILE}")))? else { return Ok(None) };
        let mut lines = record.lines();
        let subvol = lines.next().unwrap_or_default().trim().to_string();
        let cmdline = lines.next().unwrap_or_default().trim().to_string();
        if subvol.is_empty() {
            return Ok(None);
        }
        let release = format!("{top}/{}/etc/os-release", subvol.trim_matches('/'));
        let distro = probe(runner, Cmd::new("cat").arg(release.replace("//", "/")))?
            .and_then(|s| os_release_id(&s))
            .unwrap_or_default();
        Ok(Some(Owner { subvol, distro, cmdline }))
    };
    let owner = read();
    runner.run(&Cmd::new("umount").arg(&top))?;
    owner
}

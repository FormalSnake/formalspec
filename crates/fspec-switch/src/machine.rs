use anyhow::{Result, bail};
use fspec_core::Paths;
use fspec_core::facts::os_release_id;
use fspec_core::runner::{Cmd, Runner};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bootloader {
    /// Kernels are copied onto the EFI system partition, which systemd-boot can read.
    SystemdBoot { esp: String },
    /// GRUB reads the kernel from the booted root's `/boot`. `mkconfig` is `grub-mkconfig` or
    /// `grub2-mkconfig`, and `cfg` the file it writes.
    Grub { mkconfig: &'static str, cfg: &'static str },
}

/// What the plans need to know about the running system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    /// Filesystem UUID shared by the root and `/home` subvolumes.
    pub uuid: String,
    /// The booted root subvolume as findmnt prints it, e.g. `/@`.
    pub root_subvol: String,
    pub home_subvol: String,
    pub bootloader: Bootloader,
    /// `ID=` of the running `/etc/os-release`.
    pub running: String,
    /// Prefix of `/boot` inside the filesystem, as GRUB addresses it: the booted subvolume.
    pub boot_prefix: String,
}

/// A tool that is not installed answers no, like one that fails: Debian ships no `bootctl` unless
/// systemd-boot is installed.
fn probe(runner: &dyn Runner, cmd: Cmd) -> Result<Option<String>> {
    let out = match runner.run(&cmd) {
        Ok(out) => out,
        Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    Ok((out.status == 0).then(|| out.stdout.trim().to_string()))
}

/// Read the running system through `runner`. Refuses a root that is not btrfs, a `/home` that is
/// not its own subvolume, and a machine with neither systemd-boot nor GRUB.
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

    let bootloader = if probe(runner, Cmd::new("bootctl").arg("is-installed"))?.is_some() {
        let esp = probe(runner, Cmd::new("bootctl").arg("--print-esp-path"))?.unwrap_or_default();
        if esp.is_empty() {
            bail!("systemd-boot is installed but `bootctl --print-esp-path` printed nothing");
        }
        Bootloader::SystemdBoot { esp: paths.to_spec(std::path::Path::new(&esp)) }
    } else if probe(runner, Cmd::new("test").arg("-d").arg(sys("/boot/grub2")))?.is_some() {
        Bootloader::Grub { mkconfig: "grub2-mkconfig", cfg: "/boot/grub2/grub.cfg" }
    } else if probe(runner, Cmd::new("test").arg("-d").arg(sys("/boot/grub")))?.is_some() {
        Bootloader::Grub { mkconfig: "grub-mkconfig", cfg: "/boot/grub/grub.cfg" }
    } else {
        bail!("found neither systemd-boot (`bootctl is-installed`) nor GRUB (/boot/grub, /boot/grub2)");
    };

    let mut boot_prefix = String::new();
    if matches!(bootloader, Bootloader::Grub { .. }) {
        let target = probe(runner, Cmd::new("findmnt").args(["-no", "TARGET", "-T"]).arg(sys("/boot")))?;
        if target
            .as_deref()
            .is_some_and(|t| t.trim_end_matches('/') != paths.root.display().to_string().trim_end_matches('/'))
        {
            bail!("/boot is a separate filesystem; with GRUB it must live on the btrfs root");
        }
        boot_prefix = root_subvol.to_string();
    }

    let running = std::fs::read_to_string(paths.system("/etc/os-release"))
        .ok()
        .and_then(|s| os_release_id(&s))
        .unwrap_or_default();
    Ok(Machine { uuid: uuid.into(), root_subvol: root_subvol.into(), home_subvol, bootloader, running, boot_prefix })
}

/// What a `--root` run assumes, since its commands are only logged and answer nothing:
/// systemd-boot on `/boot`, root in `@`, home in `@home`.
pub fn assumed() -> Machine {
    Machine {
        uuid: "00000000-0000-0000-0000-000000000000".into(),
        root_subvol: "/@".into(),
        home_subvol: "/@home".into(),
        bootloader: Bootloader::SystemdBoot { esp: "/boot".into() },
        running: String::new(),
        boot_prefix: String::new(),
    }
}

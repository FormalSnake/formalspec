use std::path::Path;

use fspec_core::Paths;
use fspec_core::runner::{Output, RecordingRunner};
use serde_json::json;

use crate::{
    Btrfs, Facts, Firmware, Ids, Init, Kind, Layout, Leftovers, Machine, Part, Target, adopt_boot, assumed, classify,
    forget, init, migrate, reset_var, switch_distro, to_image,
};

const UUID: &str = "1111-aaaa";

fn paths() -> Paths {
    Paths::new("/r".into(), "/r/home/u".into())
}

fn esp(dev: &str, partn: u64, mib: u64, at: &[&str]) -> serde_json::Value {
    json!({"name": dev, "type": "part", "fstype": "vfat", "size": mib << 20, "uuid": "EEEE-0001",
        "partuuid": "esp-guid", "parttype": "C12A7328-F81F-11D2-BA4B-00A0C93EC93B", "mountpoints": at,
        "fsavail": (mib / 2) << 20, "partn": partn, "start": 2048})
}

fn btrfs_part(dev: &str, partn: u64, at: &[&str]) -> serde_json::Value {
    json!({"name": dev, "type": "part", "fstype": "btrfs", "size": 40u64 << 30, "uuid": UUID, "partuuid": "root-guid",
        "parttype": "4f68bce3-e8cd-4db1-96e7-fbcaf984b709", "mountpoints": at, "partn": partn, "start": 4198400})
}

fn lsblk(pttype: &str, parts: Vec<serde_json::Value>) -> String {
    json!({"blockdevices": [{"name": "/dev/vda", "type": "disk", "size": 64u64 << 30, "pttype": pttype,
        "mountpoints": [], "children": parts}]})
    .to_string()
}

const SHARED: [&str; 4] = ["@home", "@swap", "@snapshots", "@fspec"];

fn facts(root: &str, lsblk: String, efi: bool) -> Facts {
    Facts {
        root_mount: format!("btrfs /dev/vda2 {root}"),
        lsblk,
        efi,
        home: "/@home".into(),
        fspec: "/@fspec".into(),
        volumes: SHARED.iter().map(|s| s.to_string()).chain([root.trim_start_matches('/').to_string()]).collect(),
        ..Facts::default()
    }
}

/// Arch on the default layout: a 2 GiB ESP at /efi and btrfs with every shared subvolume.
fn default_arch() -> Machine {
    let l = lsblk("gpt", vec![esp("/dev/vda1", 1, 2048, &["/efi"]), btrfs_part("/dev/vda2", 2, &["/", "/home"])]);
    classify(&facts("/@arch", l, true), "arch".into(), "BOOT_IMAGE=/x root=UUID=old rw console=ttyS0,115200".into())
        .unwrap()
}

fn bios_debian() -> Machine {
    let boot = json!({"name": "/dev/vda3", "type": "part", "fstype": "ext4", "size": 2u64 << 30, "uuid": "boot-uuid",
        "parttype": "0fc63daf-8483-4772-8e79-3d69d8477de4", "mountpoints": ["/var/lib/fspec/boot", "/boot"],
        "fsavail": 1u64 << 30, "partn": 3});
    let grub = json!({"name": "/dev/vda1", "type": "part", "size": 1u64 << 20,
        "parttype": "21686148-6449-6e6f-744e-656564454649", "partn": 1, "mountpoints": []});
    let mut f = facts("/@debian", lsblk("gpt", vec![grub, btrfs_part("/dev/vda2", 2, &["/"]), boot]), false);
    f.boot = "/debian".into();
    classify(&f, "debian".into(), String::new()).unwrap()
}

fn zfs_debian() -> Machine {
    let pool = json!({"name": "/dev/vda2", "type": "part", "fstype": "zfs_member", "size": 40u64 << 30, "partn": 2,
        "mountpoints": []});
    let f = Facts {
        root_mount: "zfs rpool/ROOT/debian /".into(),
        lsblk: lsblk("gpt", vec![esp("/dev/vda1", 1, 2048, &["/efi"]), pool]),
        efi: true,
        volumes: ["rpool", "rpool/ROOT", "rpool/ROOT/debian", "rpool/home", "rpool/fspec"].map(String::from).to_vec(),
        zpool: "rpool\t40G\n\t/dev/vda2\t40G\n".into(),
        ..Facts::default()
    };
    classify(&f, "debian".into(), String::new()).unwrap()
}

fn target(distro: &str) -> Target<'_> {
    Target {
        distro,
        host: "box",
        arch: "x86_64",
        release: Some("44"),
        spec_dir: Path::new("/work/spec"),
        spec_file: "spec.star",
        exe: Path::new("/usr/bin/fspec"),
    }
}

fn stdin_of(plan: &crate::Plan, prefix: &str) -> String {
    plan.steps
        .iter()
        .find_map(|s| match s {
            crate::Step::Run(c) if c.to_string().starts_with(prefix) => c.stdin.clone(),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no `{prefix}` in {:#?}", plan.lines()))
}

fn position(lines: &[String], prefix: &str) -> usize {
    lines.iter().position(|l| l.starts_with(prefix)).unwrap_or_else(|| panic!("no `{prefix}` in {lines:#?}"))
}

#[test]
fn classify_reads_the_default_layout() {
    let m = default_arch();
    let Layout::Default { esp, btrfs } = &m.layout else { panic!("{:?}", m.layout) };
    assert_eq!((esp.dev.as_str(), esp.uuid.as_str(), esp.size), ("/dev/vda1", "EEEE-0001", 2 << 30));
    assert_eq!(btrfs, &Btrfs { uuid: UUID.into(), luks: None, home: "/@home".into() });
    assert_eq!((m.root.as_str(), m.disk.as_str(), m.firmware.clone()), ("/@arch", "/dev/vda", Firmware::Efi));
    assert_eq!(m.root_part.partn, 2);
}

#[test]
fn classify_finds_the_luks_container_under_btrfs() {
    let crypt = json!({"name": "/dev/mapper/luks-cccc", "type": "crypt", "fstype": "btrfs", "uuid": UUID,
        "mountpoints": ["/"]});
    let luks = json!({"name": "/dev/vda2", "type": "part", "fstype": "crypto_LUKS", "uuid": "cccc", "partn": 2,
        "size": 40u64 << 30, "mountpoints": [], "children": [crypt]});
    let mut f = facts("/@arch", lsblk("gpt", vec![esp("/dev/vda1", 1, 2048, &["/efi"]), luks]), true);
    f.root_mount = "btrfs /dev/mapper/luks-cccc /@arch".into();
    let m = classify(&f, "arch".into(), String::new()).unwrap();
    assert_eq!(m.layout.btrfs().unwrap().luks.as_deref(), Some("cccc"));
    assert_eq!(m.root_part.dev, "/dev/vda2");
}

#[test]
fn classify_names_what_keeps_a_disk_off_the_layouts() {
    // A cloud image: root at the top level, a small ESP at /boot/efi, nothing shared.
    let l = lsblk("gpt", vec![esp("/dev/vda1", 1, 300, &["/boot/efi"]), btrfs_part("/dev/vda2", 2, &["/"])]);
    let mut f = facts("/", l, true);
    f.volumes = vec!["@home".into()];
    f.fspec = String::new();
    let m = classify(&f, "arch".into(), String::new()).unwrap();
    let Layout::Unsupported { reasons } = &m.layout else { panic!("{:?}", m.layout) };
    assert_eq!(
        reasons,
        &[
            "the root sits at the btrfs top level instead of in @arch",
            "the shared subvolumes @swap, @snapshots, @fspec are missing",
            "the ESP is mounted at /boot/efi, not /efi",
        ]
    );
}

#[test]
fn classify_reads_the_bios_and_zfs_layouts() {
    let m = bios_debian();
    let Layout::Bios { boot, .. } = &m.layout else { panic!("{:?}", m.layout) };
    assert_eq!(boot.uuid, "boot-uuid");
    let m = zfs_debian();
    assert_eq!(
        m.layout,
        Layout::Zfs {
            esp: match &m.layout {
                Layout::Zfs { esp, .. } => esp.clone(),
                _ => panic!(),
            },
            pool: "rpool".into()
        }
    );
    assert_eq!(m.root, "rpool/ROOT/debian");
}

#[test]
fn switching_on_the_default_layout_builds_a_uki_root() {
    let plan = switch_distro(&paths(), &default_arch(), Some("arch"), &target("fedora")).unwrap();
    let lines = plan.lines();
    let at = |p: &str| position(&lines, p);
    assert!(
        at("mount -o subvolid=5,noatime,compress=zstd:1 UUID=1111-aaaa")
            < at("test -e /r/run/fspec/switch/top/@fedora")
    );
    assert!(at("btrfs subvolume create /r/run/fspec/switch/top/@fedora") < at("dnf --installroot"));
    assert!(lines[at("dnf --installroot")].ends_with("kernel systemd-ukify systemd-boot-unsigned"));
    assert!(at("tee /r/run/fspec/switch/fedora/etc/kernel/install.conf") < at("mount --rbind /r/efi"));
    assert!(
        at("chroot /r/run/fspec/switch/fedora /usr/local/bin/fspec")
            < at("chroot /r/run/fspec/switch/fedora sh -c 'set -e")
    );
    assert_eq!(
        stdin_of(&plan, "tee /r/run/fspec/switch/fedora/etc/fstab"),
        "UUID=1111-aaaa / btrfs subvol=@fedora,compress=zstd 0 0\n\
         UUID=1111-aaaa /home btrfs subvol=@home,compress=zstd 0 0\n\
         UUID=1111-aaaa /opt/fspec btrfs subvol=@fspec,compress=zstd 0 0\n\
         UUID=1111-aaaa /swap btrfs subvol=@swap 0 0\n\
         /swap/swapfile none swap defaults,nofail 0 0\n\
         UUID=EEEE-0001 /efi vfat umask=0077 0 2\n"
    );
    assert_eq!(
        stdin_of(&plan, "tee /r/run/fspec/switch/fedora/etc/kernel/cmdline"),
        "root=UUID=1111-aaaa rootflags=subvol=@fedora rw console=ttyS0,115200\n"
    );
    // Boot is already owned, so the default entry stays.
    assert!(!lines.iter().any(|l| l.contains("loader.conf")));
    assert_eq!(
        lines[lines.len() - 3..],
        ["umount -R /r/run/fspec/switch/fedora", "umount /r/run/fspec/switch/top", "umount /r/run/fspec/switch"]
    );
}

#[test]
fn the_first_switch_keeps_the_running_root_the_default() {
    let plan = switch_distro(&paths(), &default_arch(), None, &target("void")).unwrap();
    assert!(plan.lines().iter().any(|l| l.ends_with("sh /r/efi/loader/loader.conf arch")), "{:#?}", plan.lines());
    let void = stdin_of(&plan, "tee /r/run/fspec/switch/void/etc/kernel.d/post-install/60-fspec");
    assert!(void.contains("/efi/loader/entries/void-fspec.conf"));
}

#[test]
fn switching_on_the_bios_layout_puts_the_kernels_in_the_shared_partition() {
    let plan = switch_distro(&paths(), &bios_debian(), Some("debian"), &target("arch")).unwrap();
    let lines = plan.lines();
    let at = |p: &str| position(&lines, p);
    assert!(at("mount --bind /r/var/lib/fspec/boot/arch /r/run/fspec/switch/arch/boot") < at("pacstrap"));
    assert!(lines[at("pacstrap")].ends_with("btrfs-progs grub"));
    assert!(stdin_of(&plan, "tee /r/run/fspec/switch/arch/etc/fstab").ends_with(
        "UUID=boot-uuid /var/lib/fspec/boot ext4 defaults 0 2\n/var/lib/fspec/boot/arch /boot none bind 0 0\n"
    ));
    assert_eq!(
        stdin_of(&plan, "tee /r/var/lib/fspec/boot/arch/fspec-cmdline"),
        "root=UUID=1111-aaaa rootflags=subvol=@arch rw\n"
    );
    assert!(lines.iter().any(|l| l.ends_with("sh /r/var/lib/fspec/boot boot-uuid debian")), "{lines:#?}");
    assert!(!lines.iter().any(|l| l.contains("rbind /r/efi")));
}

#[test]
fn switching_on_zfs_makes_a_boot_environment() {
    let plan = switch_distro(&paths(), &zfs_debian(), Some("debian"), &target("void")).unwrap();
    let lines = plan.lines();
    let at = |p: &str| position(&lines, p);
    assert!(
        at("zfs list -H -o name rpool/ROOT/void") < at("zfs create -o canmount=noauto -o mountpoint=/ rpool/ROOT/void")
    );
    assert!(at("mount -t zfs -o zfsutil rpool/ROOT/void /r/run/fspec/switch/void") < at("xbps-install"));
    assert!(lines[at("xbps-install")].ends_with("btrfs-progs zfs linux-headers"));
    assert!(lines.contains(&"cp /r/etc/hostid /r/run/fspec/switch/void/etc/hostid".to_string()));
    assert!(lines.contains(&"zfs set org.zfsbootmenu:commandline= rpool/ROOT/void".to_string()));
    assert_eq!(stdin_of(&plan, "tee /r/run/fspec/switch/void/etc/fstab"), "UUID=EEEE-0001 /efi vfat umask=0077 0 2\n");
}

#[test]
fn a_switch_the_disk_cannot_take_is_refused_with_the_fix() {
    let mut m = default_arch();
    if let Layout::Default { esp, .. } = &mut m.layout {
        esp.size = 300 << 20;
    }
    let err = switch_distro(&paths(), &m, Some("arch"), &target("fedora")).unwrap_err().to_string();
    assert!(err.contains("the ESP is 300 MiB and fedora needs 1024 MiB; run `fspec disk migrate`"), "{err}");
    let err = switch_distro(&paths(), &zfs_debian(), Some("debian"), &target("arch")).unwrap_err().to_string();
    assert!(err.contains("arch ships ZFS only from a third-party repository"), "{err}");
    let unsupported = Machine { layout: Layout::Unsupported { reasons: vec!["no ESP".into()] }, ..default_arch() };
    let err = switch_distro(&paths(), &unsupported, None, &target("void")).unwrap_err().to_string();
    assert!(err.contains("not on a supported layout: no ESP; run `fspec disk plan`"), "{err}");
    assert!(crate::caveats("debian", &zfs_debian().layout)[0].contains("Secure Boot"));
    assert!(crate::caveats("ubuntu", &zfs_debian().layout).is_empty());
}

#[test]
fn the_kernel_command_line_keeps_the_machine_options_and_adds_luks() {
    let mut m = default_arch();
    m.cmdline = "BOOT_IMAGE=/v root=UUID=x ro rootflags=subvol=@old,compress=zstd:1 rd.luks.uuid=old quiet \
                 systemd.machine_id=abc moss.fstx=3 spl.spl_hostid=0x1"
        .into();
    if let Layout::Default { btrfs, .. } = &mut m.layout {
        btrfs.luks = Some("cccc".into());
    }
    let plan = switch_distro(&paths(), &m, Some("arch"), &target("debian")).unwrap();
    assert_eq!(
        stdin_of(&plan, "tee /r/run/fspec/switch/debian/etc/kernel/cmdline"),
        "root=UUID=1111-aaaa rootflags=subvol=@debian,compress=zstd:1 rw rd.luks.uuid=cccc quiet\n"
    );
    assert_eq!(
        stdin_of(&plan, "tee /r/run/fspec/switch/debian/etc/crypttab"),
        "luks-cccc UUID=cccc none luks,discard,initramfs\n"
    );
    let populate =
        plan.lines().into_iter().find(|l| l.starts_with("chroot /r/run/fspec/switch/debian sh -c 'set -e")).unwrap();
    assert!(populate.contains("systemd-ukify systemd-boot-efi cryptsetup cryptsetup-initramfs"), "{populate}");
    assert!(populate.contains("update-initramfs -c -k"));
}

#[test]
fn a_stub_resolver_is_replaced_by_the_upstream_servers() {
    let plan = switch_distro(&paths(), &default_arch(), Some("arch"), &target("void")).unwrap();
    let line = plan.lines().into_iter().find(|l| l.contains("resolv.conf")).unwrap();
    assert!(
        line.ends_with(
            "sh /r/etc/resolv.conf /r/run/systemd/resolve/resolv.conf /r/run/fspec/switch/void/etc/resolv.conf"
        ),
        "{line}"
    );
}

#[test]
fn forget_refuses_unless_this_root_owns_boot() {
    let m = default_arch();
    let err = forget(&paths(), &m, Some("void"), "fedora").unwrap_err().to_string();
    assert_eq!(err, "boot is owned by void, not this root (arch); run `fspec switch --adopt-boot` from arch first");
    let err = forget(&paths(), &m, None, "fedora").unwrap_err().to_string();
    assert!(err.starts_with("no root is recorded as owning boot"));
    assert!(forget(&paths(), &m, Some("arch"), "arch").unwrap_err().to_string().contains("is the system you booted"));
}

#[test]
fn forget_clears_the_roots_boot_files_on_each_layout() {
    let plan = forget(&paths(), &default_arch(), Some("arch"), "fedora").unwrap();
    assert_eq!(
        plan.lines()[5..],
        [
            "test -e /r/run/fspec/switch/top/@fedora",
            r#"sh -c 'rm -rf "$1/$2" "$1"/EFI/Linux/"$2"-*.efi "$1"/loader/entries/"$2"-*.conf' sh /r/efi fedora"#,
            "btrfs subvolume delete --recursive /r/run/fspec/switch/top/@fedora",
            "umount /r/run/fspec/switch/top",
            "umount /r/run/fspec/switch"
        ]
    );
    let lines = forget(&paths(), &bios_debian(), Some("debian"), "arch").unwrap().lines();
    assert!(lines.contains(&"rm -rf /r/var/lib/fspec/boot/arch".to_string()));
    assert!(lines.iter().any(|l| l.ends_with("sh /r/var/lib/fspec/boot boot-uuid debian")));
    assert_eq!(
        forget(&paths(), &zfs_debian(), Some("debian"), "void").unwrap().lines(),
        ["zfs list -H -o name rpool/ROOT/void", "zfs destroy -r rpool/ROOT/void"]
    );
}

#[test]
fn adopt_boot_makes_this_root_the_default_on_each_layout() {
    let lines = adopt_boot(&paths(), &default_arch()).unwrap().lines();
    assert!(lines[0].ends_with("sh /r/efi/loader/loader.conf arch") && lines[1].contains("bootctl set-default ''"));
    let lines = adopt_boot(&paths(), &bios_debian()).unwrap().lines();
    assert_eq!(lines[0], "grub-install --target=i386-pc --boot-directory=/r/var/lib/fspec/boot /dev/vda");
    assert!(lines[1].ends_with("sh /r/var/lib/fspec/boot boot-uuid debian"));
    assert_eq!(adopt_boot(&paths(), &zfs_debian()).unwrap().lines(), ["zpool set bootfs=rpool/ROOT/debian rpool"]);
}

#[test]
fn the_grub_menu_lists_every_root_folder_with_its_stable_kernel_names() {
    let dir = tempfile::tempdir().unwrap();
    let b = dir.path();
    for (d, files) in [
        ("arch", vec!["vmlinuz-linux", "initramfs-linux.img"]),
        ("debian", vec!["vmlinuz", "initrd.img"]),
        ("void", vec!["vmlinuz"]),
        ("lost+found", vec![]),
    ] {
        std::fs::create_dir_all(b.join(d)).unwrap();
        for f in files {
            std::fs::write(b.join(d).join(f), "").unwrap();
        }
        std::fs::write(
            b.join(d).join("fspec-cmdline"),
            format!("root=UUID=x rootflags=subvol=@{d} rw console=ttyS0\n"),
        )
        .unwrap();
    }
    std::fs::remove_file(b.join("lost+found/fspec-cmdline")).unwrap();
    let out = std::process::Command::new("sh")
        .args(["-c", crate::classic::GRUB_MENU, "sh"])
        .arg(b)
        .args(["boot-uuid", "debian"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let cfg = std::fs::read_to_string(b.join("grub/grub.cfg")).unwrap();
    assert_eq!(std::fs::read_to_string(b.join("grub2/grub.cfg")).unwrap(), cfg);
    assert!(cfg.starts_with("set timeout=5\nset default=\"fspec-debian\"\nserial --unit=0"), "{cfg}");
    assert!(cfg.contains(
        "menuentry 'Arch (fspec)' --id fspec-arch {\n\tsearch --no-floppy --fs-uuid --set=root boot-uuid\n\tlinux \
         /arch/vmlinuz-linux root=UUID=x rootflags=subvol=@arch rw console=ttyS0\n\tinitrd /arch/initramfs-linux.img\n}"
    ));
    assert!(cfg.contains("linux /debian/vmlinuz ") && cfg.contains("initrd /debian/initrd.img"));
    // Void has no initramfs yet, so it gets no entry.
    assert!(!cfg.contains("void") && !cfg.contains("lost"));
}

fn ids() -> Ids {
    Ids { fs: "ffff".into(), luks: "cccc".into(), serial: "ABCD1234".into(), boot: "bbbb".into() }
}

#[test]
fn init_formats_the_default_layout_with_luks() {
    let ids = ids();
    let i = Init {
        device: "/dev/vdb",
        kind: Kind::Default,
        luks: true,
        ids: &ids,
        swap_gib: 4,
        efi_vars: false,
        target: None,
        cmdline: "",
        in_root: &[],
        passphrase: None,
    };
    let plan = init(&paths(), &i).unwrap();
    assert_eq!(
        plan.lines(),
        [
            "mkdir -p /r/run/fspec/disk",
            "mount --bind /r/run/fspec/disk /r/run/fspec/disk",
            "mount --make-private /r/run/fspec/disk",
            "wipefs -a /dev/vdb",
            "sgdisk --zap-all /dev/vdb",
            "sgdisk -n1:0:+2G -t1:ef00 -c1:EFI -n2:0:0 -t2:8309 -c2:root /dev/vdb",
            "udevadm settle",
            "wipefs -a /dev/vdb1 /dev/vdb2",
            "mkfs.vfat -F 32 -n EFI -i ABCD1234 /dev/vdb1",
            "mkdir -p /r/run/fspec/disk/efi",
            "mount /dev/vdb1 /r/run/fspec/disk/efi",
            "cryptsetup luksFormat --type luks2 --uuid cccc /dev/vdb2",
            "cryptsetup open /dev/vdb2 luks-cccc",
            "mkfs.btrfs -f -L root -U ffff /dev/mapper/luks-cccc",
            "mkdir -p /r/run/fspec/disk/top",
            "mount -o noatime,compress=zstd:1 /dev/mapper/luks-cccc /r/run/fspec/disk/top",
            "btrfs subvolume create /r/run/fspec/disk/top/@home",
            "btrfs subvolume create /r/run/fspec/disk/top/@swap",
            "btrfs subvolume create /r/run/fspec/disk/top/@snapshots",
            "btrfs subvolume create /r/run/fspec/disk/top/@fspec",
            "btrfs filesystem mkswapfile --size 4g /r/run/fspec/disk/top/@swap/swapfile",
            "bootctl --esp-path=/r/run/fspec/disk/efi install --no-variables",
            "umount /r/run/fspec/disk/efi",
            "umount /r/run/fspec/disk/top",
            "cryptsetup close --deferred luks-cccc",
            "umount /r/run/fspec/disk"
        ]
    );
    assert_eq!(crate::partition("/dev/nvme0n1", 2), "/dev/nvme0n1p2");
    let i = Init { passphrase: Some("hunter2"), ..i };
    let plan = init(&paths(), &i).unwrap();
    let lines = plan.lines();
    assert!(
        lines.contains(
            &"cryptsetup luksFormat --type luks2 --uuid cccc --batch-mode --key-file=- /dev/vdb2".to_string()
        )
    );
    assert!(lines.contains(&"cryptsetup open --key-file=- /dev/vdb2 luks-cccc".to_string()));
    assert_eq!(stdin_of(&plan, "cryptsetup open"), "hunter2");
    assert!(!lines.iter().any(|l| l.contains("hunter2")));
}

#[test]
fn init_with_a_distro_installs_the_first_root_onto_the_new_disk() {
    let ids = ids();
    let t = target("arch");
    let i = Init {
        device: "/dev/vdb",
        kind: Kind::Bios,
        luks: false,
        ids: &ids,
        swap_gib: 2,
        efi_vars: false,
        target: Some(&t),
        cmdline: "console=ttyS0",
        in_root: &[fspec_core::runner::Cmd::new("chroot").args(["/r/run/fspec/disk/arch", "chpasswd"])],
        passphrase: None,
    };
    let plan = init(&paths(), &i).unwrap();
    let lines = plan.lines();
    let at = |p: &str| position(&lines, p);
    assert_eq!(
        lines[5],
        "sgdisk -n1:0:+1M -t1:ef02 -c1:bios -n2:0:+2G -t2:8300 -c2:boot -n3:0:0 -t3:8300 -c3:root /dev/vdb"
    );
    assert!(
        at("mkfs.ext4 -F -L boot -U bbbb /dev/vdb2")
            < at("grub-install --target=i386-pc --boot-directory=/r/run/fspec/disk/boot /dev/vdb")
    );
    assert!(at("mount -o subvol=@arch UUID=ffff /r/run/fspec/disk/arch") < at("pacstrap -K /r/run/fspec/disk/arch"));
    assert!(lines.contains(&"mount --rbind /r/run/fspec/disk/top/@home /r/run/fspec/disk/arch/home".to_string()));
    assert_eq!(
        stdin_of(&plan, "tee /r/run/fspec/disk/boot/arch/fspec-cmdline"),
        "root=UUID=ffff rootflags=subvol=@arch rw console=ttyS0\n"
    );
    assert_eq!(crate::root_dir(&paths(), "arch"), "/r/run/fspec/disk/arch");
    assert!(at("chroot /r/run/fspec/disk/arch sh -c") < at("chroot /r/run/fspec/disk/arch chpasswd"));
    assert!(at("chroot /r/run/fspec/disk/arch chpasswd") < at("umount -R /r/run/fspec/disk/arch"));
    assert!(lines[lines.len() - 5].ends_with("sh /r/run/fspec/disk/boot bbbb arch"), "{lines:#?}");
    assert_eq!(
        lines[lines.len() - 4..],
        [
            "umount -R /r/run/fspec/disk/arch",
            "umount /r/run/fspec/disk/boot",
            "umount /r/run/fspec/disk/top",
            "umount /r/run/fspec/disk"
        ]
    );
}

/// The Arch cloud image the live tests start from: BIOS, root at the top level, an unused ESP.
fn arch_cloud() -> Machine {
    let grub = json!({"name": "/dev/vda1", "type": "part", "size": 1u64 << 20, "partuuid": "g",
        "parttype": "21686148-6449-6e6f-744e-656564454649", "partn": 1, "mountpoints": []});
    let l = lsblk("gpt", vec![grub, esp("/dev/vda2", 2, 300, &["/efi"]), btrfs_part("/dev/vda3", 3, &["/", "/home"])]);
    let mut f = facts("/", l, false);
    f.root_mount = "btrfs /dev/vda3 /".into();
    f.volumes = vec!["@home".into(), "swap".into()];
    f.fspec = String::new();
    classify(&f, "arch".into(), "console=ttyS0".into()).unwrap()
}

#[test]
fn migrate_moves_a_bios_cloud_image_onto_the_bios_layout() {
    let m = arch_cloud();
    let steps = migrate(&paths(), &m, &Leftovers::default(), &ids(), 4).unwrap();
    let what: Vec<&str> = steps.iter().map(|s| s.what.as_str()).collect();
    assert_eq!(
        what,
        [
            "create the shared subvolumes @swap, @snapshots, @fspec",
            "shrink the btrfs partition by 2 GiB and create the shared boot partition at /dev/vda4",
            "copy this root into @arch as a snapshot and set up its boot there",
        ]
    );
    assert_eq!(steps.iter().map(|s| s.destructive).collect::<Vec<_>>(), [false, true, false]);
    let carve = steps[1].plan.lines();
    let size = (40u64 << 30) - (2 << 30) - (2 << 20);
    assert_eq!(carve[3], format!("btrfs filesystem resize {} /r/", size - (1 << 20)));
    assert_eq!(carve[4], "sfdisk --no-reread -N 3 /dev/vda");
    assert_eq!(stdin_of(&steps[1].plan, "sfdisk"), format!(", {}\n", size / 512));
    let start = (4198400 + size / 512).div_ceil(2048) * 2048;
    assert!(carve.contains(&format!("sgdisk -n4:{start}:+2G -t4:8300 -c4:boot /dev/vda")), "{carve:#?}");
    assert!(carve.contains(&"partx -a -n 4:4 /dev/vda".to_string()));
    let root = steps[2].plan.lines();
    assert!(root.contains(&"btrfs subvolume snapshot /r/run/fspec/disk/top /r/run/fspec/disk/top/@arch".to_string()));
    assert!(root.contains(&"cp -a /r/run/fspec/disk/arch/boot/. /r/var/lib/fspec/boot/arch/".to_string()));
    assert!(
        root.contains(&"grub-install --target=i386-pc --boot-directory=/r/var/lib/fspec/boot /dev/vda".to_string())
    );
    assert_eq!(
        stdin_of(&steps[2].plan, "tee /r/var/lib/fspec/boot/arch/fspec-cmdline"),
        "root=UUID=1111-aaaa rootflags=subvol=@arch rw console=ttyS0\n"
    );
}

#[test]
fn migrate_finishes_a_partition_an_earlier_run_carved() {
    let mut m = arch_cloud();
    let made = json!({"name": "/dev/vda4", "type": "part", "size": 2u64 << 30, "partlabel": "boot", "partn": 4,
        "parttype": "0fc63daf-8483-4772-8e79-3d69d8477de4", "mountpoints": []});
    m.parts.push(crate::machine::part(&made));
    let steps = migrate(&paths(), &m, &Leftovers::default(), &ids(), 4).unwrap();
    assert_eq!(steps[1].what, "format the shared boot partition at /dev/vda4");
    let lines = steps[1].plan.lines();
    assert!(!lines.iter().any(|l| l.starts_with("btrfs filesystem resize") || l.starts_with("sgdisk")), "{lines:#?}");
    assert!(lines.contains(&"mkfs.ext4 -F -L boot -U bbbb /dev/vda4".to_string()));
}

#[test]
fn migrate_replaces_a_small_esp_and_keeps_its_identity() {
    let l = lsblk("gpt", vec![esp("/dev/vda1", 1, 100, &["/boot/efi"]), btrfs_part("/dev/vda2", 2, &["/", "/home"])]);
    let mut f = facts("/root", l, true);
    f.home = "/home".into();
    f.volumes = vec!["root".into(), "home".into()];
    f.fspec = String::new();
    let m = classify(&f, "fedora".into(), String::new()).unwrap();
    let steps = migrate(&paths(), &m, &Leftovers::default(), &ids(), 4).unwrap();
    assert_eq!(
        steps[1].what,
        "shrink the btrfs partition by 2 GiB and create a 2 GiB ESP at /dev/vda3 and move the 100 MiB ESP /dev/vda1 into it"
    );
    let lines = steps[1].plan.lines();
    let at = |p: &str| position(&lines, p);
    assert!(at("mkfs.vfat -F 32 -n EFI -i EEEE0001 /dev/vda3") < at("cp -a /r/boot/efi/. /r/run/fspec/disk/esp/"));
    assert!(at("fatlabel -r -i /dev/vda1") < at("sgdisk -u3:esp-guid /dev/vda"));
    assert!(at("sgdisk -u1:R -t1:8300 -c1:old-esp /dev/vda") < at("sgdisk -u3:esp-guid /dev/vda"));
    assert_eq!(lines[lines.len() - 2], "mount /dev/vda3 /r/efi");
    let root = steps[2].plan.lines();
    assert!(
        root.contains(&"btrfs subvolume snapshot /r/run/fspec/disk/top/root /r/run/fspec/disk/top/@fedora".to_string())
    );
    assert!(root.contains(&"bootctl --esp-path=/r/efi install".to_string()));
    assert!(stdin_of(&steps[2].plan, "tee /r/run/fspec/disk/fedora/etc/fstab").contains("subvol=home,compress=zstd"));
}

#[test]
fn migrate_offers_to_delete_what_the_move_left_behind() {
    let left = Leftovers { old_root: Some(String::new()), shadowed: vec!["/home".into()], ..Leftovers::default() };
    let steps = migrate(&paths(), &default_arch(), &left, &ids(), 4).unwrap();
    let what: Vec<(&str, bool)> = steps.iter().map(|s| (s.what.as_str(), s.destructive)).collect();
    assert_eq!(
        what,
        [
            ("delete the old root's files from the btrfs top level", true),
            ("delete the copies of /home left under their mounts in @arch", true),
        ]
    );
    let left = Leftovers { old_root: Some("root".into()), ..Leftovers::default() };
    let steps = migrate(&paths(), &default_arch(), &left, &ids(), 4).unwrap();
    assert!(
        steps[0].plan.lines().contains(&"btrfs subvolume delete --recursive /r/run/fspec/disk/top/root".to_string())
    );
    assert!(migrate(&paths(), &default_arch(), &Leftovers::default(), &ids(), 4).unwrap().is_empty());
}

#[test]
fn migrate_sends_ext4_to_a_live_system_and_refuses_luks_shrinks() {
    let mut m = arch_cloud();
    m.fstype = "ext4".into();
    let err = migrate(&paths(), &m, &Leftovers::default(), &ids(), 4).unwrap_err().to_string();
    assert!(err.contains("boot a live system and run `fspec disk migrate /dev/vda3`"), "{err}");
    let mut m = arch_cloud();
    m.luks = Some("cccc".into());
    let err = migrate(&paths(), &m, &Leftovers::default(), &ids(), 4).unwrap_err().to_string();
    assert!(err.contains("does not shrink a LUKS container"), "{err}");
}

#[test]
fn convert_keeps_the_uuid_and_fixes_the_fstab_type() {
    let plan = crate::convert(&paths(), "/dev/vdb1", "/dev/vdb", &Firmware::Efi);
    let lines = plan.lines();
    assert_eq!(lines[3], "e2fsck -fy /dev/vdb1");
    assert!(lines[4].starts_with("sh -c") && lines[4].contains("resize2fs"), "{}", lines[4]);
    assert_eq!(lines[5], "btrfs-convert --uuid copy /dev/vdb1");
    assert!(lines.iter().any(|l| l.starts_with("sed -i -e")));
    for (from, to) in [
        ("UUID=abc / ext4 rw,discard,errors=remount-ro 0 1\n", "UUID=abc / btrfs rw,discard 0 0\n"),
        ("UUID=abc\t/\text4\terrors=remount-ro,noatime\t0\t1\n", "UUID=abc\t/\tbtrfs\tnoatime\t0\t0\n"),
        ("UUID=E /boot/efi vfat defaults 0 1\n", "UUID=E /boot/efi vfat defaults 0 1\n"),
        ("UUID=h /home ext4 errors=remount-ro 0 2\n", "UUID=h /home ext4 errors=remount-ro 0 2\n"),
    ] {
        let mut sed = std::process::Command::new("sed");
        for e in crate::disk::FSTAB_SED {
            sed.args(["-e", e]);
        }
        let out = sed.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
        use std::io::Write;
        out.stdin.as_ref().unwrap().write_all(from.as_bytes()).unwrap();
        let out = out.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8(out.stdout).unwrap(), to);
    }
}

const NIXOS_UUID: &str = "1a10e345-3d66-4bde-9f6b-55d43e6f7e0d";

fn nixos_root() -> crate::NixosRoot {
    crate::NixosRoot {
        device: "/dev/nvme0n1p2".into(),
        generation: 426,
        system: "/nix/store/x58v-nixos-system-e1504g".into(),
        esp: "/dev/disk/by-uuid/ABCD-1234".into(),
        esp_at: "/boot".into(),
    }
}

#[test]
fn a_nixos_system_must_mount_its_root_from_at_nixos() {
    let ours = format!("/dev/disk/by-uuid/{NIXOS_UUID}");
    let fstab = format!(
        "# This is a generated file.\n{ours} / btrfs subvol=@nixos,noatime,compress=zstd:1 0 0\n/dev/disk/by-uuid/ABCD-1234 /boot vfat \
         fmask=0077,dmask=0077 0 2\n{ours} /home btrfs subvol=@home,noatime 0 0\n{ours} /swap btrfs subvol=@swap,noatime \
         0 0\n/swap/swapfile none swap defaults 0 0\n"
    );
    assert_eq!(
        crate::nixos_fstab(&fstab, NIXOS_UUID).unwrap(),
        ("/dev/disk/by-uuid/ABCD-1234".to_string(), "/boot".to_string())
    );
    let ext4 = format!("{ours} / ext4 defaults 0 1\n/dev/disk/by-uuid/ABCD-1234 /boot vfat defaults 0 2\n");
    let err = crate::nixos_fstab(&ext4, NIXOS_UUID).unwrap_err().to_string();
    assert!(err.contains("not btrfs subvol=@nixos"), "{err}");
    let other = fstab.replace(NIXOS_UUID, "0000");
    assert!(crate::nixos_fstab(&other, NIXOS_UUID).is_err());
    let home = fstab.replace("subvol=@home", "subvol=@other");
    let err = crate::nixos_fstab(&home, NIXOS_UUID).unwrap_err().to_string();
    assert!(err.contains("/home"), "{err}");
    let no_esp = format!("{ours} / btrfs subvol=/@nixos 0 0\n");
    assert!(crate::nixos_fstab(&no_esp, NIXOS_UUID).unwrap_err().to_string().contains("no vfat ESP"));
}

#[test]
fn store_symlinks_resolve_inside_the_mounted_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("nix/store/sys")).unwrap();
    std::fs::create_dir_all(root.join("nix/store/etc/etc")).unwrap();
    std::fs::write(root.join("nix/store/fstab"), "x").unwrap();
    std::os::unix::fs::symlink("/nix/store/etc/etc", root.join("nix/store/sys/etc")).unwrap();
    std::os::unix::fs::symlink("../../fstab", root.join("nix/store/etc/etc/fstab")).unwrap();
    let found = crate::disk::resolve_in(root, "/nix/store/sys/etc/fstab").unwrap();
    assert_eq!(found, root.join("nix/store/fstab"));
}

#[test]
fn a_nixos_root_converts_snapshots_and_boots_its_new_system() {
    let steps =
        crate::nixos_migrate(&paths(), &nixos_root(), "/r/run/fspec/esp", "/r/run/fspec/esp/fspec/disk-journal");
    let what: Vec<(&str, bool)> = steps.iter().map(|s| (s.what.as_str(), s.destructive)).collect();
    assert_eq!(
        what,
        [
            ("boot the fspec-live entry by default until the migration is done", false),
            (
                "convert the ext4 root /dev/nvme0n1p2 to btrfs in place, keeping its UUID and the ext4 image as \
                 ext2_saved for `btrfs-convert -r`",
                true
            ),
            (
                "snapshot the top level into @nixos, and create @home with a reflink copy of /home, @swap without \
                 copy-on-write, @snapshots and @fspec",
                false
            ),
            (
                "make /nix/store/x58v-nixos-system-e1504g generation 427 of NixOS in @nixos and its boot default with \
                 three tries, move generation 426 and older to loader/entries.ext4 on the ESP, and point the fspec-live \
                 entry at @fspec/live",
                false
            ),
        ]
    );
    let convert = steps[1].plan.lines();
    let at = |l: &[String], p: &str| position(l, p);
    assert!(at(&convert, "undo sh -c") < at(&convert, "btrfs-convert --uuid copy /dev/nvme0n1p2"));
    assert!(convert.iter().any(|l| l.contains("btrfs-convert -r")));
    let subvols = steps[2].plan.lines();
    let top = "/r/run/fspec/disk/top";
    assert!(
        at(&subvols, &format!("btrfs subvolume snapshot {top} {top}/@nixos"))
            < at(&subvols, &format!("btrfs subvolume create {top}/@home"))
    );
    assert!(subvols.contains(&format!("chattr +C {top}/@swap")));
    assert!(!subvols.iter().any(|l| l.contains("mkswapfile")));
    let boot = steps[3].plan.lines();
    let root = "/r/run/fspec/disk/nixos";
    assert!(boot.contains(&format!("mount -o subvol=@nixos,noatime,compress=zstd:1 /dev/nvme0n1p2 {root}")));
    assert!(at(&boot, "undo sh -c") < at(&boot, &format!("mount -t proc proc {root}/proc")));
    let sys = "/nix/store/x58v-nixos-system-e1504g";
    let env = format!("chroot {root} {sys}/sw/bin/env PATH={sys}/sw/bin HOME=/root LOCALE_ARCHIVE=");
    let find = |what: &str| boot.iter().position(|l| l.starts_with(&env) && l.contains(what)).unwrap();
    assert!(find("nix-env -p /nix/var/nix/profiles/system --set") < find("bin/switch-to-configuration boot"));
    assert!(boot.iter().any(|l| l.contains("init=$2/init") && l.ends_with("/nix/store/x58v-nixos-system-e1504g")));
}

/// Runs the ESP scripts of the NixOS boot step against a fake ESP. They use GNU sed.
#[cfg(target_os = "linux")]
#[test]
fn the_new_generation_gets_tries_and_the_live_entry_moves_into_at_fspec() {
    let dir = tempfile::tempdir().unwrap();
    let (esp, top) = (dir.path().join("esp"), dir.path().join("top"));
    let entries = esp.join("loader/entries");
    std::fs::create_dir_all(&entries).unwrap();
    std::fs::create_dir_all(top.join("@fspec/live")).unwrap();
    let w = |p: std::path::PathBuf, t: &str| std::fs::write(p, t).unwrap();
    let sys = "/nix/store/x58v-nixos-system-e1504g";
    w(entries.join("nixos-aaa.conf"), &format!("title NixOS\noptions init={sys}/init loglevel=4\n"));
    w(entries.join("nixos-bbb.conf"), "title NixOS\noptions init=/nix/store/old/init loglevel=4\n");
    let live = "title FormalSpec live\nlinux /fspec-live/vmlinuz\noptions boot=live findiso=/var/lib/fspec-live/formalspec-installer-amd64.iso fspec.overlay=/var/lib/fspec-live/overlay.tar quiet\n";
    w(entries.join("fspec-live+3.conf"), live);
    w(esp.join("loader/loader.conf"), "timeout 3\ndefault nixos-aaa.conf\nconsole-mode keep\n");
    let steps =
        crate::nixos_migrate(&paths(), &nixos_root(), "/r/run/fspec/esp", "/r/run/fspec/esp/fspec/disk-journal");
    let script = |marker: &str| {
        steps[3]
            .plan
            .steps
            .iter()
            .find_map(|s| match s {
                crate::Step::Run(c) if c.program == "sh" && c.args[1].contains(marker) => Some(c.args[1].clone()),
                _ => None,
            })
            .unwrap()
    };
    let run = |script: String, args: &[&std::path::Path]| {
        let out = std::process::Command::new("sh").arg("-c").arg(script).arg("sh").args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    let ls = |d: &std::path::Path| {
        let mut v: Vec<String> =
            std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    };
    run(script("panic_on_fail"), &[&esp, Path::new(sys)]);
    assert_eq!(ls(&entries), ["fspec-live+3.conf", "nixos-aaa+3.conf"]);
    assert_eq!(ls(&esp.join("loader/entries.ext4")), ["nixos-bbb.conf"]);
    let new = std::fs::read_to_string(entries.join("nixos-aaa+3.conf")).unwrap();
    assert!(
        new.contains(&format!(
            "options init={sys}/init loglevel=4 boot.panic_on_fail panic=10 systemd.mask=emergency.target \
         systemd.mask=emergency.service systemd.mask=rescue.target systemd.mask=rescue.service\n"
        )),
        "{new}"
    );
    assert_eq!(std::fs::read_to_string(esp.join("loader/loader.conf")).unwrap(), "timeout 3\nconsole-mode keep\n");

    let move_live = script("fspec-live*.conf");
    let err =
        std::process::Command::new("sh").arg("-c").arg(&move_live).arg("sh").arg(&esp).arg(&top).output().unwrap();
    assert!(!err.status.success() && String::from_utf8_lossy(&err.stderr).contains("not in @fspec/live"));
    assert_eq!(std::fs::read_to_string(entries.join("fspec-live+3.conf")).unwrap(), live);
    w(top.join("@fspec/live/formalspec-installer-amd64.iso"), "");
    w(top.join("@fspec/live/overlay.tar"), "");
    run(move_live, &[&esp, &top]);
    assert_eq!(
        std::fs::read_to_string(entries.join("fspec-live+3.conf")).unwrap(),
        live.replace("=/var/lib/fspec-live/", "=/@fspec/live/")
    );
    assert_eq!(ls(&entries), ["fspec-live+3.conf", "nixos-aaa+3.conf"]);
}

#[test]
fn a_failed_nixos_step_leaves_its_undo_to_the_journal() {
    let dir = tempfile::tempdir().unwrap();
    let steps =
        crate::nixos_migrate(&paths(), &nixos_root(), "/r/run/fspec/esp", "/r/run/fspec/esp/fspec/disk-journal");
    let runner = RecordingRunner::default();
    answer(&runner, "btrfs-convert --uuid", 1, "");
    assert!(steps[1].plan.execute(&runner).unwrap_err().to_string().contains("need a journal"));
    let mut journal = fspec_core::journal::Journal::begin(dir.path(), 0).unwrap();
    assert!(steps[1].plan.execute_journaled(&runner, &mut journal).is_err());
    let ran = runner.lines().len();
    journal.rollback(&runner).unwrap();
    let undo = &runner.lines()[ran..];
    assert_eq!(undo.len(), 1);
    assert!(undo[0].contains("btrfs-convert -r") && undo[0].ends_with("/r/run/fspec/disk /dev/nvme0n1p2"), "{undo:?}");
    assert!(!dir.path().join("journal.json").exists());
}

#[test]
fn nixos_on_btrfs_only_cleans_up_after_the_move() {
    let l =
        lsblk("gpt", vec![esp("/dev/vda1", 1, 1024, &["/boot"]), btrfs_part("/dev/vda2", 2, &["/", "/home", "/swap"])]);
    let mut f = facts("/@nixos", l, true);
    f.volumes.push("ext2_saved".into());
    let m = classify(&f, "nixos".into(), "init=/nix/store/x/init".into()).unwrap();
    assert!(migrate(&paths(), &m, &Leftovers::default(), &ids(), 4).unwrap().is_empty());
    let left = Leftovers { old_root: Some(String::new()), shadowed: vec!["/home".into()], old_swapfile: true };
    let steps = migrate(&paths(), &m, &left, &ids(), 4).unwrap();
    let what: Vec<(&str, bool)> = steps.iter().map(|s| (s.what.as_str(), s.destructive)).collect();
    assert_eq!(
        what,
        [
            (
                "delete the old root's files from the btrfs top level, and ext2_saved, after which `btrfs-convert -r` \
                 cannot go back to ext4",
                true
            ),
            ("delete the copies of /home left under their mounts in @nixos", true),
            ("delete the old /swapfile in @nixos", true),
        ]
    );
    assert!(steps[2].plan.lines().contains(&"rm -f /r/run/fspec/disk/top/@nixos/swapfile".to_string()));
    let guard = steps[0].plan.steps.iter().find_map(|s| match s {
        crate::Step::Expect { probe, ok: true, msg } if msg.contains("fspec-live") => Some(probe.to_string()),
        _ => None,
    });
    assert!(guard.unwrap().ends_with(" sh /r/boot"));
}

#[test]
fn rooted_runs_assume_the_default_layout() {
    let m = assumed("arch");
    assert!(matches!(m.layout, Layout::Default { .. }));
    assert_eq!(m.root, "/@arch");
}

#[test]
fn every_part_keeps_its_partition_number() {
    let m = default_arch();
    assert_eq!(m.parts.iter().map(|p: &Part| p.partn).collect::<Vec<_>>(), [1, 2]);
}

fn answer(r: &RecordingRunner, prefix: &str, status: i32, stdout: &str) {
    r.respond(prefix, Output { status, stdout: stdout.into(), stderr: String::new() });
}

#[test]
fn a_failing_step_stops_the_plan_and_reports_the_command() {
    let plan = switch_distro(&paths(), &default_arch(), Some("arch"), &target("void")).unwrap();
    let runner = RecordingRunner::default();
    answer(&runner, "test -e", 1, "");
    runner.respond("xbps-install", Output { status: 2, stdout: String::new(), stderr: "no space".into() });
    let err = plan.execute(&runner).unwrap_err();
    assert!(err.to_string().contains("xbps-install") && err.to_string().contains("no space"), "{err}");
    assert!(runner.lines().iter().all(|l| !l.starts_with("chroot")));
    assert_eq!(runner.lines().last().unwrap(), "umount /r/run/fspec/switch");
}

struct Missing<'a>(&'a RecordingRunner, &'static str);

impl fspec_core::runner::Runner for Missing<'_> {
    fn run(&self, cmd: &fspec_core::runner::Cmd) -> anyhow::Result<Output> {
        if cmd.program == self.1 {
            let err = std::io::Error::from(std::io::ErrorKind::NotFound);
            return Err(anyhow::Error::from(err).context(format!("running `{cmd}`")));
        }
        self.0.run(cmd)
    }
}

#[test]
fn a_machine_without_bootc_is_not_a_bootc_deployment() {
    let r = RecordingRunner::default();
    assert!(!crate::is_bootc(&Missing(&r, "bootc")).unwrap());
}

#[test]
fn reset_var_stages_files_and_writes_the_marker_last() {
    let persist = vec!["/var/lib/docker/".to_string(), "/etc/ssh".into(), "/etc/shadow".into(), "/home".into()];
    let rv = reset_var(&paths(), &persist).unwrap();
    assert_eq!(
        rv.plan.lines(),
        [
            "mkdir -p /r/var/lib/fspec/reset-var",
            "tee /r/var/lib/fspec/reset-var/keep",
            "tee /r/var/lib/fspec/reset-var/reset.sh",
            "chmod 755 /r/var/lib/fspec/reset-var/reset.sh",
            "tee /r/etc/systemd/system/fspec-reset-var.service",
            "systemctl enable fspec-reset-var.service",
            "tee /r/var/lib/fspec/reset-var/pending",
        ]
    );
    assert_eq!(
        rv.keep,
        [
            "/var/lib/fspec",
            "/opt/fspec",
            "/var/opt/fspec",
            "/home",
            "/var/home",
            "/var/roothome",
            "/etc/passwd",
            "/etc/shadow",
            "/etc/group",
            "/etc/gshadow",
            "/etc/subuid",
            "/etc/subgid",
            "/var/lib/docker",
            "/etc/ssh"
        ]
    );
    let crate::Step::Run(keep) = &rv.plan.steps[1] else { panic!() };
    assert_eq!(keep.stdin.as_deref().unwrap(), rv.keep.join("\n") + "\n");
    let crate::Step::Run(unit) = &rv.plan.steps[4] else { panic!() };
    let unit = unit.stdin.as_deref().unwrap();
    assert!(unit.contains("ConditionPathExists=/var/lib/fspec/reset-var/pending"));
    assert!(unit.contains("Type=oneshot") && unit.contains("Before=sysinit.target"));
}

#[test]
fn reset_var_rejects_relative_and_dotdot_paths() {
    assert!(reset_var(&paths(), &["etc/ssh".to_string()]).is_err());
    assert!(reset_var(&paths(), &["/etc/../root".to_string()]).is_err());
}

#[test]
// The script uses GNU `rm --one-file-system`; it only ever runs on bootc Linux hosts.
#[cfg(target_os = "linux")]
fn reset_script_keeps_listed_paths_and_their_parents() {
    let dir = tempfile::tempdir().unwrap();
    let t = dir.path().display().to_string();
    for d in ["var/lib/fspec", "var/lib/other", "var/cache", "etc/ssh", "etc/hosts.d", "usr/etc"] {
        std::fs::create_dir_all(format!("{t}/{d}")).unwrap();
    }
    for (f, body) in [
        ("var/lib/fspec/state", "s"),
        ("var/lib/other/x", "x"),
        ("etc/ssh/key", "k"),
        ("etc/local.conf", "l"),
        ("usr/etc/default.conf", "d"),
        ("pending", "1"),
        ("keep", &format!("{t}/var/lib/fspec\n{t}/etc/ssh\n")),
    ] {
        std::fs::write(format!("{t}/{f}"), body).unwrap();
    }
    let script = crate::bootc::SCRIPT
        .replace("dir=/var/lib/fspec/reset-var", &format!("dir={t}"))
        .replace("purge /var\n", &format!("purge {t}/var\n"))
        .replace("purge /etc\n", &format!("purge {t}/etc\n"))
        .replace("/usr/etc/. /etc/", &format!("{t}/usr/etc/. {t}/etc/"));
    let out = std::process::Command::new("sh").arg("-c").arg(&script).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let has = |p: &str| Path::new(&format!("{t}/{p}")).exists();
    assert!(has("var/lib/fspec/state") && has("etc/ssh/key"));
    assert!(!has("var/lib/other") && !has("var/cache") && !has("etc/local.conf") && !has("etc/hosts.d"));
    assert!(has("etc/default.conf") && !has("pending"));
}

#[test]
fn to_image_runs_bootc_from_the_image_itself() {
    let plan = to_image(&paths(), "ghcr.io/me/box@sha256:abc");
    assert_eq!(
        plan.lines(),
        ["podman run --rm --privileged --pid=host -v /dev:/dev -v /var/lib/containers:/var/lib/containers -v \
             /r:/target --security-opt label=type:unconfined_t ghcr.io/me/box@sha256:abc bootc install \
             to-existing-root --acknowledge-destructive"]
    );
}

#[test]
fn populate_adds_the_update_and_security_pockets() {
    let dir = tempfile::tempdir().unwrap();
    let list = dir.path().join("sources.list");
    for (distro, release, security) in [
        ("ubuntu", "deb http://archive.ubuntu.com/ubuntu noble main universe\n", "http://archive.ubuntu.com/ubuntu"),
        (
            "debian",
            "deb http://deb.debian.org/debian stable main contrib\n",
            "http://security.debian.org/debian-security",
        ),
    ] {
        std::fs::write(&list, release).unwrap();
        let script = crate::recipe::populate(distro, "x86_64", crate::recipe::Boot::Zbm, false);
        let sources =
            script.split("\nexport").next().unwrap().replace("/etc/apt/sources.list", &list.display().to_string());
        let out = std::process::Command::new("sh").arg("-c").arg(&sources).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let (_, mirror_suite) = release.trim_end().split_once(' ').unwrap();
        let (mirror, rest) = mirror_suite.split_once(' ').unwrap();
        let (suite, comps) = rest.split_once(' ').unwrap();
        assert_eq!(
            std::fs::read_to_string(&list).unwrap(),
            format!(
                "deb {mirror} {suite} {comps}\ndeb {mirror} {suite}-updates {comps}\ndeb {security} {suite}-security {comps}\n"
            )
        );
    }
}

#[test]
fn switching_to_aerynos_lets_moss_sync_boot_with_the_subvolume_in_the_options() {
    let plan = switch_distro(&paths(), &default_arch(), Some("arch"), &target("aerynos")).unwrap();
    let lines = plan.lines();
    let at = |p: &str| lines.iter().position(|l| l.contains(p)).unwrap_or_else(|| panic!("no `{p}` in {lines:#?}"));
    let root = "/r/run/fspec/switch/aerynos";
    assert!(at(&format!("mount --rbind /r/efi {root}/efi")) < at("sh -c 'command -v moss"));
    assert_eq!(lines.iter().filter(|l| l.starts_with("mount --rbind /r/efi")).count(), 1);
    let install = &lines[at(&format!("/r/efi/loader/loader.conf -D {root} -y install pkgset-aeryn-base"))];
    assert!(install.ends_with("linux-firmware btrfs-progs"), "{install}");
    assert!(
        stdin_of(&plan, &format!("tee {root}/etc/kernel/cmdline.d/fspec.cmdline"))
            .starts_with("root=UUID=1111-aaaa rootflags=subvol=@aerynos rw console=ttyS0,115200")
    );
    assert!(at(&format!("tee {root}/etc/kernel/cmdline.d/fspec.cmdline")) < at("-y install pkgset-aeryn-base"));
    assert!(at(&format!("{root}/sys/firmware/efi/efivars")) < at(&format!("chroot {root} /usr/local/bin/fspec")));
    assert!(!lines.iter().any(|l| l.contains("/etc/kernel/cmdline ")), "{lines:#?}");
}

#[test]
fn aerynos_needs_the_default_layout() {
    for (m, owner) in [(bios_debian(), "debian"), (zfs_debian(), "debian")] {
        let err = switch_distro(&paths(), &m, Some(owner), &target("aerynos")).unwrap_err().to_string();
        assert!(err.contains("aerynos boots only through systemd-boot from the default layout"), "{err}");
    }
}

#[test]
fn forgetting_aerynos_clears_the_kernels_blsforme_keeps() {
    let lines = forget(&paths(), &default_arch(), Some("arch"), "aerynos").unwrap().lines();
    assert!(lines.contains(&"rm -rf /r/efi/EFI/aerynos".to_string()), "{lines:#?}");
}

#[test]
fn the_boot_owner_reads_the_default_blsforme_writes() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().into(), dir.path().join("home"));
    std::fs::create_dir_all(dir.path().join("efi/loader")).unwrap();
    let owner = |conf: &str| {
        std::fs::write(dir.path().join("efi/loader/loader.conf"), conf).unwrap();
        crate::boot_owner(&RecordingRunner::default(), &paths, &default_arch()).unwrap()
    };
    assert_eq!(owner("timeout 3\ndefault fedora-*\n").as_deref(), Some("fedora"));
    assert_eq!(owner("default \"aerynos*\"\n").as_deref(), Some("aerynos"));
    assert_eq!(owner("timeout 3\n"), None);
}

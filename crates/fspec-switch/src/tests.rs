use std::path::Path;

use fspec_core::Paths;
use fspec_core::runner::{Output, RecordingRunner};

use crate::{
    BootMount, Bootloader, Firmware, Machine, Owner, Target, adopt_boot, assumed, detect, forget, reset_var,
    switch_distro, to_image,
};

const UUID: &str = "1111-aaaa";

fn paths() -> Paths {
    Paths::new("/r".into(), "/r/home/u".into())
}

fn mount(target: &str, uuid: &str, fstype: &str) -> BootMount {
    BootMount { target: target.into(), uuid: uuid.into(), fstype: fstype.into() }
}

fn machine(bootloader: Bootloader, boot_mounts: Vec<BootMount>, firmware: Firmware) -> Machine {
    Machine {
        uuid: UUID.into(),
        root_subvol: "/@".into(),
        home_subvol: "/@home".into(),
        bootloader,
        running: "fedora".into(),
        boot_mounts,
        firmware,
        cmdline: String::new(),
    }
}

fn systemd_boot() -> Machine {
    machine(
        Bootloader::SystemdBoot { esp: "/efi".into(), default: None },
        vec![mount("/efi", "EEEE-0001", "vfat")],
        Firmware::Efi,
    )
}

fn grub() -> Machine {
    machine(
        Bootloader::Grub { mkconfig: "grub-mkconfig", cfg: "/boot/grub/grub.cfg" },
        Vec::new(),
        Firmware::Bios { disk: "/dev/vda".into() },
    )
}

/// The running root `/@` owns boot.
fn owned() -> Owner {
    Owner { subvol: "/@".into(), distro: "fedora".into(), cmdline: String::new() }
}

fn target(distro: &str) -> Target<'_> {
    Target {
        distro,
        host: "box",
        arch: "x86_64",
        release: Some("42"),
        spec_dir: Path::new("/work/spec"),
        spec_file: "spec.star",
        exe: Path::new("/usr/bin/fspec"),
    }
}

/// Everything before the bootstrap command and everything between it and the boot entry.
fn expected(distro: &str, bootstrap: &str, boot: &[String]) -> Vec<String> {
    let new = format!("/r/run/fspec/switch/{distro}");
    let top = "/r/run/fspec/switch/top";
    let mut v = vec![
        format!("mkdir -p {top}"),
        format!("mount -o subvolid=5 UUID={UUID} {top}"),
        format!("test -e {top}/@{distro}"),
        format!("btrfs subvolume create {top}/@{distro}"),
        format!("mkdir -p {new}"),
        format!("mount -o subvol=@{distro} UUID={UUID} {new}"),
        bootstrap.to_string(),
        format!("cp -L /r/etc/resolv.conf {new}/etc/resolv.conf"),
        format!("tee {new}/etc/fstab"),
        format!("mount --rbind /r/home {new}/home"),
        format!("mount --make-rslave {new}/home"),
        format!("mount --rbind /r/dev {new}/dev"),
        format!("mount --make-rslave {new}/dev"),
        format!("mount --rbind /r/sys {new}/sys"),
        format!("mount --make-rslave {new}/sys"),
        format!("mount -t proc proc {new}/proc"),
        format!("mkdir -p {new}/var/lib/fspec/spec {new}/usr/local/bin"),
        format!("cp /usr/bin/fspec {new}/usr/local/bin/fspec"),
        format!("cp -a /work/spec/. {new}/var/lib/fspec/spec"),
        format!(
            "chroot {new} /usr/local/bin/fspec --spec /var/lib/fspec/spec/spec.star apply --local --scope system \
             --host box"
        ),
    ];
    v.extend(boot.iter().cloned());
    v.push(format!("umount -R {new}"));
    v.push(format!("umount {top}"));
    v
}

fn copy_kernel(from: &str, stage: &str, kernel: &str, initrd: &str) -> Vec<String> {
    vec![
        format!("mkdir -p {stage}"),
        format!("sh -c 'cp \"$(ls -1 $1 | sort -V | tail -n 1)\" \"$2\"' sh {from}/{kernel} {stage}/vmlinuz"),
        format!("sh -c 'cp \"$(ls -1 $1 | sort -V | tail -n 1)\" \"$2\"' sh {from}/{initrd} {stage}/initrd"),
    ]
}

fn sdboot_tail(distro: &str, kernel: &str, initrd: &str) -> Vec<String> {
    let from = format!("/r/run/fspec/switch/{distro}");
    let mut v = copy_kernel(&from, &format!("/r/efi/fspec/{distro}"), kernel, initrd);
    v.push("mkdir -p /r/efi/loader/entries".into());
    v.push(format!("tee /r/efi/loader/entries/fspec-{distro}.conf"));
    v
}

fn grub_tail(from: &str, distro: &str, kernel: &str, initrd: &str) -> Vec<String> {
    let mut v = copy_kernel(from, &format!("/r/boot/fspec/{distro}"), kernel, initrd);
    v.push(format!("tee /r/etc/grub.d/42_fspec_{distro}"));
    v.push(format!("chmod 755 /r/etc/grub.d/42_fspec_{distro}"));
    v.push("grub-mkconfig -o /r/boot/grub/grub.cfg".into());
    v
}

/// The bootstrap command, with the GRUB packages a BIOS machine needs for `--adopt-boot`.
fn bootstrap_line(distro: &str, grub: bool) -> String {
    let root = format!("/r/run/fspec/switch/{distro}");
    let pkgs = |p: &str| if grub { format!(" {p}") } else { String::new() };
    match distro {
        "fedora" => format!(
            "dnf --installroot {root} --releasever 42 -y --setopt=install_weak_deps=False install systemd dnf \
             fedora-release passwd btrfs-progs kernel{}",
            pkgs("grub2-tools grub2-pc")
        ),
        "arch" => format!("pacstrap -K {root} base linux linux-firmware btrfs-progs{}", pkgs("grub")),
        "debian" => format!(
            "debootstrap --arch amd64 --include=systemd-sysv,btrfs-progs,linux-image-amd64{} 42 {root}",
            if grub { ",grub-pc" } else { "" }
        ),
        _ => format!(
            "xbps-install -S -y -r {root} -R https://repo-default.voidlinux.org/current base-system linux btrfs-progs{}",
            pkgs("grub")
        ),
    }
}

fn globs(distro: &str) -> (&'static str, &'static str) {
    match distro {
        "arch" => ("boot/vmlinuz-linux", "boot/initramfs-linux.img"),
        "debian" => ("boot/vmlinuz-*", "boot/initrd.img-*"),
        _ => ("boot/vmlinuz-*", "boot/initramfs-*.img"),
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

#[test]
fn every_distro_on_both_bootloaders_plans_the_exact_sequence() {
    let o = owned();
    for distro in ["fedora", "arch", "debian", "void"] {
        let (k, i) = globs(distro);
        let plan = switch_distro(&paths(), &systemd_boot(), Some(&o), &target(distro)).unwrap();
        assert_eq!(
            plan.lines(),
            expected(distro, &bootstrap_line(distro, false), &sdboot_tail(distro, k, i)),
            "{distro} sdboot"
        );
        let plan = switch_distro(&paths(), &grub(), Some(&o), &target(distro)).unwrap();
        let from = format!("/r/run/fspec/switch/{distro}");
        assert_eq!(
            plan.lines(),
            expected(distro, &bootstrap_line(distro, true), &grub_tail(&from, distro, k, i)),
            "{distro} grub"
        );
    }
}

#[test]
fn efi_grub_bootstraps_the_efi_packages() {
    let mut m = grub();
    m.firmware = Firmware::Efi;
    let plan = switch_distro(&paths(), &m, Some(&owned()), &target("debian")).unwrap();
    assert!(plan.lines()[6].contains("linux-image-amd64,grub-efi-amd64 "), "{}", plan.lines()[6]);
    let plan = switch_distro(&paths(), &m, Some(&owned()), &target("arch")).unwrap();
    assert!(plan.lines()[6].ends_with(" grub efibootmgr"), "{}", plan.lines()[6]);
}

#[test]
fn the_first_switch_records_the_running_root_as_the_boot_owner() {
    let plan = switch_distro(&paths(), &grub(), None, &target("void")).unwrap();
    let marker = "tee /r/run/fspec/switch/top/.fspec-boot-owner";
    assert_eq!(plan.lines()[3], marker);
    assert_eq!(stdin_of(&plan, marker), "/@\n\n");
    let plan = switch_distro(&paths(), &grub(), Some(&owned()), &target("void")).unwrap();
    assert!(!plan.lines().iter().any(|l| l == marker));
}

#[test]
fn a_grub_switch_from_a_root_that_does_not_own_boot_is_refused() {
    let other = Owner { subvol: "/@arch".into(), distro: "arch".into(), cmdline: String::new() };
    let err = switch_distro(&paths(), &grub(), Some(&other), &target("void")).unwrap_err().to_string();
    assert!(err.contains("owned by the root /@arch") && err.contains("--adopt-boot` from /@ first"), "{err}");
    assert!(switch_distro(&paths(), &systemd_boot(), Some(&other), &target("void")).is_ok());
}

#[test]
fn void_bootstrap_sets_the_architecture() {
    let plan = switch_distro(&paths(), &systemd_boot(), Some(&owned()), &target("void")).unwrap();
    let crate::Step::Run(cmd) = &plan.steps[6] else { panic!("not a run step") };
    assert_eq!(cmd.env, [("XBPS_ARCH".to_string(), "x86_64".to_string())]);
    assert_eq!(cmd.stdin.as_deref(), Some("y\n"));
}

#[test]
fn the_chroot_apply_gets_a_path_with_sbin() {
    let plan = switch_distro(&paths(), &systemd_boot(), Some(&owned()), &target("debian")).unwrap();
    let chroot = plan.steps.iter().find_map(|s| match s {
        crate::Step::Run(c) if c.program == "chroot" => Some(c),
        _ => None,
    });
    let path = chroot.unwrap().env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.as_str());
    assert!(path.unwrap().split(':').any(|d| d == "/usr/sbin"));
}

#[test]
fn debian_defaults_to_stable_and_maps_the_architecture() {
    let mut t = target("debian");
    t.release = None;
    t.arch = "aarch64";
    let plan = switch_distro(&paths(), &systemd_boot(), Some(&owned()), &t).unwrap();
    assert_eq!(
        plan.lines()[6],
        "debootstrap --arch arm64 --include=systemd-sysv,btrfs-progs,linux-image-arm64 stable \
         /r/run/fspec/switch/debian"
    );
}

#[test]
fn fedora_without_a_release_is_refused_before_anything_is_planned() {
    let mut t = target("fedora");
    t.release = None;
    let err = switch_distro(&paths(), &systemd_boot(), Some(&owned()), &t).unwrap_err();
    assert!(err.to_string().contains("--release"), "{err}");
    assert!(switch_distro(&paths(), &systemd_boot(), Some(&owned()), &target("ubuntu")).is_err());
}

#[test]
fn fstab_shares_home_and_boot_and_the_boot_entries_name_the_new_subvolume() {
    let plan = switch_distro(&paths(), &systemd_boot(), Some(&owned()), &target("void")).unwrap();
    assert_eq!(
        stdin_of(&plan, "tee /r/run/fspec/switch/void/etc/fstab"),
        format!(
            "UUID={UUID} / btrfs subvol=@void,compress=zstd 0 0\nUUID={UUID} /home btrfs subvol=@home,compress=zstd 0 \
             0\nUUID=EEEE-0001 /efi vfat umask=0077 0 2\n"
        )
    );
    assert_eq!(
        stdin_of(&plan, "tee /r/efi/loader/entries/fspec-void.conf"),
        format!(
            "title Void (fspec)\nlinux /fspec/void/vmlinuz\ninitrd /fspec/void/initrd\noptions root=UUID={UUID} \
             rootflags=subvol=@void rw\n"
        )
    );

    let plan = switch_distro(&paths(), &grub(), Some(&owned()), &target("void")).unwrap();
    let script = stdin_of(&plan, "tee /r/etc/grub.d/42_fspec_void");
    assert!(script.starts_with("#!/bin/sh\nexec tail -n +3 $0\nmenuentry 'Void (fspec)'"));
    assert!(script.contains(&format!("search --no-floppy --fs-uuid --set=root {UUID}")));
    assert!(script.contains("linux /@/boot/fspec/void/vmlinuz root=UUID=1111-aaaa rootflags=subvol=@void rw"));
}

#[test]
fn grub_finds_kernels_on_a_separate_boot_and_at_the_top_level() {
    let mut m = grub();
    m.boot_mounts = vec![mount("/boot", "BBBB-0002", "ext4")];
    let plan = switch_distro(&paths(), &m, Some(&owned()), &target("void")).unwrap();
    assert!(
        stdin_of(&plan, "tee /r/run/fspec/switch/void/etc/fstab").ends_with("UUID=BBBB-0002 /boot ext4 defaults 0 2\n")
    );
    let script = stdin_of(&plan, "tee /r/etc/grub.d/42_fspec_void");
    assert!(script.contains("search --no-floppy --fs-uuid --set=root BBBB-0002"), "{script}");
    assert!(script.contains("linux /fspec/void/vmlinuz "), "{script}");

    let mut m = grub();
    m.root_subvol = "/".into();
    let top = Owner { subvol: "/".into(), distro: "arch".into(), cmdline: String::new() };
    let plan = switch_distro(&paths(), &m, Some(&top), &target("void")).unwrap();
    assert!(stdin_of(&plan, "tee /r/etc/grub.d/42_fspec_void").contains("linux /boot/fspec/void/vmlinuz "));
}

#[test]
fn an_existing_subvolume_stops_the_switch_and_still_unmounts() {
    let plan = switch_distro(&paths(), &systemd_boot(), Some(&owned()), &target("arch")).unwrap();
    let runner = RecordingRunner::default();
    runner.respond("test -e", Output::default());
    let err = plan.execute(&runner).unwrap_err();
    assert!(err.to_string().contains("fspec forget arch"), "{err}");
    let lines = runner.lines();
    assert!(!lines.iter().any(|l| l.starts_with("btrfs subvolume create")));
    assert_eq!(lines[lines.len() - 2..], ["umount -R /r/run/fspec/switch/arch", "umount /r/run/fspec/switch/top"]);
}

#[test]
fn a_failing_step_stops_the_plan_and_reports_the_command() {
    let plan = switch_distro(&paths(), &systemd_boot(), Some(&owned()), &target("arch")).unwrap();
    let runner = RecordingRunner::default();
    runner.respond("test -e", Output { status: 1, ..Output::default() });
    runner.respond("pacstrap", Output { status: 2, stdout: String::new(), stderr: "no space".into() });
    let err = plan.execute(&runner).unwrap_err();
    assert!(err.to_string().contains("pacstrap") && err.to_string().contains("no space"), "{err}");
    assert!(runner.lines().iter().all(|l| !l.starts_with("chroot")));
}

fn answer(r: &RecordingRunner, prefix: &str, status: i32, stdout: &str) {
    r.respond(prefix, Output { status, stdout: stdout.into(), stderr: String::new() });
}

fn btrfs_host(r: &RecordingRunner) {
    answer(r, "findmnt -no FSTYPE,UUID,FSROOT -T /r", 0, "btrfs 1111-aaaa /@\n");
    answer(r, "findmnt -no FSROOT -T /r/home", 0, "/@home\n");
}

#[test]
fn detect_reads_systemd_boot_from_the_esp() {
    let r = RecordingRunner::default();
    let dir = tempfile::tempdir().unwrap();
    let at = |p: &str| dir.path().join(p);
    std::fs::create_dir_all(at("etc")).unwrap();
    std::fs::create_dir_all(at("efi/EFI/systemd")).unwrap();
    std::fs::create_dir_all(at("efi/loader/entries")).unwrap();
    std::fs::write(at("etc/os-release"), "ID=fedora\n").unwrap();
    std::fs::write(at("efi/loader/loader.conf"), "timeout 3\ndefault fspec-void.conf\n").unwrap();
    std::fs::write(at("efi/loader/entries/fspec-void.conf"), "options rootflags=subvol=@void rw\n").unwrap();
    let p = Paths::new(dir.path().into(), at("home/u"));
    let root = dir.path().display().to_string();
    answer(&r, &format!("findmnt -no FSTYPE,UUID,FSROOT -T {root}"), 0, "btrfs 1111-aaaa /@\n");
    answer(&r, &format!("findmnt -no FSROOT -T {root}/home"), 0, "/@home\n");
    answer(&r, &format!("findmnt -no UUID,FSTYPE {root}/efi"), 0, "EEEE-0001 vfat\n");
    let m = detect(&r, &p).unwrap();
    assert_eq!(
        m,
        Machine {
            uuid: UUID.into(),
            root_subvol: "/@".into(),
            home_subvol: "/@home".into(),
            bootloader: Bootloader::SystemdBoot {
                esp: "/efi".into(),
                default: Some("options rootflags=subvol=@void rw\n".into())
            },
            running: "fedora".into(),
            boot_mounts: vec![mount("/efi", "EEEE-0001", "vfat")],
            firmware: Firmware::Efi,
            cmdline: String::new(),
        }
    );
}

#[test]
fn detect_falls_back_to_grub_and_its_fedora_spelling() {
    let r = RecordingRunner::default();
    btrfs_host(&r);
    answer(&r, "test -d /r/boot/grub2", 0, "");
    let m = detect(&r, &paths()).unwrap();
    assert_eq!(m.bootloader, Bootloader::Grub { mkconfig: "grub2-mkconfig", cfg: "/boot/grub2/grub.cfg" });

    let r = RecordingRunner::default();
    btrfs_host(&r);
    answer(&r, "test -d /r/boot/grub2", 1, "");
    answer(&r, "grub2-mkconfig", 1, "");
    let m = detect(&r, &paths()).unwrap();
    assert_eq!(m.bootloader, Bootloader::Grub { mkconfig: "grub-mkconfig", cfg: "/boot/grub/grub.cfg" });
}

/// Fails to spawn `program`, as the live runner does when it is not installed.
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
fn detect_finds_grub_in_a_root_that_never_installed_it_by_its_tools() {
    let r = RecordingRunner::default();
    btrfs_host(&r);
    answer(&r, "test -d /r/boot", 1, "");
    let m = detect(&Missing(&r, "grub2-mkconfig"), &paths()).unwrap();
    assert_eq!(m.bootloader, Bootloader::Grub { mkconfig: "grub-mkconfig", cfg: "/boot/grub/grub.cfg" });
}

#[test]
fn a_machine_without_bootc_is_not_a_bootc_deployment() {
    let r = RecordingRunner::default();
    assert!(!crate::is_bootc(&Missing(&r, "bootc")).unwrap());
}

#[test]
fn detect_reads_the_bios_boot_disk() {
    let r = RecordingRunner::default();
    btrfs_host(&r);
    answer(&r, "test -d /r/boot/grub2", 0, "");
    answer(&r, "test -d /r/sys/firmware/efi", 1, "");
    answer(&r, "findmnt -nvo SOURCE -T /r", 0, "/dev/vda3\n");
    answer(&r, "lsblk -ndpo PKNAME /dev/vda3", 0, "/dev/vda\n");
    assert_eq!(detect(&r, &paths()).unwrap().firmware, Firmware::Bios { disk: "/dev/vda".into() });
}

#[test]
fn detect_refuses_non_btrfs_roots_and_bad_layouts() {
    let r = RecordingRunner::default();
    answer(&r, "findmnt -no FSTYPE,UUID,FSROOT", 0, "ext4 2222-bbbb /\n");
    let err = detect(&r, &paths()).unwrap_err().to_string();
    assert!(err.contains("ext4, not btrfs"), "{err}");
    assert!(r.lines().len() == 1, "nothing else probed: {:?}", r.lines());

    let r = RecordingRunner::default();
    answer(&r, "findmnt -no FSTYPE,UUID,FSROOT", 0, "btrfs 1111-aaaa /@\n");
    answer(&r, "findmnt -no FSROOT", 0, "/@\n");
    let err = detect(&r, &paths()).unwrap_err().to_string();
    assert!(err.contains("/home is not its own btrfs subvolume"), "{err}");

    let r = RecordingRunner::default();
    btrfs_host(&r);
    answer(&r, "test -d", 1, "");
    answer(&r, "grub2-mkconfig", 1, "");
    answer(&r, "grub-mkconfig", 1, "");
    let err = detect(&r, &paths()).unwrap_err().to_string();
    assert!(err.contains("found neither systemd-boot"), "{err}");
}

#[test]
fn forget_deletes_the_subvolume_and_entry_on_each_bootloader() {
    let plan = forget(&paths(), &systemd_boot(), Some(&owned()), "void").unwrap();
    assert_eq!(
        plan.lines(),
        [
            "mkdir -p /r/run/fspec/switch/top",
            "mount -o subvolid=5 UUID=1111-aaaa /r/run/fspec/switch/top",
            "test -e /r/run/fspec/switch/top/@void",
            "rm -rf /r/efi/fspec/void",
            "rm -f /r/efi/loader/entries/fspec-void.conf",
            "btrfs subvolume delete --recursive /r/run/fspec/switch/top/@void",
            "umount /r/run/fspec/switch/top",
        ]
    );
    let plan = forget(&paths(), &grub(), Some(&owned()), "arch").unwrap();
    assert_eq!(
        plan.lines()[3..],
        [
            "rm -rf /r/boot/fspec/arch",
            "rm -f /r/etc/grub.d/42_fspec_arch",
            "grub-mkconfig -o /r/boot/grub/grub.cfg",
            r#"sh -c 'rm -rf "$1"/etc/grub.d/42_fspec_"$2" "$1"/*/etc/grub.d/42_fspec_"$2" "$1"/boot/fspec/"$2" "$1"/*/boot/fspec/"$2"' sh /r/run/fspec/switch/top arch"#,
            "btrfs subvolume delete --recursive /r/run/fspec/switch/top/@arch",
            "umount /r/run/fspec/switch/top",
        ]
    );
}

#[test]
fn forget_refuses_the_booted_root() {
    let err = forget(&paths(), &systemd_boot(), Some(&owned()), "fedora").unwrap_err().to_string();
    assert!(err.contains("fedora is the system you booted"), "{err}");

    let mut m = systemd_boot();
    m.running = String::new();
    m.root_subvol = "/@void".into();
    assert!(forget(&paths(), &m, Some(&owned()), "void").unwrap_err().to_string().contains("booted"));
    assert!(forget(&paths(), &m, Some(&owned()), "gentoo").unwrap_err().to_string().contains("not a distro"));
}

#[test]
fn forget_refuses_unless_this_root_owns_boot() {
    let other = Owner { subvol: "/@void".into(), distro: "void".into(), cmdline: String::new() };
    for m in [systemd_boot(), grub()] {
        let err = forget(&paths(), &m, Some(&other), "void").unwrap_err().to_string();
        assert!(err.contains("owned by the root /@void") && err.contains("--adopt-boot` from /@ first"), "{err}");
        let err = forget(&paths(), &m, None, "arch").unwrap_err().to_string();
        assert!(err.contains("no root is recorded as owning boot"), "{err}");
    }

    let mut m = systemd_boot();
    m.bootloader =
        Bootloader::SystemdBoot { esp: "/efi".into(), default: Some("options rootflags=subvol=@void\n".into()) };
    let err = forget(&paths(), &m, Some(&owned()), "void").unwrap_err().to_string();
    assert!(err.contains("default systemd-boot entry boots @void"), "{err}");
    assert!(forget(&paths(), &m, Some(&owned()), "arch").is_ok());
}

#[test]
fn forget_stops_when_the_subvolume_is_missing() {
    let plan = forget(&paths(), &systemd_boot(), Some(&owned()), "void").unwrap();
    let runner = RecordingRunner::default();
    runner.respond("test -e", Output { status: 1, ..Output::default() });
    let err = plan.execute(&runner).unwrap_err();
    assert!(err.to_string().contains("no @void subvolume"), "{err}");
    assert!(runner.lines().iter().all(|l| !l.starts_with("btrfs") && !l.starts_with("rm")));
}

/// Debian in `@debian`, switched to from the Arch root at the top level, which owns boot.
fn debian_on(m: Machine) -> (Machine, Owner) {
    let m = Machine { root_subvol: "/@debian".into(), running: "debian".into(), ..m };
    (m, Owner { subvol: "/".into(), distro: "arch".into(), cmdline: String::new() })
}

#[test]
fn adopt_boot_under_bios_grub_installs_grub_and_lists_the_old_root() {
    let (m, old) = debian_on(grub());
    let plan = adopt_boot(&paths(), &m, Some(&old), "debian", "x86_64").unwrap();
    let top = "/r/run/fspec/switch/top";
    let mut want = vec![
        format!("mkdir -p {top}"),
        format!("mount -o subvolid=5 UUID={UUID} {top}"),
        "grub-install --target=i386-pc /dev/vda".into(),
    ];
    want.extend(grub_tail(top, "arch", "boot/vmlinuz-linux", "boot/initramfs-linux.img"));
    want.push(format!("tee {top}/.fspec-boot-owner"));
    want.push(format!("umount {top}"));
    assert_eq!(plan.lines(), want);
    let script = stdin_of(&plan, "tee /r/etc/grub.d/42_fspec_arch");
    assert!(script.contains("linux /@debian/boot/fspec/arch/vmlinuz root=UUID=1111-aaaa rootflags=subvol=/ rw"));
    assert_eq!(stdin_of(&plan, "tee /r/run/fspec/switch/top/.fspec-boot-owner"), "/@debian\n\n");
}

#[test]
fn the_old_root_entry_keeps_its_recorded_kernel_options() {
    let (mut m, mut old) = debian_on(grub());
    m.cmdline = "BOOT_IMAGE=/@debian/boot/vmlinuz root=UUID=1111-aaaa rootflags=subvol=@debian rw".into();
    old.cmdline = "BOOT_IMAGE=/boot/vmlinuz-linux root=UUID=1111-aaaa ro rootflags=compress=zstd:1 net.ifnames=0 \
                   console=ttyS0,115200"
        .into();
    let plan = adopt_boot(&paths(), &m, Some(&old), "debian", "x86_64").unwrap();
    let script = stdin_of(&plan, "tee /r/etc/grub.d/42_fspec_arch");
    assert!(
        script
            .contains("root=UUID=1111-aaaa rootflags=subvol=/,compress=zstd:1 rw net.ifnames=0 console=ttyS0,115200\n")
    );
    assert_eq!(stdin_of(&plan, "tee /r/run/fspec/switch/top/.fspec-boot-owner"), format!("/@debian\n{}\n", m.cmdline));
}

#[test]
fn adopt_boot_under_efi_grub_installs_to_the_esp() {
    let (mut m, old) = debian_on(grub());
    m.firmware = Firmware::Efi;
    assert!(
        adopt_boot(&paths(), &m, Some(&old), "debian", "x86_64").unwrap_err().to_string().contains("no ESP is mounted")
    );
    m.boot_mounts = vec![mount("/boot/efi", "EEEE-0001", "vfat")];
    let plan = adopt_boot(&paths(), &m, Some(&old), "debian", "x86_64").unwrap();
    assert_eq!(
        plan.lines()[2],
        "grub-install --target=x86_64-efi --efi-directory=/r/boot/efi --bootloader-id=fspec-debian"
    );

    m.bootloader = Bootloader::Grub { mkconfig: "grub2-mkconfig", cfg: "/boot/grub2/grub.cfg" };
    let plan = adopt_boot(&paths(), &m, Some(&old), "debian", "aarch64").unwrap();
    assert!(plan.lines()[2].starts_with("grub2-install --target=arm64-efi") && plan.lines()[2].ends_with(" --force"));
}

#[test]
fn adopt_boot_by_the_owner_only_regenerates_its_config() {
    let plan = adopt_boot(&paths(), &grub(), Some(&owned()), "fedora", "x86_64").unwrap();
    assert_eq!(
        plan.lines()[2..4],
        ["grub-install --target=i386-pc /dev/vda", "grub-mkconfig -o /r/boot/grub/grub.cfg"]
    );
}

#[test]
fn adopt_boot_under_systemd_boot_makes_this_root_the_default() {
    let (m, old) = debian_on(systemd_boot());
    let plan = adopt_boot(&paths(), &m, Some(&old), "debian", "x86_64").unwrap();
    let lines = plan.lines();
    assert_eq!(
        lines[3],
        "sh -c 'cp \"$(ls -1 $1 | sort -V | tail -n 1)\" \"$2\"' sh /r/boot/vmlinuz-* /r/efi/fspec/debian/vmlinuz"
    );
    assert!(stdin_of(&plan, "tee /r/efi/loader/entries/fspec-debian.conf").contains("rootflags=subvol=@debian rw"));
    assert!(lines.iter().any(|l| l.contains("/r/efi/loader/loader.conf fspec-debian.conf")), "{lines:#?}");
    assert!(lines.iter().any(|l| l.contains("bootctl set-default") && l.ends_with("sh fspec-debian.conf")));
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
fn rooted_runs_assume_systemd_boot_on_btrfs() {
    let m = assumed();
    assert!(matches!(m.bootloader, Bootloader::SystemdBoot { .. }));
    let own = Owner { subvol: m.root_subvol.clone(), distro: String::new(), cmdline: String::new() };
    assert!(forget(&paths(), &m, Some(&own), "void").is_ok());
}

#[test]
fn boot_owner_reads_the_subvolume_and_the_kernel_options() {
    let r = RecordingRunner::default();
    let top = "/r/run/fspec/switch/owner";
    answer(&r, &format!("cat {top}/.fspec-boot-owner"), 0, "/\nroot=UUID=1111-aaaa net.ifnames=0\n");
    answer(&r, &format!("cat {top}/etc/os-release"), 0, "ID=arch\n");
    let owner = crate::boot_owner(&r, &paths(), &grub()).unwrap().unwrap();
    assert_eq!(
        owner,
        Owner { subvol: "/".into(), distro: "arch".into(), cmdline: "root=UUID=1111-aaaa net.ifnames=0".into() }
    );
}

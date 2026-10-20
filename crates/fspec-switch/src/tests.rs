use std::path::Path;

use fspec_core::Paths;
use fspec_core::runner::{Output, RecordingRunner};

use crate::{Bootloader, Machine, Target, assumed, detect, forget, reset_var, switch_distro, to_image};

const UUID: &str = "1111-aaaa";

fn paths() -> Paths {
    Paths::new("/r".into(), "/r/home/u".into())
}

fn machine(bootloader: Bootloader) -> Machine {
    Machine {
        uuid: UUID.into(),
        root_subvol: "/@".into(),
        home_subvol: "/@home".into(),
        bootloader,
        running: "fedora".into(),
        boot_prefix: "/@".into(),
    }
}

fn systemd_boot() -> Machine {
    machine(Bootloader::SystemdBoot { esp: "/efi".into() })
}

fn grub() -> Machine {
    machine(Bootloader::Grub { mkconfig: "grub-mkconfig", cfg: "/boot/grub/grub.cfg" })
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

fn copy_kernel(distro: &str, stage: &str, kernel: &str, initrd: &str) -> Vec<String> {
    let new = format!("/r/run/fspec/switch/{distro}");
    vec![
        format!("mkdir -p {stage}"),
        format!("sh -c 'cp \"$(ls -1 $1 | sort -V | tail -n 1)\" \"$2\"' sh {new}/{kernel} {stage}/vmlinuz"),
        format!("sh -c 'cp \"$(ls -1 $1 | sort -V | tail -n 1)\" \"$2\"' sh {new}/{initrd} {stage}/initrd"),
    ]
}

fn sdboot_tail(distro: &str, kernel: &str, initrd: &str) -> Vec<String> {
    let mut v = copy_kernel(distro, &format!("/r/efi/fspec/{distro}"), kernel, initrd);
    v.push("mkdir -p /r/efi/loader/entries".into());
    v.push(format!("tee /r/efi/loader/entries/fspec-{distro}.conf"));
    v
}

fn grub_tail(distro: &str, kernel: &str, initrd: &str) -> Vec<String> {
    let mut v = copy_kernel(distro, &format!("/r/boot/fspec/{distro}"), kernel, initrd);
    v.push(format!("tee /r/etc/grub.d/42_fspec_{distro}"));
    v.push(format!("chmod 755 /r/etc/grub.d/42_fspec_{distro}"));
    v.push("grub-mkconfig -o /r/boot/grub/grub.cfg".into());
    v
}

fn bootstrap_line(distro: &str) -> String {
    let root = format!("/r/run/fspec/switch/{distro}");
    match distro {
        "fedora" => format!(
            "dnf --installroot {root} --releasever 42 -y --setopt=install_weak_deps=False install systemd dnf \
             fedora-release passwd btrfs-progs kernel"
        ),
        "arch" => format!("pacstrap -K {root} base linux linux-firmware btrfs-progs"),
        "debian" => format!("debootstrap --arch amd64 --include=systemd-sysv,btrfs-progs,linux-image-amd64 42 {root}"),
        _ => format!(
            "xbps-install -S -y -r {root} -R https://repo-default.voidlinux.org/current base-system linux btrfs-progs"
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

#[test]
fn every_distro_on_both_bootloaders_plans_the_exact_sequence() {
    for distro in ["fedora", "arch", "debian", "void"] {
        let (k, i) = globs(distro);
        let plan = switch_distro(&paths(), &systemd_boot(), &target(distro)).unwrap();
        assert_eq!(
            plan.lines(),
            expected(distro, &bootstrap_line(distro), &sdboot_tail(distro, k, i)),
            "{distro} sdboot"
        );
        let plan = switch_distro(&paths(), &grub(), &target(distro)).unwrap();
        assert_eq!(plan.lines(), expected(distro, &bootstrap_line(distro), &grub_tail(distro, k, i)), "{distro} grub");
    }
}

#[test]
fn void_bootstrap_sets_the_architecture() {
    let plan = switch_distro(&paths(), &systemd_boot(), &target("void")).unwrap();
    let crate::Step::Run(cmd) = &plan.steps[6] else { panic!("not a run step") };
    assert_eq!(cmd.env, [("XBPS_ARCH".to_string(), "x86_64".to_string())]);
}

#[test]
fn debian_defaults_to_stable_and_maps_the_architecture() {
    let mut t = target("debian");
    t.release = None;
    t.arch = "aarch64";
    let plan = switch_distro(&paths(), &systemd_boot(), &t).unwrap();
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
    let err = switch_distro(&paths(), &systemd_boot(), &t).unwrap_err();
    assert!(err.to_string().contains("--release"), "{err}");
    assert!(switch_distro(&paths(), &systemd_boot(), &target("ubuntu")).is_err());
}

#[test]
fn fstab_shares_home_and_the_boot_entries_name_the_new_subvolume() {
    let plan = switch_distro(&paths(), &systemd_boot(), &target("void")).unwrap();
    let stdin = |prefix: &str| {
        plan.steps
            .iter()
            .find_map(|s| match s {
                crate::Step::Run(c) if c.to_string().starts_with(prefix) => c.stdin.clone(),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(
        stdin("tee /r/run/fspec/switch/void/etc/fstab"),
        format!(
            "UUID={UUID} / btrfs subvol=@void,compress=zstd 0 0\nUUID={UUID} /home btrfs subvol=@home,compress=zstd 0 0\n"
        )
    );
    assert_eq!(
        stdin("tee /r/efi/loader/entries/fspec-void.conf"),
        format!(
            "title Void (fspec)\nlinux /fspec/void/vmlinuz\ninitrd /fspec/void/initrd\noptions root=UUID={UUID} \
             rootflags=subvol=@void rw\n"
        )
    );

    let plan = switch_distro(&paths(), &grub(), &target("void")).unwrap();
    let script = plan
        .steps
        .iter()
        .find_map(|s| match s {
            crate::Step::Run(c) if c.program == "tee" && c.args[0].contains("grub.d") => c.stdin.clone(),
            _ => None,
        })
        .unwrap();
    assert!(script.starts_with("#!/bin/sh\nexec tail -n +3 $0\nmenuentry 'Void (fspec)'"));
    assert!(script.contains(&format!("search --no-floppy --fs-uuid --set=root {UUID}")));
    assert!(script.contains("linux /@/boot/fspec/void/vmlinuz root=UUID=1111-aaaa rootflags=subvol=@void rw"));
}

#[test]
fn an_existing_subvolume_stops_the_switch_and_still_unmounts() {
    let plan = switch_distro(&paths(), &systemd_boot(), &target("arch")).unwrap();
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
    let plan = switch_distro(&paths(), &systemd_boot(), &target("arch")).unwrap();
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
fn detect_reads_systemd_boot() {
    let r = RecordingRunner::default();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("etc")).unwrap();
    std::fs::write(dir.path().join("etc/os-release"), "ID=fedora\n").unwrap();
    let p = Paths::new(dir.path().into(), dir.path().join("home/u"));
    let root = dir.path().display().to_string();
    answer(&r, &format!("findmnt -no FSTYPE,UUID,FSROOT -T {root}"), 0, "btrfs 1111-aaaa /@\n");
    answer(&r, &format!("findmnt -no FSROOT -T {root}/home"), 0, "/@home\n");
    answer(&r, "bootctl --print-esp-path", 0, &format!("{root}/efi\n"));
    let m = detect(&r, &p).unwrap();
    assert_eq!(
        m,
        Machine {
            uuid: UUID.into(),
            root_subvol: "/@".into(),
            home_subvol: "/@home".into(),
            bootloader: Bootloader::SystemdBoot { esp: "/efi".into() },
            running: "fedora".into(),
            boot_prefix: String::new(),
        }
    );
}

#[test]
fn detect_falls_back_to_grub_and_its_fedora_spelling() {
    let r = RecordingRunner::default();
    btrfs_host(&r);
    answer(&r, "bootctl is-installed", 1, "");
    answer(&r, "test -d /r/boot/grub2", 0, "");
    answer(&r, "findmnt -no TARGET -T /r/boot", 0, "/r\n");
    let m = detect(&r, &paths()).unwrap();
    assert_eq!(m.bootloader, Bootloader::Grub { mkconfig: "grub2-mkconfig", cfg: "/boot/grub2/grub.cfg" });
    assert_eq!(m.boot_prefix, "/@");

    let r = RecordingRunner::default();
    btrfs_host(&r);
    answer(&r, "bootctl is-installed", 1, "");
    answer(&r, "test -d /r/boot/grub2", 1, "");
    answer(&r, "findmnt -no TARGET -T /r/boot", 0, "/r\n");
    let m = detect(&r, &paths()).unwrap();
    assert_eq!(m.bootloader, Bootloader::Grub { mkconfig: "grub-mkconfig", cfg: "/boot/grub/grub.cfg" });
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
    answer(&r, "bootctl is-installed", 1, "");
    answer(&r, "test -d", 1, "");
    let err = detect(&r, &paths()).unwrap_err().to_string();
    assert!(err.contains("found neither systemd-boot"), "{err}");

    let r = RecordingRunner::default();
    btrfs_host(&r);
    answer(&r, "bootctl is-installed", 1, "");
    answer(&r, "test -d /r/boot/grub2", 0, "");
    answer(&r, "findmnt -no TARGET", 0, "/r/boot\n");
    assert!(detect(&r, &paths()).unwrap_err().to_string().contains("/boot is a separate filesystem"));
}

#[test]
fn forget_deletes_the_subvolume_and_entry_on_each_bootloader() {
    let plan = forget(&paths(), &systemd_boot(), "void").unwrap();
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
    let plan = forget(&paths(), &grub(), "arch").unwrap();
    assert_eq!(
        plan.lines()[3..],
        [
            "rm -rf /r/boot/fspec/arch",
            "rm -f /r/etc/grub.d/42_fspec_arch",
            "grub-mkconfig -o /r/boot/grub/grub.cfg",
            "btrfs subvolume delete --recursive /r/run/fspec/switch/top/@arch",
            "umount /r/run/fspec/switch/top",
        ]
    );
}

#[test]
fn forget_refuses_the_booted_root() {
    let err = forget(&paths(), &systemd_boot(), "fedora").unwrap_err().to_string();
    assert!(err.contains("fedora is the system you booted"), "{err}");

    let mut m = systemd_boot();
    m.running = String::new();
    m.root_subvol = "/@void".into();
    assert!(forget(&paths(), &m, "void").unwrap_err().to_string().contains("booted"));
    assert!(forget(&paths(), &m, "gentoo").unwrap_err().to_string().contains("not a distro"));
}

#[test]
fn forget_stops_when_the_subvolume_is_missing() {
    let plan = forget(&paths(), &systemd_boot(), "void").unwrap();
    let runner = RecordingRunner::default();
    runner.respond("test -e", Output { status: 1, ..Output::default() });
    let err = plan.execute(&runner).unwrap_err();
    assert!(err.to_string().contains("no @void subvolume"), "{err}");
    assert!(runner.lines().iter().all(|l| !l.starts_with("btrfs") && !l.starts_with("rm")));
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
    assert!(forget(&paths(), &m, "void").is_ok());
}

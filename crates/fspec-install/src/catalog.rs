//! The distros the installer offers, in the order the picker lists them.

use crate::model::Delivery::{Atomic, Classic, Image};
use crate::model::Desktop::{Cosmic, Gnome, Kde};
use crate::model::Init::{Runit, Systemd};
use crate::model::Preset::{Desktop, Gaming, Minimal, Server};
use crate::model::Release::{Fixed, Rolling};
use crate::model::{Distro, Preset};

const ALL: &[Preset] = &[Minimal, Desktop, Gaming, Server];

pub const DISTROS: [Distro; 11] = [
    Distro {
        id: "fedora",
        name: "Fedora",
        reason: "Current software every six months, with a signed kernel that Secure Boot and anti-cheat trust.",
        release: Fixed,
        init: Systemd,
        signed_kernel: true,
        delivery: Classic,
        based_on: None,
        alpha: false,
        needs_efi: false,
        presets: ALL,
        desktops: &[Gnome, Kde],
    },
    Distro {
        id: "bazzite",
        name: "Bazzite",
        reason: "A gaming image built on Fedora, with Steam, HDR and handheld support ready on first boot.",
        release: Fixed,
        init: Systemd,
        signed_kernel: true,
        delivery: Image,
        based_on: Some("Fedora"),
        alpha: false,
        needs_efi: true,
        presets: &[Desktop, Gaming],
        desktops: &[Kde, Gnome],
    },
    Distro {
        id: "fedora-atomic",
        name: "Fedora Atomic",
        reason: "Fedora as an image: updates land whole and the previous one stays in the boot menu.",
        release: Fixed,
        init: Systemd,
        signed_kernel: true,
        delivery: Image,
        based_on: Some("Fedora"),
        alpha: false,
        needs_efi: true,
        presets: &[Desktop],
        desktops: &[Gnome, Kde],
    },
    Distro {
        id: "aurora",
        name: "Aurora",
        reason: "A KDE image from Universal Blue, tuned for daily work and development.",
        release: Fixed,
        init: Systemd,
        signed_kernel: true,
        delivery: Image,
        based_on: Some("Fedora"),
        alpha: false,
        needs_efi: true,
        presets: &[Desktop],
        desktops: &[Kde],
    },
    Distro {
        id: "arch",
        name: "Arch Linux",
        reason: "The newest packages the week they ship, and only what you ask for.",
        release: Rolling,
        init: Systemd,
        signed_kernel: false,
        delivery: Classic,
        based_on: None,
        alpha: false,
        needs_efi: false,
        presets: ALL,
        desktops: &[Kde, Gnome],
    },
    Distro {
        id: "cachyos",
        name: "CachyOS",
        reason: "Arch with packages built for current CPUs and a kernel tuned for desktops and games.",
        release: Rolling,
        init: Systemd,
        signed_kernel: false,
        delivery: Classic,
        based_on: Some("Arch Linux"),
        alpha: false,
        needs_efi: false,
        presets: ALL,
        desktops: &[Kde, Gnome],
    },
    Distro {
        id: "ubuntu",
        name: "Ubuntu",
        reason: "Long-term support and the broadest hardware and vendor support.",
        release: Fixed,
        init: Systemd,
        signed_kernel: true,
        delivery: Classic,
        based_on: Some("Debian"),
        alpha: false,
        needs_efi: false,
        presets: ALL,
        desktops: &[Gnome, Kde],
    },
    Distro {
        id: "debian",
        name: "Debian",
        reason: "Stable for years at a time. Nothing changes until you decide to upgrade.",
        release: Fixed,
        init: Systemd,
        signed_kernel: true,
        delivery: Classic,
        based_on: None,
        alpha: false,
        needs_efi: false,
        presets: ALL,
        desktops: &[Gnome, Kde],
    },
    Distro {
        id: "void",
        name: "Void Linux",
        reason: "An independent rolling distro with runit and a package manager that stays out of the way.",
        release: Rolling,
        init: Runit,
        signed_kernel: false,
        delivery: Classic,
        based_on: None,
        alpha: false,
        needs_efi: false,
        presets: ALL,
        desktops: &[Kde, Gnome],
    },
    Distro {
        id: "void-musl",
        name: "Void Linux (musl)",
        reason: "Void built on musl instead of glibc: smaller and simpler, but some prebuilt apps will not run.",
        release: Rolling,
        init: Runit,
        signed_kernel: false,
        delivery: Classic,
        based_on: None,
        alpha: false,
        needs_efi: false,
        presets: &[Minimal, Desktop, Server],
        desktops: &[Kde, Gnome],
    },
    Distro {
        id: "aerynos",
        name: "AerynOS",
        reason: "Built from scratch by its own tooling. Moss switches each update in atomically, and you can boot back into an earlier state.",
        release: Rolling,
        init: Systemd,
        signed_kernel: false,
        delivery: Atomic,
        based_on: None,
        alpha: true,
        needs_efi: true,
        presets: &[Desktop],
        desktops: &[Gnome, Kde, Cosmic],
    },
];

pub fn distro(id: &str) -> Option<&'static Distro> {
    DISTROS.iter().find(|d| d.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_every_distro_has_a_preset() {
        for (i, d) in DISTROS.iter().enumerate() {
            assert!(DISTROS[..i].iter().all(|o| o.id != d.id), "{} listed twice", d.id);
            assert!(!d.presets.is_empty(), "{} has no preset", d.id);
            let needs_desktop = d.presets.iter().any(|p| matches!(p, Desktop | Gaming));
            assert_eq!(needs_desktop, !d.desktops.is_empty(), "{} desktops", d.id);
        }
    }
}

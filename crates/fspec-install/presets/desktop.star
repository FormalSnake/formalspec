# GNOME or KDE Plasma with a browser, audio, printing and Flatpak.

load("//modules/minimal.star", base_modules = "modules", base_system = "system", family = "family")

# Aeon and Kalpa are one desktop each, defined by their base pattern, which brings the desktop,
# audio, printing and Flatpak with Flathub.
TRANSACTIONAL = {
    "aeon": {"gnome": "patterns-aeon-base"},
    "kalpa-desktop": {"kde": "patterns-kalpa-base"},
}

# The image already carries the desktop, audio, printing and Flatpak.
IMAGES = {
    "bazzite": {"kde": "ghcr.io/ublue-os/bazzite:stable", "gnome": "ghcr.io/ublue-os/bazzite-gnome:stable"},
    "fedora-atomic": {"gnome": "quay.io/fedora/fedora-silverblue:44", "kde": "quay.io/fedora/fedora-kinoite:44"},
    "aurora": {"kde": "ghcr.io/ublue-os/aurora:stable"},
}

GNOME = {
    "fedora": ["gdm", "gnome-shell", "gnome-session-wayland-session", "nautilus", "gnome-control-center", "ptyxis", "gnome-text-editor", "xdg-desktop-portal-gnome"],
    "arch": ["gdm", "gnome-shell", "nautilus", "gnome-control-center", "gnome-console", "gnome-text-editor", "xdg-desktop-portal-gnome", "gnome-keyring"],
    "debian": ["gdm3", "gnome-shell", "gnome-session", "nautilus", "gnome-control-center", "gnome-console", "gnome-text-editor", "xdg-desktop-portal-gnome", "gnome-keyring"],
    "ubuntu": ["gdm3", "gnome-shell", "gnome-session", "nautilus", "gnome-control-center", "gnome-console", "gnome-text-editor", "xdg-desktop-portal-gnome", "gnome-keyring"],
    "void": ["gnome-core", "gdm"],
    "aerynos": ["pkgset-aeryn-gnome-recommended", "gdm"],
    # gdm.service lives in gdm-systemd, which gdm only suggests.
    "opensuse": ["patterns-gnome-gnome_basic", "gdm", "gdm-systemd"],
}

KDE = {
    "fedora": ["plasma-desktop", "sddm", "konsole", "dolphin", "plasma-nm", "plasma-pa", "kscreen", "xdg-desktop-portal-kde"],
    "arch": ["plasma-desktop", "sddm", "konsole", "dolphin", "plasma-nm", "plasma-pa", "kscreen", "xdg-desktop-portal-kde"],
    "debian": ["kde-plasma-desktop", "sddm"],
    "ubuntu": ["kde-plasma-desktop", "sddm"],
    "void": ["kde-plasma", "sddm", "konsole", "dolphin"],
    "aerynos": ["pkgset-aeryn-plasma-recommended", "sddm"],
    "opensuse": ["patterns-kde-kde_plasma", "sddm-qt6"],
}

# Only AerynOS offers COSMIC here.
COSMIC = {
    "aerynos": ["pkgset-aeryn-cosmic-recommended", "cosmic-greeter"],
}

DESKTOPS = {"gnome": GNOME, "kde": KDE, "cosmic": COSMIC}

# The unit name differs from the package on Debian and Ubuntu.
DISPLAY_MANAGER = {
    "gnome": {"debian": "gdm3", "ubuntu": "gdm3"},
    "kde": {},
    "cosmic": {},
}
DEFAULT_MANAGER = {"gnome": "gdm", "kde": "sddm", "cosmic": "cosmic-greeter"}

AUDIO = {
    "fedora": ["pipewire", "pipewire-pulseaudio", "wireplumber"],
    "arch": ["pipewire", "pipewire-pulse", "pipewire-alsa", "wireplumber"],
    "debian": ["pipewire", "pipewire-pulse", "pipewire-alsa", "wireplumber"],
    "ubuntu": ["pipewire", "pipewire-pulse", "pipewire-alsa", "wireplumber"],
    "void": ["pipewire", "wireplumber"],
    # pkgset-aeryn-base-desktop, which every desktop pkgset pulls in, carries PipeWire.
    "aerynos": [],
    "opensuse": ["pipewire", "pipewire-pulseaudio", "pipewire-alsa", "wireplumber"],
}

# Fonts the desktop packages do not pull in, and the session manager Void's GNOME and KDE need.
EXTRA = {
    "arch": ["noto-fonts"],
    "void": ["elogind", "noto-fonts-ttf"],
}

PRINTING = ["cups"]
FLATPAK = ["flatpak"]

# Void starts neither PipeWire nor its session manager on its own.
# https://docs.voidlinux.org/config/media/pipewire.html
VOID_AUDIO = [
    file(
        "/etc/xdg/autostart/pipewire.desktop",
        content = "[Desktop Entry]\nName=PipeWire\nComment=Start PipeWire\nIcon=pipewire\nExec=pipewire\nTerminal=false\nType=Application\nNoDisplay=true\n",
    ),
    file(
        "/etc/pipewire/pipewire.conf.d/10-wireplumber.conf",
        content = "context.exec = [ { path = \"/usr/bin/wireplumber\" args = \"\" } ]\n",
    ),
    file(
        "/etc/pipewire/pipewire.conf.d/20-pipewire-pulse.conf",
        content = "context.exec = [ { path = \"/usr/bin/pipewire\" args = \"-c pipewire-pulse.conf\" } ]\n",
    ),
]

def chosen(desktop):
    if desktop not in DESKTOPS:
        fail("desktop is \"gnome\", \"kde\" or \"cosmic\"")
    return DESKTOPS[desktop]

def system(distro, desktop = None, extra = []):
    de = chosen(desktop)
    if distro in IMAGES:
        return image(base = IMAGES[distro][desktop])
    if distro == "aeon":
        return aeon(packages = [TRANSACTIONAL[distro][desktop]])
    if distro == "kalpa-desktop":
        return kalpa(packages = [TRANSACTIONAL[distro][desktop]])
    f = family(distro)
    return base_system(distro, extra = de[f] + AUDIO[f] + PRINTING + FLATPAK + EXTRA.get(f, []) + extra)

def modules(distro, desktop = None):
    chosen(desktop)
    if distro in IMAGES:
        return []
    if distro in TRANSACTIONAL:
        return [service("NetworkManager")]
    f = family(distro)
    found = base_modules(distro) + [
        service(DISPLAY_MANAGER[desktop].get(f, DEFAULT_MANAGER[desktop])),
        service("cupsd" if f == "void" else "cups"),
        # Flatpak ships no remote, and it imports this file on first use.
        file("/etc/flatpak/remotes.d/flathub.flatpakrepo", src = "files/flathub.flatpakrepo"),
        # The browser updates itself from Flathub, so it needs no lock entry.
        pkg("org.mozilla.firefox", source = flatpak("org.mozilla.firefox"), mode = "self_managed"),
    ]
    if f == "void":
        found = found + [service("elogind")] + VOID_AUDIO
    return found

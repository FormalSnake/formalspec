def apps(host):
    return [
        pkg("htop", source = dnf()),
        pkg("dev-tools", source = dnf(["jq", "ripgrep"])),
        pkg("firefox", source = flatpak("org.mozilla.firefox")),
        pkg("steam", source = flatpak("com.valvesoftware.Steam"), mode = "self_managed"),
    ]

hosts = [
    host(name = "workstation", platform = "x86_64-linux", system = fedora(), modules = [apps]),
]

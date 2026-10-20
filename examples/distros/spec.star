def dev_tools(host):
    # The lock holds one source per package name, so each distro's packages get their own name.
    mgr = {"fedora": dnf, "arch": pacman, "debian": apt, "void": xbps}[host.distro]
    return [
        pkg("dev-tools-" + host.distro, source = mgr(["jq", "ripgrep"])),
        pkg("htop-" + host.distro, source = mgr(["htop"])),
    ]

hosts = [
    host(
        name = "fedora",
        platform = "x86_64-linux",
        system = fedora(packages = ["kernel", "NetworkManager", "firewalld"]),
        modules = [dev_tools],
    ),
    host(
        name = "arch",
        platform = "x86_64-linux",
        system = arch(packages = ["base", "linux", "linux-firmware", "networkmanager"]),
        modules = [dev_tools],
    ),
    host(
        name = "debian",
        platform = "x86_64-linux",
        system = debian(packages = ["linux-image-amd64", "network-manager", "sudo"]),
        modules = [dev_tools],
    ),
    host(
        name = "void",
        platform = "x86_64-linux",
        system = void(packages = ["base-system", "linux", "NetworkManager"]),
        modules = [dev_tools],
    ),
]

def dev_tools(host):
    if host.distro == "fedora":
        return [pkg("dev-tools", source = dnf(["jq", "ripgrep"])), pkg("htop", source = dnf())]
    if host.distro == "arch":
        return [pkg("dev-tools", source = pacman(["jq", "ripgrep"])), pkg("htop", source = pacman())]
    if host.distro == "debian":
        return [pkg("dev-tools", source = apt(["jq", "ripgrep"])), pkg("htop", source = apt())]
    return [pkg("dev-tools", source = xbps(["jq", "ripgrep"])), pkg("htop", source = xbps())]

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

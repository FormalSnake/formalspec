# A console system with networking and sudo. The other presets build on it.

FAMILY = {"cachyos": "arch", "void-musl": "void"}

NETWORK = {
    "fedora": ["NetworkManager"],
    "arch": ["networkmanager"],
    "debian": ["network-manager"],
    "ubuntu": ["network-manager"],
    "void": ["NetworkManager", "dbus"],
    "aerynos": ["networkmanager"],
}

NETWORK_SERVICES = {
    "void": ["dbus", "NetworkManager"],
}

SUDO = {"aerynos": ["sudo-rs"]}
EDITOR = ["nano"]

def family(distro):
    return FAMILY.get(distro, distro)

def distro_system(distro, packages):
    if distro == "fedora":
        return fedora(packages = packages)
    if distro == "arch":
        return arch(packages = packages)
    if distro == "cachyos":
        return cachyos(packages = packages)
    if distro == "debian":
        return debian(packages = packages)
    if distro == "ubuntu":
        return ubuntu(packages = packages)
    if distro == "void":
        return void(packages = packages)
    if distro == "void-musl":
        return void(libc = "musl", packages = packages)
    if distro == "aerynos":
        return aerynos(packages = packages)
    fail("minimal has no system for " + distro)

def system(distro, desktop = None, extra = []):
    return distro_system(distro, NETWORK[family(distro)] + SUDO.get(distro, ["sudo"]) + EDITOR + extra)

def modules(distro, desktop = None):
    f = family(distro)
    found = [service(name) for name in NETWORK_SERVICES.get(f, ["NetworkManager"])]
    # These ship without a wheel rule.
    if f in ["arch", "void"]:
        found.append(file("/etc/sudoers.d/wheel", content = "%wheel ALL=(ALL:ALL) ALL\n", perms = "0440"))
    return found

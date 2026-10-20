# Run inside throwaway systemd-nspawn containers, one per distro. Flip `live` to False and
# apply again to check removal.
live = True

def mgr(host):
    return {"fedora": dnf, "arch": pacman, "debian": apt, "void": xbps}[host.distro]

def probe(host):
    if not live:
        return []
    return [
        pkg("ripgrep"),
        pkg("tree-" + host.distro, source = mgr(host)(["tree"])),
        file("/etc/fspec-live.conf", content = "managed = true\n"),
        file("~/.config/fspec-live/settings.json", mode = "patch", owns = {"model": "opus"}),
        file("~/.config/fspec-live/seed.txt", mode = "seed", content = "seeded once\n"),
        service("fspec-live-sleep", exec = "/bin/sleep 86400", restart = "always"),
        timer("fspec-live-tick", exec = "/bin/true", schedule = "@hourly"),
        user("fspecuser", shell = "/bin/sh"),
    ]

hosts = [
    host("fedora", platform = "x86_64-linux", system = fedora(packages = ["jq"] if live else []), modules = [probe]),
    host("arch", platform = "x86_64-linux", system = arch(packages = ["jq"] if live else []), modules = [probe]),
    host("debian", platform = "x86_64-linux", system = debian(packages = ["jq"] if live else []), modules = [probe]),
    host("void", platform = "x86_64-linux", system = void(packages = ["jq"] if live else []), modules = [probe]),
    # The Debian container again, applied over ssh from the machine running the containers.
    host(
        "debian-remote",
        ssh = "ssh://root@127.0.0.1:22922",
        platform = "x86_64-linux",
        system = debian(packages = ["jq"] if live else []),
        modules = [probe],
    ),
]

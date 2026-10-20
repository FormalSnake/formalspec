def fedora_box(name):
    return host(
        name = name,
        platform = "x86_64-linux",
        system = fedora(),
        users = [user("kyan", uid = 1000, shell = "fish", groups = ["wheel", "docker"])],
        modules = [
            service("syncthing", exec = "syncthing serve --no-browser", user = True, restart = "always", after = ["network"]),
            service(
                "caddy",
                exec = "/usr/bin/caddy run --config /etc/caddy/Caddyfile",
                restart = "on_failure",
                after = ["network", "docker.target"],
                env = {"XDG_DATA_HOME": "/var/lib/caddy"},
                description = "Caddy web server",
                systemd = {
                    "unit": {"StartLimitBurst": 5},
                    "service": {"AmbientCapabilities": "CAP_NET_BIND_SERVICE"},
                    "MemoryMax": "512M",
                    "NoNewPrivileges": True,
                },
            ),
            timer("backup", exec = "/usr/local/bin/backup --all", schedule = "30 2 * * 1-5"),
            timer("tidy", exec = "fstrim -av", schedule = "@weekly"),
        ],
    )

def mac(name):
    return host(
        name = name,
        platform = "aarch64-darwin",
        users = [user("kyan", uid = 501, shell = "fish")],
        modules = [
            service("syncthing", exec = "syncthing serve --no-browser", user = True, restart = "on_failure", env = {"PATH": "/opt/homebrew/bin:/usr/bin:/bin"}),
            service("tunnel", exec = "cloudflared tunnel run home 2>&1 | logger -t tunnel", restart = "always"),
            timer("backup", exec = "/usr/local/bin/backup --all", schedule = "0 9,17 * * 1-5", user = True),
            defaults("com.apple.dock", "autohide", True),
            defaults("com.apple.dock", "tilesize", 48),
            defaults("NSGlobalDomain", "AppleShowAllExtensions", True),
            defaults("com.apple.finder", "FXPreferredViewStyle", "Nlsv"),
        ],
    )

hosts = [fedora_box("fedora"), mac("mac")]

def services():
    return [
        service("web", exec = "/usr/bin/webd --port 8080", restart = "always", after = ["network"], env = {"PORT": "8080"}),
        service("worker", exec = "worker --queue default", restart = "on_failure", after = ["web"]),
        service("oneshot", exec = "/usr/local/bin/seed", restart = "no"),
        timer("backup", exec = "/usr/local/bin/backup --all", schedule = "30 2 * * 1-5"),
    ]

def user_services():
    return [
        service("syncthing", exec = "syncthing serve --no-browser", user = True, restart = "always"),
        timer("notes", exec = "/usr/local/bin/sync-notes", schedule = "@daily", user = True),
    ]

hosts = [
    host(
        name = "void",
        platform = "x86_64-linux",
        system = void(),
        modules = services() + user_services(),
    ),
    host(
        name = "arch-openrc",
        platform = "x86_64-linux",
        system = arch(init = "openrc"),
        modules = services() + [user_services()[1]],
    ),
    host(
        name = "arch-dinit",
        platform = "x86_64-linux",
        system = arch(init = "dinit"),
        modules = services() + user_services(),
    ),
    host(
        name = "arch-s6",
        platform = "x86_64-linux",
        system = arch(init = "s6"),
        modules = services() + [user_services()[1]],
    ),
]

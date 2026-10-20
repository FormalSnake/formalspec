def desktop(host):
    return [
        pkg(
            "syncthing",
            source = github("syncthing/syncthing"),
            data = ["~/.local/state/syncthing"],
            services = [service("syncthing", exec = "syncthing serve --no-browser", user = True, restart = "always", after = ["network"])],
        ),
        pkg("steam", source = flatpak("com.valvesoftware.Steam"), mode = "self_managed"),
        timer("backup", exec = "restic backup ~/Documents", schedule = "@daily", user = True),
    ]

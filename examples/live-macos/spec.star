# Touches only fspec-namespaced paths, so applying it and then `live = False` leaves the
# machine as it was. Used to check apply, drift, removal and rollback on a real macOS host.
live = False

def probe(host):
    if not live:
        return []
    return [
        pkg("ripgrep"),
        file("~/.config/fspec-live/managed.txt", content = "hello from fspec\n"),
        file("~/.config/fspec-live/settings.json", mode = "patch", owns = {
            "model": "opus",
            "allow": list_union(["rg", "fd"]),
        }),
        file("~/.config/fspec-live/seed.txt", mode = "seed", content = "seeded once\n"),
        service("fspec-live-sleep", exec = "sleep 86400", user = True, restart = "always"),
        defaults("fspec.live", "flag", True),
    ]

hosts = [host("macbook-pro-2", platform = "aarch64-darwin", modules = [probe])]

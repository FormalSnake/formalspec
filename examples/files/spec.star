# Only file and secret resources, so apply has nothing unsupported on this host.
# To try it in a temp dir, copy age.key to <home>/.config/fspec/age.key first (a demo key).

def files(host):
    return [
        file("~/.config/fspec-demo/settings.json", content = {
            "theme": "dark",
            "editor": {"tabs": 4, "wrap": True},
        }),
        file("~/.config/fspec-demo/notes.txt", src = "files/notes.txt", perms = "0600"),
        file("~/.config/fspec-demo/rc.conf", mode = "link", src = "files/rc.conf"),
        file("~/.config/fspec-demo/first-run.toml", mode = "seed", content = {"welcome": True}),
        file("~/.config/fspec-demo/app.json", mode = "patch", on_conflict = "ours", owns = {
            "model": "opus",
            "permissions.allow": list_union(["Bash(rg:*)", "Bash(fd:*)"]),
            "hooks.PostToolUse": list_keyed("matcher", [{"matcher": "Edit", "command": "fmt"}]),
        }),
        secret("~/.config/fspec-demo/token", src = "secrets/token.age"),
    ]

hosts = [
    host(name = "macbook", platform = "aarch64-darwin", modules = [files]),
]

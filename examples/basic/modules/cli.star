def cli(host):
    tools = [
        pkg("ripgrep"),
        pkg("fd", source = github("sharkdp/fd")),
        pkg("claude-code", source = npm("@anthropic-ai/claude-code"), mode = "self_managed"),
        file("~/.config/fish/config.fish", src = "files/config.fish"),
        file("~/.claude/settings.json", mode = "patch", owns = {
            "model": "opus",
            "permissions.allow": list_union(["Bash(rg:*)", "Bash(fd:*)"]),
        }),
    ]
    if host.platform.is_darwin:
        tools.append(pkg("aerospace", source = brew(cask = True)))
    return tools

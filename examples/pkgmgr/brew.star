def tools(host):
    return [
        pkg("ripgrep", source = brew()),
        pkg("aerospace", source = brew(cask = True)),
        pkg("fd-find", source = brew("fd"), mode = "self_managed"),
    ]

hosts = [
    host(name = "macbook", platform = "aarch64-darwin", modules = [tools]),
]

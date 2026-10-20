load("//modules/cli.star", "cli")
load("//modules/desktop.star", "desktop")

def laptop(name, extra = []):
    return host(
        name = name,
        platform = "x86_64-linux",
        system = image(base = "ghcr.io/ublue-os/bazzite-nvidia:stable", kernel_args = ["iommu=force"]),
        users = [user("kyandesutter", uid = 1000, shell = "fish")],
        modules = [cli, desktop] + extra,
    )

hosts = [
    laptop("g815"),
    laptop("e1504g"),
    host(
        name = "macbook",
        platform = "aarch64-darwin",
        users = [user("kyandesutter", uid = 501, shell = "fish")],
        modules = [cli, defaults("com.apple.dock", "autohide", True)],
    ),
]

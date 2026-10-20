# fspec.lock is not committed: it holds file:// URLs, which must be absolute, so
# crates/fspec/tests/store.rs writes one next to a copy of this spec.
hosts = [
    host(
        name = "macbook",
        platform = "aarch64-darwin",
        modules = [
            lambda host: [
                pkg("hello"),
                pkg("greet", source = github("acme/greet")),
                pkg("shout", source = npm("@acme/shout")),
            ],
        ],
    ),
]

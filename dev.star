# Rust through rustup's proxies, which act on the name they run as and install the pinned
# toolchain on first use.
RUST = ["cargo", "rustc", "rustdoc", "rustfmt", "cargo-fmt", "cargo-clippy", "clippy-driver"]

dev(
    packages = [pkg("rustup", links = {name: "rustup" for name in RUST})],
    # mkShell's stdenv brings a C compiler for build scripts and ring; Xcode's does on macOS.
    bins = [bin("gcc")] if platform.is_linux else [],
    env = {"RUSTUP_TOOLCHAIN": "1.99.1"},
)

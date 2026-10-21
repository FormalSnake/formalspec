#!/usr/bin/env bash
# Builds out/formalspec-installer-amd64.iso. Needs docker and network; run from anywhere.
# Debian packages come from the snapshot.debian.org timestamp in ./snapshot, and the ISO timestamps
# come from SOURCE_DATE_EPOCH (default: the HEAD commit time). The file name has no version or date,
# so releases/latest/download/formalspec-installer-amd64.iso always names the newest release.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
work=$here/work
out=$here/out

RUST_IMAGE=rust:1.91-alpine3.22@sha256:8efbfb788786eeb127adc581394349c5fb567712156e0f8c2e499acadbc23756
DEBIAN_IMAGE=debian:trixie@sha256:4c25275446ca371766b8b093c696782eabce06263fccacb8d5d813b3dc44b308
SBCTL_VERSION=0.18
SBCTL_SHA256=d274451b145b0aaecfdf2d01ad45473b61ab40f3f58e4735cee50aa7573c584d

SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$repo" log -1 --format=%ct)}
export SOURCE_DATE_EPOCH

mkdir -p "$out" "$work"
rm -rf "$work/config"
cp -R "$here/config" "$work/config"
bin=$work/config/includes.chroot/usr/local/bin

docker run --rm -v "$repo:/src:ro" -v "$work/cargo:/cargo" -v "$work/target:/target" \
	-e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target -e SOURCE_DATE_EPOCH -w /src "$RUST_IMAGE" sh -ec '
	apk add --no-cache build-base >/dev/null
	cargo build --release --locked --target x86_64-unknown-linux-musl -p fspec -p fspec-install
'
install -m 0755 "$work/target/x86_64-unknown-linux-musl/release/fspec" "$bin/fspec"
install -m 0755 "$work/target/x86_64-unknown-linux-musl/release/fspec-install" "$bin/fspec-install"

# sbctl is not packaged in trixie and upstream ships source only; built against trixie so it links the live system's libpcsclite.
docker run --rm -v "$work:/work" -v "$here:/iso:ro" -e SBCTL_VERSION="$SBCTL_VERSION" -e SBCTL_SHA256="$SBCTL_SHA256" \
	"$DEBIAN_IMAGE" bash -ec '
	. /iso/snapshot-apt.sh
	apt-get update -qq
	apt-get install -y -qq golang-go gcc libc6-dev libpcsclite-dev pkgconf curl ca-certificates >/dev/null
	cd /tmp
	curl -fsSL -o sbctl.tar.gz "https://github.com/Foxboron/sbctl/releases/download/$SBCTL_VERSION/sbctl-$SBCTL_VERSION.tar.gz"
	echo "$SBCTL_SHA256  sbctl.tar.gz" | sha256sum -c -
	tar xzf sbctl.tar.gz
	cd "sbctl-$SBCTL_VERSION"
	go build -trimpath -ldflags "-s -w" -o /work/config/includes.chroot/usr/local/bin/sbctl ./cmd/sbctl
'

docker run --rm --privileged -v "$work:/work" -v "$here:/iso:ro" \
	-e OUT_UID="$(id -u)" -e OUT_GID="$(id -g)" -e SOURCE_DATE_EPOCH \
	"$DEBIAN_IMAGE" bash /iso/lb-build.sh

iso=formalspec-installer-amd64.iso
mv "$work/formalspec-installer.iso" "$out/$iso"
ls -l "$out/$iso"

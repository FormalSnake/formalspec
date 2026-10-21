# Sourced inside a debian:trixie container. Release builds read the snapshot.debian.org timestamp
# in /iso/snapshot so a tag rebuilds from the same packages; dev builds use deb.debian.org, which
# is far faster than the throttled snapshot service. Either way the .debs land in a cache that
# build.sh keeps between runs, so a rebuild only downloads what changed.
if [ "$RELEASE" = 1 ]; then
	snapshot=$(cat /iso/snapshot)
	main=http://snapshot.debian.org/archive/debian/$snapshot/
	security=http://snapshot.debian.org/archive/debian-security/$snapshot/
	echo 'Acquire::Check-Valid-Until "false";' > /etc/apt/apt.conf.d/99snapshot
else
	main=http://deb.debian.org/debian/
	security=http://deb.debian.org/debian-security/
fi
# The image's docker-clean hook deletes downloaded .debs, which would empty the bind-mounted cache.
rm -f /etc/apt/sources.list /etc/apt/sources.list.d/* /etc/apt/apt.conf.d/docker-clean
cat > /etc/apt/sources.list.d/debian.sources <<SOURCES
Types: deb
URIs: $main
Suites: trixie trixie-updates
Components: main
Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg

Types: deb
URIs: $security
Suites: trixie-security
Components: main
Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg
SOURCES

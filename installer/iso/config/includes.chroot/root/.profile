case "$(tty)" in
/dev/tty1 | /dev/ttyS0 | /dev/ttyS1)
	if [ -z "$FSPEC_SESSION" ] && ! grep -qw fspec.session=shell /proc/cmdline; then
		FSPEC_SESSION=1 exec /usr/local/bin/fspec-install-session
	fi
	;;
esac
[ -n "$BASH_VERSION" ] && [ -f ~/.bashrc ] && . ~/.bashrc

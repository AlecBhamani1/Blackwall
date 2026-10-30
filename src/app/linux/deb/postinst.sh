#!/bin/sh
# Debian postinst: load the AppArmor profile for the bundled bubblewrap (/usr/lib/blackwall/bwrap).
# The profile is copied into /etc/apparmor.d only when this system's AppArmor can parse it, so a
# parser without user-namespace rules (Ubuntu 22.04, Debian 12) never meets a profile it would fail
# to load at boot. Installation never fails because of AppArmor: without the profile, Blackwall uses
# a system bwrap 0.12 or newer, or shows sandboxed commands as unavailable.
set -e

source=/usr/share/blackwall/apparmor/blackwall-bwrap
profile=/etc/apparmor.d/blackwall-bwrap

if [ "$1" = configure ] && command -v apparmor_parser >/dev/null 2>&1 && aa-enabled --quiet 2>/dev/null; then
  if apparmor_parser -Q -K "$source" >/dev/null 2>&1; then
    install -m 0644 "$source" "$profile"
    if ! apparmor_parser -r -T "$profile"; then
      rm -f "$profile"
      echo 'Blackwall: the AppArmor profile for bubblewrap did not load; sandboxed commands may be unavailable.' >&2
    fi
  else
    echo 'Blackwall: this AppArmor version does not support the bubblewrap profile; it was not installed.' >&2
  fi
fi

exit 0

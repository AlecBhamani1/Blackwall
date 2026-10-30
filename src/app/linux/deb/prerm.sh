#!/bin/sh
# Debian prerm: unload and delete the bubblewrap AppArmor profile that postinst installed.
# Upgrades keep it loaded; the new version's postinst replaces it.
set -e

profile=/etc/apparmor.d/blackwall-bwrap

if [ "$1" = remove ] && [ -f "$profile" ]; then
  if command -v apparmor_parser >/dev/null 2>&1 && aa-enabled --quiet 2>/dev/null; then
    apparmor_parser -R "$profile" >/dev/null 2>&1 || true
  fi
  rm -f /etc/apparmor.d/blackwall-bwrap
  rm -f /var/cache/apparmor/*/blackwall-bwrap
fi

exit 0

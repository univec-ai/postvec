#!/bin/sh
# Create the postvec-server system account. Do not enable start or reload
# the unit, and do not edit configuration. verify-package.sh scans this
# file for those commands.
#
# Same account name on Debian and RPM. Never removed on purge: a uid that
# has owned a lease file or a home directory is not one to reuse.
set -e

if ! getent group postvec-server >/dev/null 2>&1; then
    groupadd --system postvec-server
fi
if ! getent passwd postvec-server >/dev/null 2>&1; then
    # --no-create-home: /var/lib/postvec-server is package-owned and unpacked
    # right after this, so useradd must not race it into existence with the
    # wrong mode. /usr/sbin/nologin exists on every supported distribution
    # (all of them are merged-/usr).
    useradd --system --gid postvec-server \
        --home-dir /var/lib/postvec-server --no-create-home \
        --shell /usr/sbin/nologin \
        --comment "postvec inference node" \
        postvec-server
fi

exit 0

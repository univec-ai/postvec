#!/bin/sh
# Runs before postvec-server's files are unpacked.
#
# Its one job is the service account the unit runs as. Creating a system user
# is the ordinary thing a daemon package does at install time, and it is the
# only thing this script does: no unit is enabled, started or reloaded, no
# configuration is edited, nothing is downloaded, nothing is deleted. See
# packaging/postvec/README.md §"What the packages may not do" — the same
# contract every postvec package is held to, and scripts/verify-package.sh
# scans this file for the commands that would break it.
#
# Same name on both families, and never removed on purge: a uid that has owned
# a lease file or a home directory is not one to hand to the next account.
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

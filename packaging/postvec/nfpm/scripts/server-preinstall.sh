#!/bin/sh
# Create the postvec-server system account. Do not enable start or reload
# the unit, and do not edit configuration. verify-package.sh scans this
# file for those commands.
#
# Same account name on Debian and RPM. Never removed on purge: a uid that
# has owned a lease file or a home directory is not one to reuse.
set -e

# uid/gid 999 when free (always in the image, whose USER is numeric);
# otherwise the next free system id.
free_999() { getent "$1" 999 >/dev/null 2>&1 || echo 999; }

if ! getent group postvec-server >/dev/null 2>&1; then
    gid="$(free_999 group)"
    groupadd --system ${gid:+--gid "$gid"} postvec-server
fi
if ! getent passwd postvec-server >/dev/null 2>&1; then
    uid="$(free_999 passwd)"
    # --no-create-home: /opt/postvec is package-owned and unpacked right
    # after this, so useradd must not race it into existence with the wrong
    # mode. /usr/sbin/nologin exists on every supported distribution (all of
    # them are merged-/usr).
    useradd --system ${uid:+--uid "$uid"} --gid postvec-server \
        --home-dir /opt/postvec --no-create-home \
        --shell /usr/sbin/nologin \
        --comment "postvec inference node" \
        postvec-server
fi

exit 0

#!/bin/sh
# Runs after postvec-server's files are removed.
#
# Prints a hint. The operator stops or disables the unit first; systemd
# keeps running a unit whose file is gone until the next reload. The engine
# root, certificates and service account stay in place.
#
# No `set -e`; ends in an unconditional `exit 0`.
#
# What happens to the configuration file is the package manager's rule, and
# the two families differ, so the message is per family. Debian: `remove`
# keeps the conffile, `purge` deletes it. RPM: an unchanged config.json is
# removed with the package; an edited one is kept as config.json.rpmsave.

# Debian passes "remove"/"purge"/"upgrade"; RPM passes the number of remaining
# copies (1 = an upgrade, 0 = a real removal). Say nothing during an upgrade.
case "${1:-}" in
    upgrade|failed-upgrade|1) exit 0 ;;
esac

case "${1:-}" in
    purge)
        config_note="/etc/postvec-server/config.json was deleted (purge)." ;;
    remove)
        config_note="/etc/postvec-server/config.json is kept until you purge the package." ;;
    *)
        config_note="/etc/postvec-server/config.json was removed if you never edited it; an edited one is kept as config.json.rpmsave." ;;
esac

cat >&2 <<EOF || true
postvec-server: package files removed.

If the node was running, stop and disable it and reload systemd:

  systemctl disable --now postvec-server
  systemctl daemon-reload

${config_note}
Nothing under /opt/postvec was deleted (models are yours), your certificate
pair under /etc/postvec-server is untouched, and the postvec-server account
is kept so its uid is never reused. Remove them yourself if this host will
not run a node again.
EOF

exit 0

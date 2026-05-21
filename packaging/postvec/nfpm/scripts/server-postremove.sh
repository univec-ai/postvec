#!/bin/sh
# Runs after postvec-server's files are removed.
#
# Only says something. It must not stop or disable the unit (the operator
# does that first; systemd keeps running a unit whose file is gone until the
# next reload), must not delete the engine root, the configuration or the
# service account, and must never fail — hence no `set -e` and an
# unconditional `exit 0`.

# Debian passes "remove"/"purge"/"upgrade"; RPM passes the number of remaining
# copies (1 = an upgrade, 0 = a real removal). Say nothing during an upgrade.
case "${1:-}" in
    upgrade|failed-upgrade|1) exit 0 ;;
esac

cat >&2 <<'EOF' || true
postvec-server: package files removed.

If the node was running, stop and disable it and reload systemd:

  systemctl disable --now postvec-server
  systemctl daemon-reload

Nothing under /opt/postvec, /var/lib/postvec-server or /etc/postvec-server was
deleted: models are yours, the configuration may hold certificate paths, and
the postvec-server account is kept so its uid is never reused. Remove them
yourself if this host will not run a node again.
EOF

exit 0

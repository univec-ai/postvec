#!/bin/sh
# Runs after the extension package's files are removed.
#
# Prints a hint. Package removal removes package-owned files. Database
# state is the operator's; only `postvec uninstall` may touch it.
#
# Runs without `set -e` and ends in an unconditional `exit 0`, so a closed
# stderr cannot fail the removal.

# Debian passes "remove"/"purge"/"upgrade"; RPM passes the number of remaining
# copies of this package (1 = an upgrade, 0 = a real removal). Say nothing
# during an upgrade — nothing has been uninstalled.
case "${1:-}" in
    upgrade|failed-upgrade|1) exit 0 ;;
esac

cat >&2 <<'EOF' || true
postvec: package files removed.

Nothing in your databases or your PostgreSQL configuration was changed. If a
cluster is still configured to preload postvec, its workers will now fail to
load the library:

  postvec uninstall --database <name>   # remove the extension and stop serving it
  postvec doctor                        # what is still configured

Run these *before* removing the package for a clean uninstall.

External embedding providers, if you configured any, keep their connector
files in /etc/postvec/providers.d. Those hold API credentials, they were never
package-owned, and nothing here deletes them — remove them yourself when no
host needs them any more.
EOF

exit 0

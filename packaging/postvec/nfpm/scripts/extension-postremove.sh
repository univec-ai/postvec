#!/bin/sh
# Runs after the extension package's files are removed.
#
# The only thing this script is permitted to do is *say* something. It must not
# stop, start, reload or reconfigure PostgreSQL, must not connect to a
# database, must not run CREATE/ALTER/DROP EXTENSION, and must not delete data
# or engine assets. Package removal removes package-owned files; database state
# is the operator's, and only `postvec uninstall` may touch it.
#
# It must also never fail. A package that cannot be removed because a message
# could not be written to a closed stderr is a far worse problem than a missing
# hint — so this script deliberately runs *without* `set -e`, and ends in an
# unconditional `exit 0`.

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
EOF

exit 0

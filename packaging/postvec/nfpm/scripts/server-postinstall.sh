#!/bin/sh
# Runs after postvec-server's files are in place.
#
# It says what to do next and does none of it. Enabling and starting a network
# service is the operator's decision — this package has just installed a
# listener that has no authentication and no model to serve yet — and the
# same rule that keeps every postvec package from touching a PostgreSQL
# cluster keeps this one from touching systemd. The commands below appear
# only inside the here-document; scripts/verify-package.sh strips those before
# scanning for what a maintainer script *runs*.
#
# Never fails: a package that cannot be configured because a message could not
# be written is a far worse problem than a missing hint.

# Debian passes "configure" on install and on upgrade with the previous
# version as $2; RPM passes the number of installed copies (2 = an upgrade).
# Say the long version once, on first install only.
case "${1:-}" in
    configure)
        [ -z "${2:-}" ] || exit 0 ;;
    2) exit 0 ;;
esac

cat >&2 <<'EOF' || true
postvec-server: installed, not started.

The node serves models from /opt/postvec (the packaged unit's engine root):

  apt install postvec-extras            # ONNX Runtime + the bundled model
  postvec model pull <name> --yes       # more models (needs postvec-cli)

Put a certificate pair at /opt/postvec/certs/server.crt and server.key, or
name one in /etc/postvec-server/config.json ("ssl"), then:

  systemctl daemon-reload
  systemctl enable --now postvec-server
  postvec-server status

The gRPC and discovery ports (33333, 22222) carry no authentication. Keep
them on a private network, or set "bind_address" to a private interface.
EOF

exit 0

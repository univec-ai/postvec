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

The node serves models from /opt/postvec (the packaged unit's engine root),
which the postvec-onnxruntime and postvec-model-* packages fill and
`sudo postvec model pull <name> --yes` (postvec-cli) adds to.

The discovery listener needs a certificate pair the service account can read.
Put yours beside the configuration, owned by root and readable by the
service group:

  install -o root -g postvec-server -m 0644 server.crt /etc/postvec-server/server.crt
  install -o root -g postvec-server -m 0640 server.key /etc/postvec-server/server.key

Then, when the models and the certificate are in place:

  systemctl daemon-reload
  systemctl enable --now postvec-server
  postvec-server status

The gRPC and discovery ports (33333, 22222) carry no authentication. Keep
them on a private network, or set "bind_address" in
/etc/postvec-server/config.json to a private interface.
EOF

exit 0

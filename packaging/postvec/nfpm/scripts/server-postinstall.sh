#!/bin/sh
# Runs after postvec-server's files are in place. Prints the next steps.
# Does not enable or start the service: the listener has no authentication
# yet, and no postvec package touches systemd. The commands live only in
# the here-document. verify-package.sh strips that before it scans for
# what a maintainer script runs. Exits 0 even if the hint cannot be written.

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

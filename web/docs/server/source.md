---
title: Build postvec-server from source
description: cargo build of postvec-server. No pgrx or PostgreSQL headers.
---

# From source

`postvec-server` is an ordinary workspace member. It needs neither pgrx
nor PostgreSQL headers:

```bash
cargo build --release -p postvec-server
sudo install -m 0755 target/release/postvec-server /usr/local/bin/
```

It still needs ONNX Runtime and at least one model under `--root`. A
checkout default is `/var/lib/postvec-server`. Point `--root` at
`/opt/postvec` if those packages are already installed, or copy the
tree there.

```bash
# Local development, no certificates.
postvec-server --root /var/lib/postvec-server --insecure

# A real process.
postvec-server \
  --root /var/lib/postvec-server \
  --ssl-cert /etc/postvec-server/tls.crt \
  --ssl-cert-key /etc/postvec-server/tls.key
```

A pair dropped at `$root/certs/server.crt` and `$root/certs/server.key`
is found without flags. The discovery listener requires TLS unless
`--insecure` is set.

The [dashboard](/docs/server/dashboard) needs a built
`postvec-server/web-ui` and `--web-ui`:

```bash
cd postvec-server/web-ui
npm ci && npm run build
postvec-server --root /var/lib/postvec-server \
  --web-ui "$PWD/dist" --insecure
```

See [models](/docs/server/models) for pulling more than MiniLM.

::: warning Licence
`postvec-server` is **Business Source License 1.1** (source-available).
Personal production use, non-production environments and a 30-day
production evaluation per organization are free. Production use by an
organization needs [postvec Pro](https://univec.ai). [License](/docs/license).
:::

Next: [connect PostgreSQL](/docs/server/connect).

- [Docker](/docs/server/docker)
- [Packages](/docs/server/packages)
- [Dashboard](/docs/server/dashboard)
- [Reference](/docs/server/reference)

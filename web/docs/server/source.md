---
title: Build postvec-server from source
description: Build the postvec-server binary with cargo, with or without GPU support.
---

# From source

`postvec-server` is an ordinary workspace member and builds with `cargo`:
it needs the Rust toolchain and `protoc`, which compiles the wire contract in
`proto/ninference.proto`. The pgrx toolchain and PostgreSQL headers belong to
the extension.

```bash
cargo build --release -p postvec-server
sudo install -m 0755 target/release/postvec-server /usr/local/bin/
```

The published packages and image carry the CPU build. A GPU node is a source
build with an execution provider feature:

```bash
cargo build --release -p postvec-server --features ort-cuda    # or ort-tensorrt
```

The node needs ONNX Runtime and at least one model under the engine root,
`/opt/postvec` by default: install the `postvec-onnxruntime` and
`postvec-model-*` packages, or copy such a tree there. `--root` names
another tree.

```bash
# Local development, plaintext.
postvec-server --insecure

# With a certificate pair.
postvec-server \
  --ssl-cert /etc/postvec-server/tls.crt \
  --ssl-cert-key /etc/postvec-server/tls.key
```

A pair dropped at `$root/certs/server.crt` and `$root/certs/server.key`
is found without flags. The discovery listener requires TLS unless
`--insecure` is set.

The [dashboard](/docs/server/dashboard) needs a built
`postvec-server/web-ui`, passed with `--web-ui` or installed at
`$root/server/ui`, which is where the packages put it:

```bash
cd postvec-server/web-ui
npm ci && npm run build
postvec-server --web-ui "$PWD/dist" --insecure
```

See [models](/docs/server/models) to install more models.

::: warning License
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

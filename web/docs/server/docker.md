---
title: postvec-server with Docker
description: Run the postvec-server image. MiniLM is loaded at start.
---

# Docker

The published image is Debian 12 with the CPU build of postvec-server,
the CLI, ONNX Runtime and the bundled MiniLM model. It serves MiniLM out
of the box.

Pair it with the remote PostgreSQL image in
[quick start remote](/docs/quickstart-remote).

<PgSnippet id="docker-server" />

Wait until the container health is `healthy`; the image healthcheck
calls `GET /ready`:

<div v-pre>

```bash
docker inspect --format '{{.State.Health.Status}}' postvec-server
```

</div>

The container generates a self-signed certificate at start, per
container. Mount a pair over `/etc/postvec-server/server.crt` and
`server.key`, or pass `--ssl-cert` / `--ssl-cert-key`. A bind mount
keeps the host's numeric owner, so a mounted key must be readable by the
`postvec-server` account the image runs as (uid and gid `999`); a Compose
secret with `uid: "999"` and `mode: 0400` is one way. The admin port is not
exposed.

:::: info Optional
```bash
docker exec postvec-server postvec-server status
curl -sk https://127.0.0.1:22222/ready
```

`status` prints the version and build features, readiness, the frontend
address, the models and the cluster members. `/ready` is `200` once a
model can serve. Image attestations:
[verify artifacts](/docs/install/verify).
::::

A checkout builds the same image from locally built packages with
`packaging/postvec/scripts/build-server-image.sh`.

::: warning License
`postvec-server` is **Business Source License 1.1** (source-available).
Personal production use, non-production environments and a 30-day
production evaluation per organization are free. Production use by an
organization needs [postvec Pro](https://univec.ai). [License](/docs/license).
:::

Next: [connect PostgreSQL](/docs/server/connect). The dashboard is
`https://<host>:22222`.

- [Packages](/docs/server/packages)
- [From source](/docs/server/source)
- [Dashboard](/docs/server/dashboard)
- [Models](/docs/server/models)

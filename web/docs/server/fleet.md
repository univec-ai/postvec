---
title: Fleet
description: Several postvec-server processes, the inventory parity rule, drift detection and rolling restarts.
---

# Fleet

A fleet is several postvec-server processes with the same models, serving
the same cluster. Every member is identical: same command, same ports,
same models. Gossip answers who else is alive.

## Start the nodes

```bash
# On every node. Only --advertise differs, and only on multi-homed hosts.
postvec-server \
  --peers node-1,node-2,node-3 \
  --advertise 10.0.0.10 \
  --ssl-cert /etc/postvec-server/tls.crt \
  --ssl-cert-key /etc/postvec-server/tls.key
```

`--peers` takes bare hosts, names or IPs, because every node uses the same
gossip port. `host:port` is an escape hatch for one colliding entry rather
than the interface. Listing the node itself is fine; the self-join is skipped.

`--group` (default `postvec`) tags the cluster. Nodes with different groups
never merge. Change it when two unrelated fleets share a network.

Then give the database every node:

```bash
sudo postvec setup --database app \
  --grpc 10.0.0.10:33333,10.0.0.11:33333,10.0.0.12:33333 \
  --http https://10.0.0.10:22222,https://10.0.0.11:22222,https://10.0.0.12:22222
```

## Clustering scope

- **Models.** Load a converter on each node that should serve it.
- **Load balancing.** postvec round-robins the gRPC list itself.
- **Discovery.** postvec never joins gossip and never reads membership.
  The endpoint lists are the routing table.

## Inventory parity

**Every node carries the same enabled set**, or the HTTP endpoint list is
restricted to the nodes that do.

The operator has to keep that true. Otherwise postvec routes a request
to a node that lacks the model, and a fraction of requests fail with
`MODEL_NOT_LOADED`.

The check:

```bash
postvec-server status --fleet
```

:::: tip Expected
It reads every alive peer's `/config` and names any model missing from some of
them. Exit code `1` when it finds drift, so it works unattended in a cron job
or a CI step.
::::

External providers are part of the same contract. Every node must carry the
same `providers.d`, and the drift report labels those entries, because the fix
is a missing connector file or an unresolvable key on that node rather than a
missing model directory:

```text
  openai-text-embedding-3-small  missing on node-3
      [provider-backed via "openai" - check providers.d and its key on that node]
```

Provider entries must also match, not just exist on every node. The same
command fingerprints each provider-backed name by its connector type,
`providers.d` file stem, `provider_model_id` and declared dimension, and
reports the names where two nodes disagree:

```text
  openai-text-embedding-3-small: served with DIFFERENT provider settings across the fleet
      [<fingerprint> on node-1 | <fingerprint> on node-3 - fix providers.d so every node agrees]
```

Round-robin sends each caller to one of the disagreeing nodes. A dimension
mismatch arrives as intermittent dead letters. A different model at the same
dimension mixes two vector spaces in one column without an error.

Provider-backed embed models and converters are also excluded from the
descriptor-drift checks, which assume an on-disk descriptor.

## Rolling restarts

On `SIGTERM` or ctrl-c a node drains:

1. `/ready` flips to `503`, so load balancers and healthchecks stop routing
   here, and the node announces its departure to its peers.
2. It **keeps serving** for `--drain-delay-ms` (5 s by default). The window is
   what makes step 1 observable to a load balancer, and the time the gossip
   announcement needs to reach the other nodes. A second signal skips the wait.
3. The listeners stop accepting. In-flight requests finish or hit their own
   deadline. An idle node exits immediately rather than padding the worst case.
4. `/health` stays `200` throughout, so a supervisor leaves the process
   running while in-flight requests finish.

So a rolling upgrade is: restart one node, wait for its `/ready` to go green,
move on. Budget `TimeoutStopSec` at more than `--drain-delay-ms` plus your
longest request. `--drain-delay-ms` replaces the usual Kubernetes `preStop`
sleep.

Version skew across a fleet is visible. Each node gossips its version and
`postvec-server status` prints it.

`/config` keeps advertising a draining node's models. postvec prunes its
SQL model cache only on a **complete** discovery refresh, so a
single-node deployment that emptied its model list mid-restart would
empty the database's cache with it. Traffic reroutes on the transport
error, which postvec already retries against the next endpoint.

## A node sees only itself

`postvec-server status` shows one member. Almost always the advertised
address: check the boot log for the autodetection warning and pin
`--advertise` to the IP the other nodes can reach. Then check that the gossip
port is open between them. It is a separate port from gRPC and discovery, and
firewalls forget it.

- [Connect PostgreSQL](/docs/server/connect)
- [Dashboard](/docs/server/dashboard)
- [Models on postvec-server](/docs/server/models)
- [Reference](/docs/server/reference)

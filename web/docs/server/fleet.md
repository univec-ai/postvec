---
title: Run a fleet
description: Several postvec-server nodes, the inventory parity rule, drift detection and rolling restarts.
---

# Run a fleet

Every node in a fleet is identical: same command, same ports, same models.
Gossip answers one question, which is who else is alive.

## Start the nodes

```bash
# On every node. Only --advertise differs, and only on multi-homed hosts.
postvec-server \
  --root /var/lib/postvec-server \
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

## What clustering does not do

- **It does not replicate models.** Load a converter on node A and node B does
  not grow one.
- **It does not load-balance.** postvec round-robins the gRPC list itself.
- **It does not feed discovery.** postvec never joins gossip and never reads
  membership. The endpoint lists are the routing table.

## The parity rule

**Every node carries the same enabled set**, or the HTTP endpoint list is
restricted to the nodes that do.

Nothing enforces it, and the failure it produces is unpleasant: postvec routes
a request to a node that lacks the model, and one request in three fails with
`MODEL_NOT_LOADED` that looks intermittent and reads like a network fault.

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

Provider-backed embed models and converters are also excluded from the
descriptor-drift checks, which assume an on-disk descriptor. They do not have
one.

## Rolling restarts

On `SIGTERM` or ctrl-c a node drains:

1. `/ready` flips to `503`, so load balancers and healthchecks stop routing
   here, and the node announces its departure to its peers.
2. It **keeps serving** for `--drain-delay-ms` (5 s by default). That window is
   what makes step 1 observable. Without it the listener stops accepting the
   instant the signal lands, and a load balancer learns the node is gone from a
   refused connection rather than from a health check. It is also the time the
   gossip announcement needs to reach peers; announce and tear down together
   and they fall back to anti-entropy, which takes thirty seconds. A second
   signal skips the wait.
3. The listeners stop accepting. In-flight requests finish or hit their own
   deadline. An idle node exits immediately rather than padding the worst case.
4. `/health` stays `200` throughout, so a supervisor does not kill the process
   mid-request.

So a rolling upgrade is: restart one node, wait for its `/ready` to go green,
move on. Budget `TimeoutStopSec` at more than `--drain-delay-ms` plus your
longest request. Under Kubernetes the drain delay does the job a `preStop`
sleep usually does, so you do not need both.

Version skew across a fleet is visible. Each node gossips its version and
`postvec-server status` prints it.

`/config` keeps advertising a draining node's models, which looks untidy and
is deliberate: postvec prunes its SQL model cache only on a **complete**
discovery refresh, so a single-node deployment that emptied its model list
mid-restart would empty the database's cache with it. Traffic reroutes on the
transport error, which postvec already retries against the next endpoint.

## A node sees only itself

`postvec-server status` shows one member. Almost always the advertised
address: check the boot log for the autodetection warning and pin
`--advertise` to the IP the other nodes can reach. Then check that the gossip
port is open between them. It is a separate port from gRPC and discovery, and
firewalls forget it.

## Related documentation

- [Connect PostgreSQL](/docs/server/connect) - the endpoint lists
- [Models on a node](/docs/server/models) - what parity is about
- [Node reference](/docs/server/reference) - flags, metrics and troubleshooting

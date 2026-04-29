# postvec-cli

The `postvec` command: install, configure and diagnose the
[postvec](../postvec) PostgreSQL extension against an existing cluster.

```bash
sudo postvec setup --database univec \
     --grpc 192.0.2.2:33333 --http https://192.0.2.2:22222
postvec doctor
sudo postvec uninstall --database univec
```

## What it is (and is not)

The package manager owns the extension files (`postvec.so`, `postvec.control`,
`postvec--<version>.sql`) and this binary. **The CLI never copies, installs or
deletes package-owned files.** "Install" here means: create the database if
asked, run `CREATE EXTENSION`, write one configuration snippet, restart with
consent, and prove the result is healthy.

That division is deliberate and it runs both ways. A package's maintainer
scripts must never restart a cluster, mutate a database or call this command —
so `apt install` gives you files and changes nothing, and everything that
*does* change something happens here, when you ask for it. The packaging side
enforces its half mechanically
([`packaging/postvec/`](../packaging/postvec/README.md) §"What the packages may
not do").

The CLI ships as its own package, `postvec-cli`, separate from the per-major
`postgresql-NN-postvec` packages. That is what lets PostgreSQL 16 and 18
extension packages be installed side by side: only one package may own
`/usr/bin/postvec`. The extension packages depend on a compatible CLI version
(`>= X.Y.Z`, `< the next breaking version`), so a mismatched pair cannot be
installed at all.

Version compatibility is checked at three layers, for the same reason: this
command compares the library and catalog versions, the background worker parks
itself when they disagree (re-asserting it inside every transaction it opens,
so a long backlog drain cannot straddle an upgrade), and the container
healthcheck reports the mismatch. The window they all guard is the one between
installing a new package and running `ALTER EXTENSION postvec UPDATE`. It does
*not* protect ordinary backends: during that window an application connection
can still load a new library against old SQL, so the restart and the `ALTER`
belong to the same maintenance window.

It exists because postvec has more footguns than a normal extension, and every
one of them is mechanically detectable: a `shared_preload_libraries` entry that
needs a restart, POSTMASTER versus SIGHUP settings, a configured-but-missing
database whose worker then respawns every 15 seconds, a thin build asked to run
in embedded mode, and an engine root whose model assets do not match what the
engine actually loaded.

## Commands

### `setup`

```bash
# Remote mode: a thin client to inference nodes (postvec-server).
sudo postvec setup --cluster 18/main --database univec \
     --grpc 192.0.2.2:33333 --http https://192.0.2.2:22222

# Add a second database without disturbing the first.
sudo postvec setup --database analytics \
     --grpc 192.0.2.2:33333 --http https://192.0.2.2:22222

# Embedded mode: the engine runs inside the launcher process.
sudo postvec setup --database univec --embedded --path /opt/postvec \
     --model baai-bge-m3 --model embed-bridge

# See the plan without changing anything.
postvec setup --database univec --grpc … --http … --dry-run
```

Order matters and is deliberate: **databases and extensions are prepared before
the launcher configuration and the restart**, so a worker is never activated for
a database that does not exist yet.

Setup is idempotent. A rerun with the same arguments renders a byte-identical
configuration file, plans nothing, and does not restart.

### `doctor`

```bash
postvec doctor                       # every configured database
postvec doctor --database univec --deep
postvec doctor --format json --strict
```

Read-only at every layer, and structurally so: the database handle exposes no
mutating operation, configuration is read with `postgres -C` (which parses a
candidate configuration without touching the running server), the host lock is
taken *shared* and only if it already exists, and no inference engine is ever
instantiated or asked to embed anything.

In embedded mode it reconciles the three inventories that drift independently —
descriptors on disk, models the engine actually loaded, and the `postvec.models`
cache — which otherwise means reading the server log.

### `uninstall`

```bash
# Retains data: the database, pgvector, and the shadow vector columns.
sudo postvec uninstall --database univec

# Destructive: needs two independent acknowledgements. Drops the shadow
# columns and the chunk destination tables postvec created
# (--keep-destinations keeps the latter).
sudo postvec uninstall --database univec \
     --drop-columns --acknowledge-data-loss --yes

# Every database in the cluster, then postvec's files on this host:
# pulled models, provider connector files, CLI state, an unpackaged
# extension library. Package-owned files stay; the apt/dnf line is printed.
sudo postvec uninstall --all --drop-columns --acknowledge-data-loss --purge --yes
```

`DROP EXTENSION` is never issued with `CASCADE`. Only databases whose SQL
removal actually succeeded are removed from the launcher configuration.
`--purge` deletes only what it can positively attribute to postvec (the
descriptor-bearing model directories, the CLI's model-store state, the
connector files it writes, its own state file, an unpackaged extension set),
under roots it has proven safe and that the CLI's own configuration names. It
runs only after a completed restart and a fresh reading of the configuration,
is fail-closed on package ownership (a lookup it cannot settle retains the
file, exit 3), refuses while another cluster on the host is still set up or a
database could not be inspected, and leaves the engine root alone while a
`postvec-server` process is running. The exact contract is
[docs/postvec-cli.md §7.1](../docs/postvec-cli.md#71---purge-the-exact-contract).

Where the launcher configuration cannot be changed — it was written by hand, it
has drifted, or the server is not on this host — the SQL is still removed but
the command exits **3** and names the file and line that still configures the
database, because a worker is still connecting to it. `--keep-config` makes that
an explicit choice and exits 0; it is required outright for an SQL-only removal
over `--database-url` with no local installation.

### `model ...`

```bash
postvec model ls                       # installed inventory, with STATE
postvec model ls --available           # the registry catalogue
sudo postvec model pull baai-bge-m3    # install — DEACTIVATED, not serving
sudo postvec model activate baai-bge-m3   # enable on disk + load
sudo postvec model deactivate baai-bge-m3 # unload + disable on disk
sudo postvec model upgrade --all       # replace bytes, keep activation state
sudo postvec model rm baai-bge-m3      # unload + delete
postvec model show baai-bge-m3 --verify
```

**Installing is not activating, and both states persist.** A fresh install's
descriptor is written `enabled: false`, so the model is neither hot-loaded nor
scan-loaded at the next PostgreSQL restart; `activate` and `deactivate` rewrite
that one field (and reconcile the install receipt over it, so a deactivated
model still verifies). Nothing else — not `pull`, not `upgrade`, not a restart —
changes it.

`activate NAME` enables the model's deactivated dependency closure with it,
because the engine refuses to load a closure containing a deactivated model.
`deactivate` has no `--all`. Both refuse package-owned and manual directories,
and both refuse a model named in an explicit `postvec.embedded_models`, where a
disabled entry is a startup error rather than a skip.

`deactivate` and `rm` scan `postvec.registry` in every configured database for
columns that would **lose their embedding route**. The question is not "does a
column name this model" but "can this column still be embedded afterwards",
evaluated against what the engine would actually hold: a column on a
convert-only space is served by a converter plus `embed-bridge`, neither of
which carries that space's name. Finding any is a loud acknowledgement, not a
refusal: interactive use types the names back, non-interactive use passes
`--acknowledge-in-use` alongside `--yes`. `--force` (break other *models*) and
`--yes` (confirm the mutation) deliberately do not stand in for it.

## Exit codes

| Code | Meaning |
|---:|---|
| 0 | completed; a doctor report had no FAIL (and no WARN under `--strict`) |
| 1 | apply failure, postcondition failure, or a doctor FAIL |
| 2 | invalid invocation, or a prompt that could not be answered |
| 3 | partial result: a target was not changed, or work the command promised was left undone |
| 4 | changes are in place but the requested restart was deferred |

## Privileges

Host configuration needs root; local `peer` authentication needs the cluster
owner's OS identity. Running `sudo postvec setup` satisfies both: the parent
keeps root for the filesystem and the service, and database work runs in a child
of the same binary that has irreversibly dropped to the cluster owner. The child
speaks a line-delimited JSON protocol over pipes, so no connection URI ever
appears in `/proc/*/cmdline`, and it announces every mutating operation on
stderr.

Read-only model commands do not need that session. `model ls`, `model ls
--available` and `model show` fall back to the owned `99-postvec.conf`
snippet (mode 0644) and the world-readable engine root when peer
authentication fails. Cluster-targeted `model pull` / `upgrade` / `rm`
/ `activate` / `deactivate` still need a database login (they refresh
`postvec.models`, and `deactivate` / `rm` read `postvec.registry`);
`--path DIR` is files only. `sudo` is still required to *write* a
root-owned engine root and to change cluster configuration.

With `--database-url` (or `POSTVEC_DATABASE_URL`) the operator's own
authentication is used unchanged — including `sslmode`, which is never silently
downgraded.

A URI on its own says nothing about this machine, so **a URI without
`--cluster` or `--pg-config` is treated as a remote-only target**: the report
covers what the connection can answer and skips everything host-shaped, rather
than pairing a remote server's settings with whatever happens to be installed
here.

Pairing the two is possible, but has to be asked for explicitly with
`--cluster` — and the pairing is then verified by opening a **second connection
through the cluster's own socket** and comparing it with the supplied one on two
values, because neither is sufficient alone:

- the **system identifier** identifies a replication *lineage*. A physical
  standby, or any restored copy, carries its primary's — so on its own it would
  match a primary against its own standby, which is exactly the pairing most
  likely to reconfigure the wrong host;
- the **exact postmaster start time** identifies the running instance.

It has to be a second connection: both sides then answer the same question, so
the comparison is exact. The cheaper-looking source, `postmaster.pid`, holds
`MyStartTime` — whole seconds, captured earlier in startup than the
`PgStartTime` that `pg_postmaster_start_time()` returns — so comparing the two
would reject the right postmaster whenever startup crossed a second boundary,
and accept a different same-lineage postmaster that started in the same second.

Paths, ports and major versions are defaults two unrelated hosts share by
construction, so they establish nothing. An installation selected with
`--pg-config` has no independently known local endpoint to probe, so pairing it
with a URI is refused outright.

`setup` and `uninstall` refuse when the pairing cannot be proven; the cost of
being wrong is restarting the wrong database server. `doctor` does not abort —
its findings may be exactly what you need — but reports `cluster.identity` as a
blocking check, so a report that mixes one server's database state with
another's files can never come back green.

## Development

```bash
cargo test -p postvec-cli --all-targets
cargo clippy -p postvec-cli --all-targets -- -D warnings
cargo fmt -p postvec-cli -- --check
```

The tests are hermetic: check logic is pure over observed
[facts](src/facts.rs), so diagnosing a broken cluster needs no cluster, and the
network probes use ephemeral local listeners. The live PostgreSQL 16/17/18
matrix is documented in [`docs/install.md`](../docs/install.md).

### Layout

| Module | Responsibility |
|---|---|
| `cli` | the public argument contract |
| `validate` | every operator-supplied value, before it reaches config or SQL |
| `facts` | observed data — the boundary between IO and judgement |
| `checks` | pure evaluation of facts into stable check results |
| `cluster` | discovery, `postgres -C` validation, restart |
| `config` | the owned snippet, ownership state, host lock |
| `db` | typed requests, constant SQL, the privilege-dropped agent |
| `engine` | read-only probes of remote nodes and the embedded engine |
| `commands` | plan, confirm, apply, verify |

`setup`'s preflight, `setup`'s post-restart verification and `doctor` all run
the same collectors and the same checks. Three independent notions of "healthy"
would drift.

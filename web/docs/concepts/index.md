---
title: How it works
description: Runtime shape, write path and common operational constraints.
---

# How it works

Most operational surprises come from the same few facts: empty vectors
right after `INSERT`, a worker that never started and `search()` that
takes too long.

## Runtime

<figure class="pvd">
<svg viewBox="0 0 640 568" role="img" aria-labelledby="pvd-runtime-title pvd-runtime-desc">
<title id="pvd-runtime-title">postvec runtime shape</title>
<desc id="pvd-runtime-desc">Inside the PostgreSQL cluster, a trigger on the source table enqueues work into postvec.jobs; a per-database worker spawned by the launcher claims the job and writes the vector back to the same table. The worker calls an inference engine that is either the launcher on loopback or a remote postvec-server.</desc>
<defs>
<marker id="pvd-rt-head" markerWidth="8" markerHeight="6" refX="7" refY="3" orient="auto">
<polygon class="head" points="0 0, 8 3, 0 6"/>
</marker>
<marker id="pvd-rt-head-accent" markerWidth="8" markerHeight="6" refX="7" refY="3" orient="auto">
<polygon class="head-accent" points="0 0, 8 3, 0 6"/>
</marker>
</defs>

<rect class="s-mask" width="640" height="568"/>

<rect class="s-zone" x="24" y="48" width="568" height="372" rx="8"/>
<rect class="s-mask" x="66" y="42" width="104" height="12"/>
<text class="t-zone" x="118" y="51" text-anchor="middle">POSTGRESQL CLUSTER</text>

<path class="c" d="M 152,160 V 208" marker-end="url(#pvd-rt-head)"/>
<path class="c" d="M 152,272 V 320" marker-end="url(#pvd-rt-head)"/>
<path class="c" d="M 240,344 H 304 Q 312,344 312,336 V 136 Q 312,128 304,128 H 240" marker-end="url(#pvd-rt-head)"/>
<path class="c c-dash" d="M 472,272 V 352 Q 472,360 464,360 H 240" marker-end="url(#pvd-rt-head)"/>
<path class="c c-accent" d="M 152,384 V 440 Q 152,448 160,448 H 312 Q 320,448 320,456 V 468" marker-end="url(#pvd-rt-head-accent)"/>

<rect class="s-mask" x="160" y="176" width="60" height="12"/>
<text class="t-arrow" x="190" y="185" text-anchor="middle">TRIGGER</text>
<rect class="s-mask" x="160" y="288" width="44" height="12"/>
<text class="t-arrow" x="182" y="297" text-anchor="middle">CLAIM</text>
<rect class="s-mask" x="320" y="226" width="56" height="12"/>
<text class="t-arrow" x="348" y="235" text-anchor="middle">WRITE-BACK</text>
<rect class="s-mask" x="480" y="300" width="52" height="12"/>
<text class="t-arrow" x="506" y="309" text-anchor="middle">SPAWNS</text>
<rect class="s-mask" x="160" y="398" width="40" height="12"/>
<text class="t-arrow" x="180" y="407" text-anchor="middle">gRPC</text>

<rect class="s-mask" x="64" y="96" width="176" height="64" rx="6"/>
<rect class="s-store" x="64" y="96" width="176" height="64" rx="6"/>
<rect class="s-tag s-tag-muted" x="76" y="106" width="40" height="12" rx="2"/>
<text class="t-tag" x="96" y="115" text-anchor="middle">TABLE</text>
<text class="t-name" x="152" y="136" text-anchor="middle">Source table</text>
<text class="t-sub" x="152" y="150" text-anchor="middle">body → body_semantic</text>

<rect class="s-mask" x="64" y="208" width="176" height="64" rx="6"/>
<rect class="s-store" x="64" y="208" width="176" height="64" rx="6"/>
<rect class="s-tag s-tag-muted" x="76" y="218" width="40" height="12" rx="2"/>
<text class="t-tag" x="96" y="227" text-anchor="middle">QUEUE</text>
<text class="t-name" x="152" y="248" text-anchor="middle">postvec.jobs</text>
<text class="t-sub" x="152" y="262" text-anchor="middle">coalesced per row</text>

<rect class="s-mask" x="64" y="320" width="176" height="64" rx="6"/>
<rect class="s-focal" x="64" y="320" width="176" height="64" rx="6"/>
<rect class="s-tag s-tag-accent" x="76" y="330" width="48" height="12" rx="2"/>
<text class="t-tag t-accent" x="100" y="339" text-anchor="middle">WORKER</text>
<text class="t-name" x="152" y="360" text-anchor="middle">Per-database worker</text>
<text class="t-sub" x="152" y="374" text-anchor="middle">one per postvec.database</text>

<rect class="s-mask" x="384" y="208" width="176" height="64" rx="6"/>
<rect class="s-node" x="384" y="208" width="176" height="64" rx="6"/>
<rect class="s-tag s-tag-ink" x="396" y="218" width="56" height="12" rx="2"/>
<text class="t-tag" x="424" y="227" text-anchor="middle">LAUNCHER</text>
<text class="t-name" x="472" y="248" text-anchor="middle">Launcher</text>
<text class="t-sub" x="472" y="262" text-anchor="middle">shared_preload_libraries</text>

<rect class="s-mask" x="160" y="468" width="320" height="64" rx="6"/>
<rect class="s-ext" x="160" y="468" width="320" height="64" rx="6"/>
<rect class="s-tag s-tag-muted" x="172" y="478" width="44" height="12" rx="2"/>
<text class="t-tag" x="194" y="487" text-anchor="middle">ENGINE</text>
<text class="t-name" x="320" y="508" text-anchor="middle">Inference engine</text>
<text class="t-sub" x="320" y="522" text-anchor="middle">launcher loopback, or a remote postvec-server</text>
</svg>
</figure>

`CREATE EXTENSION` exposes the SQL surface immediately. Automatic sync
still needs the preloaded launcher and a worker for that database. If
`shared_preload_libraries` does not include `postvec`, there is no
worker.

## Write path

<figure class="pvd">
<svg viewBox="0 0 656 208" role="img" aria-labelledby="pvd-write-title pvd-write-desc">
<title id="pvd-write-title">The write path, and the window where the vector is empty</title>
<desc id="pvd-write-desc">Four steps left to right: the application commits, a trigger enqueues the row into postvec.jobs, the worker claims it and calls inference, and the vector is written back. A bracket spans from the commit to the write-back marking the window in which the stored vector is null or stale.</desc>
<defs>
<marker id="pvd-wp-head" markerWidth="8" markerHeight="6" refX="7" refY="3" orient="auto">
<polygon class="head" points="0 0, 8 3, 0 6"/>
</marker>
</defs>

<rect class="s-mask" width="656" height="208"/>

<path class="c" d="M 152,80 H 184" marker-end="url(#pvd-wp-head)"/>
<path class="c" d="M 312,80 H 344" marker-end="url(#pvd-wp-head)"/>
<path class="c" d="M 472,80 H 504" marker-end="url(#pvd-wp-head)"/>

<path class="c c-rule" d="M 88,128 V 148 H 568 V 128"/>
<rect class="s-mask" x="262" y="156" width="132" height="12"/>
<text class="t-arrow" x="328" y="165" text-anchor="middle">VECTOR NULL OR STALE</text>

<rect class="s-mask" x="24" y="48" width="128" height="64" rx="6"/>
<rect class="s-node" x="24" y="48" width="128" height="64" rx="6"/>
<rect class="s-tag s-tag-ink" x="36" y="58" width="16" height="12" rx="2"/>
<text class="t-tag" x="44" y="67" text-anchor="middle">1</text>
<text class="t-name" x="88" y="88" text-anchor="middle">Commit</text>
<text class="t-sub" x="88" y="102" text-anchor="middle">INSERT / UPDATE</text>

<rect class="s-mask" x="184" y="48" width="128" height="64" rx="6"/>
<rect class="s-node" x="184" y="48" width="128" height="64" rx="6"/>
<rect class="s-tag s-tag-ink" x="196" y="58" width="16" height="12" rx="2"/>
<text class="t-tag" x="204" y="67" text-anchor="middle">2</text>
<text class="t-name" x="248" y="88" text-anchor="middle">Enqueue</text>
<text class="t-sub" x="248" y="102" text-anchor="middle">coalesced per row</text>

<rect class="s-mask" x="344" y="48" width="128" height="64" rx="6"/>
<rect class="s-focal" x="344" y="48" width="128" height="64" rx="6"/>
<rect class="s-tag s-tag-accent" x="356" y="58" width="16" height="12" rx="2"/>
<text class="t-tag t-accent" x="364" y="67" text-anchor="middle">3</text>
<text class="t-name" x="408" y="88" text-anchor="middle">Claim + embed</text>
<text class="t-sub" x="408" y="102" text-anchor="middle">re-reads the text</text>

<rect class="s-mask" x="504" y="48" width="128" height="64" rx="6"/>
<rect class="s-node" x="504" y="48" width="128" height="64" rx="6"/>
<rect class="s-tag s-tag-ink" x="516" y="58" width="16" height="12" rx="2"/>
<text class="t-tag" x="524" y="67" text-anchor="middle">4</text>
<text class="t-name" x="568" y="88" text-anchor="middle">Write-back</text>
<text class="t-sub" x="568" y="102" text-anchor="middle">body_semantic</text>
</svg>
</figure>

Between the application commit and worker write-back, the vector is NULL
or stale. A committed write arms an at-commit latch and nudges the
worker, so the gap is normally just inference time.
`postvec.poll_interval_ms` (default 5000) is only the backstop. See
[eventual consistency](/docs/concepts/consistency).

`search()` is the exception: it embeds the **query** synchronously.
Query embedding is the only synchronous inference step on the search
path.

## Durable and transient state

| Durable (dumped) | Cache (not dumped) |
|---|---|
| registry, jobs, jobs_dead, migrations | `postvec.models`, worker heartbeat |

A restore needs matching files, cluster configuration, a model refresh
and `doctor`. [Backup](/docs/guides/backup) has the checklist.

## Operational constraints

1. **Vectors fill after commit.** `status()` reports readiness.
2. **The worker needs preload and a restart.** Every database named in
   `postvec.database` must exist. A missing name makes the worker fail
   and respawn about every 15 seconds.
3. **ANN indexes are opt-in.** Missing ANN is a common reason search is
   slow. `index_mode => 'auto'` is available; the build is blocking.
4. **A new library and `ALTER EXTENSION` belong in one window.** Until
   then the worker pauses.
5. **`adopt()`'s `model` is an assertion.** The wrong name makes
   `search()` embed into an incompatible space.

## Related documentation

- [Embedded vs remote](/docs/concepts/modes)
- [Embedding debt and vector lock-in](/docs/concepts/lock-in)
- [Eventual consistency](/docs/concepts/consistency)
- [Enable a column](/docs/guides/enable)

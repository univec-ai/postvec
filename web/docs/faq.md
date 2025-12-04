---
title: FAQ
description: Short answers to the questions a first-time operator actually asks.
---

# FAQ

## Does this work on RDS / Aurora / Cloud SQL / Azure Database?

No, if the service will not load `shared_preload_libraries = 'postvec'`.
That is structural, not a packaging gap. Use a host you control, or the
[Docker image](/docs/install/docker).

## What are embedding debt and vector lock-in?

[Defined here](/docs/concepts/lock-in). Lock-in is "these vectors only
work with one model." Debt is what that costs you every time you want
to change.

## Do I need a UniVec API key?

Not to embed with the bundled MiniLM model, and not to search. You need
a key (`postvec login`) to pull **private**-catalogue models — the full
embed suite and 100+ conversion pairs. Organisation accounts live at
[univec.ai](https://univec.ai).

## Can I stay on ada-002 and still search?

Yes. [Adopt the column](/docs/guides/adopt) and name that space;
[embed-bridge](/docs/guides/bridge) produces query vectors in it. No
corpus re-embed, no OpenAI call.

## Does embedded mode call out to the internet?

Not for inference. Models you already have on disk run in the launcher.
`model pull` / `ls --available` contact the registry when you ask them
to. Air-gapped hosts are [supported](/docs/models/air-gapped).

## Where do OpenAI / Gemini keys go?

They don't. Not in PostgreSQL. Embedded mode has no place to put them.
Remote mode talks to *your* ninference, not to those providers.

## Why is the vector NULL right after INSERT?

[Eventual consistency](/docs/concepts/consistency). Wait for
`pending_jobs = 0`.

## Why is search slow?

There is no ANN index. [Indexes](/docs/guides/indexes).

## Why did search ignore my new row's meaning and just keyword-match?

Either the vector is not filled yet, or query embedding failed and
`search_degrade_to_fts` kicked in.

## Can I keep my existing `embedding vector(768)` column?

Yes. [`adopt()`](/docs/guides/adopt). postvec will not drop it later.

## Can I change the chunk size later?

Not live. `disable` (optionally `drop_destination`), then `enable`
again.

## Does `model upgrade` rewrite my column?

No. Only [`migrate()`](/docs/guides/migrate) or a re-embed does that.

## Is the extension AGPL?

No. PostgreSQL License, for the extension, the CLI, and their packages.

## Will `apt remove` drop my data?

No. Packages never run `DROP EXTENSION` or delete user data. You still
need [uninstall](/docs/install/uninstall) to take the worker off a
database.

## Can I run two PostgreSQL majors on one host?

Yes. The CLI is a separate package so `postgresql-16-postvec` and
`postgresql-18-postvec` can coexist. Select with `--cluster`.

## Where are the downloads hosted?

GitHub Releases and GHCR today. See [Download](/download). There is no
signed apt/yum repo yet.

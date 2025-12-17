---
title: FAQ
description: Short answers to common postvec questions.
---

# FAQ

## Are RDS, Aurora, Cloud SQL and Azure Database supported?

No, when the service will not load `shared_preload_libraries = 'postvec'`.
A self-managed host or the [Docker image](/docs/install/docker) is required.

## What are embedding debt and vector lock-in?

[Defined here](/docs/concepts/lock-in). Vector lock-in is the dependency of
stored vectors on one model space. Embedding debt is the accumulated cost and
risk of changing that dependency.

## Is a UniVec API key required?

No key is required for the bundled MiniLM model or for search. A key and
`postvec login` are required to pull **private-catalogue** models. The
authenticated route is identity-only (verified account, unexpired key). A
dedicated key with a $0 spending limit is the recommended credential. Account details:
[univec.ai](https://univec.ai).

## Can an ada-002 corpus remain unchanged?

Yes. [Adopt the column](/docs/guides/adopt) and name that space;
[embed-bridge](/docs/guides/bridge) produces query vectors in it without a
corpus re-embed or OpenAI call.

## Does embedded inference require internet access?

No. Models already present on disk run in the launcher. `model pull` and
`ls --available` contact the registry when invoked. Air-gapped hosts are
[supported](/docs/models/air-gapped).

## Where do OpenAI / Gemini keys go?

They are not stored in PostgreSQL. Embedded mode does not use provider
credentials. Remote mode talks to the configured ninference nodes.

## Why is the vector NULL right after INSERT?

[Eventual consistency](/docs/concepts/consistency). Wait for
`pending_jobs = 0`.

## Why is search slow?

There is no ANN index. [Indexes](/docs/guides/indexes).

## Why can a new row produce only a lexical match?

Either the vector is not filled yet, or query embedding failed and
`search_degrade_to_fts` kicked in.

## Can an existing `embedding vector(768)` column be retained?

Yes. [`adopt()`](/docs/guides/adopt). postvec will not drop it later.

## Can chunk size be changed later?

Not live. `disable` (optionally `drop_destination`), then `enable`
again.

## Does `model upgrade` rewrite stored vector columns?

No. Only [`migrate()`](/docs/guides/migrate) or a re-embed does that.

## Is the extension AGPL?

No. The extension, CLI and packages use the PostgreSQL License.

## Does `apt remove` drop database data?

No. Packages do not run `DROP EXTENSION` or delete user data. The
[uninstall procedure](/docs/install/uninstall) removes the worker from a
database.

## Can two PostgreSQL majors run on one host?

Yes. The CLI is a separate package so `postgresql-16-postvec` and
`postgresql-18-postvec` can coexist. Select with `--cluster`.

## Where are the downloads hosted?

GitHub Releases and GHCR are the configured publication channels. See
[Release artifacts](/download) for the live/preview state. There is no signed
apt/yum repository.

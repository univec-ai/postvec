---
title: License
description: PostgreSQL License for the extension, Business Source License 1.1 for postvec-server, and the postvec pro tier.
---

# License

The extension, the CLI and their packages use the **PostgreSQL License** in either
inference mode. `postvec-server` is source-available under the **Business Source
License 1.1** (`BUSL-1.1`), and each of its versions becomes PostgreSQL-licensed
four years after release.

Paid use follows one rule: production use by an organization. Hobby projects,
study, evaluation, CI and every non-production environment are free, including
inside a company. Hosting postvec-server for third parties, redistribution and
OEM need an enterprise order.

| Component | License | Free | Paid |
|---|---|---|---|
| Extension, CLI, engine, packages, PostgreSQL images | PostgreSQL License | Any use, either inference mode | - |
| **postvec-server** | Business Source License 1.1 | Development, testing, CI, staging, teaching, research; personal, noncommercial production use; one 30-day production evaluation per organization, including its affiliates under common control | Production use by an organization: [postvec pro](/server#plans). Hosting or embedding for third parties: platform/OEM agreement |
| Public catalogue models | Each model's own license | Any use | - |
| Private catalogue (converters) | UniVec Model Terms | Noncommercial use and a 30-day non-production evaluation | Commercial use, either mode: postvec pro |
| Hosted embed and convert API | Service terms | - | Pay as you go |
| Support SLA, air-gapped catalogue, custom pairs, redistribution, OEM | Enterprise order | - | Contract |

## postvec pro

One self-serve tier, €30/month excluding tax, billed to an organization:

- Production rights to `postvec-server` for that organization, with no limit on
  nodes or environments
- Commercial rights to the private catalogue (all converters)
- €30/month of UniVec API credit, non-rolling, for hosted embed and convert calls
  or for UniVec as an external provider
- Best-effort support

Subscribe from the [UniVec dashboard](https://univec.ai/dashboard/postvec).
[Plans](/server#plans) compares the free use, postvec pro and enterprise terms.

Cancelling ends production use of the server and the private converters after a
30-day transition. Generated vectors stay in the database, and embedded mode with
public models keeps running.

Redistribution, resale of inference or conversion, managed-database resale,
hosting the server for third parties and OEM need an enterprise order.

## What this means in practice

| Situation | What you run | What you pay |
|---|---|---|
| Self-hosted PostgreSQL, local models, public catalogue | Extension and `postvec setup --embedded` | Nothing |
| Self-hosted, inference in [postvec-server](/docs/server/) | Extension and postvec-server | Personal and non-production use free; Pro for production use by an organization |
| Hosted OpenAI, Cohere, Bedrock, Gemini, Mistral, OpenRouter or UniVec (your key) | Extension or [postvec-server](/docs/server/) plus a `providers.d` file | The provider's bill |
| RDS, Aurora, Cloud SQL, Azure, Supabase, Neon | `postvec-server` in [managed](/docs/server/managed) mode | Pro, from the first production deployment |
| Personal, noncommercial production | Either host | Nothing |
| 30-day production evaluation, one per organization and its affiliates | Either host | Nothing for those 30 days |
| Commercial use of a private converter | Either mode | Pro |
| Air-gapped catalogue, SLA, custom pairs, OEM | Server and a contract | Enterprise |

## How compliance works

The extension has no runtime key, telemetry or usage clause; it reports nothing.
Private-model terms are accepted at `postvec model pull --accept-license`.
postvec-server compliance is contractual: the package, the image label and the
release manifest carry the license, and the server itself contains no check.

Operative texts: [LICENSING.md](https://github.com/univec-ai/postvec/blob/main/LICENSING.md)
in the repository.

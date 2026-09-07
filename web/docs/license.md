---
title: License
description: PostgreSQL License for the extension, Business Source License 1.1 for postvec-server and postvec Pro.
---

# License

| Component | License | Free | Paid |
|---|---|---|---|
| Extension, CLI, engine, packages, PostgreSQL images | PostgreSQL License | Any use, either inference mode | - |
| **postvec-server** | Business Source License 1.1 (source-available) | Development, testing, CI, staging, teaching, research; personal production use; one 30-day production evaluation per organization | Production use by an organization: [postvec Pro](https://univec.ai). Hosting or embedding for third parties: enterprise |
| Public catalogue models | Each model's own license | Any use | - |
| Private catalogue (converters) | UniVec Model Terms | Noncommercial use and a 30-day evaluation | Commercial use, either mode: postvec Pro |
| Hosted embed / convert API | Service terms | - | Pay as you go |
| Support SLA, air-gapped catalogue, custom pairs, redistribution, OEM | Enterprise order | - | Contract |

What you pay for is production use by an organization. Hobby, study,
evaluation and every non-production environment are free, including
inside a company. Each `postvec-server` version converts to the
PostgreSQL License four years after release.

The extension is unrestricted at runtime. It includes local inference,
public-catalogue models and BYOK connectors for OpenAI, Cohere, Amazon
Bedrock, Gemini, Mistral, OpenRouter and UniVec. Private-model terms are
accepted at `model pull` (`--accept-license`). Server compliance is
contractual.

## postvec Pro

One self-serve tier, €30/month excluding tax, billed to an
organization:

- Production rights to `postvec-server` for that organization, unlimited
  nodes and environments
- Commercial rights to the private catalogue
- €30/month of direct UniVec API credit, non-rolling
- Best-effort support

Generated vectors stay after cancellation. Embedded mode with public
models continues to run. Redistribution, resale of inference or
conversion, managed-database resale, hosting the server for third
parties and OEM need an enterprise order.

## What this means in practice

| Situation | What you run | What you pay |
|---|---|---|
| Self-hosted PostgreSQL, local models, public catalogue | Extension + `postvec setup --embedded` | Nothing |
| Self-hosted, inference in [postvec-server](/docs/server/) | Extension + postvec-server | Pro for organization production; personal and non-production free |
| Hosted OpenAI, Cohere, Bedrock, Gemini, Mistral, OpenRouter or UniVec (your key) | Extension or [postvec-server](/docs/server/) + a `providers.d` file | The provider's bill |
| RDS, Aurora, Cloud SQL, Azure, Supabase, Neon | `postvec-server` in [managed](/docs/server/managed) mode | Pro, from the first production deployment |
| Personal production | Either | Nothing |
| 30-day production evaluation (one per organization) | Either | Nothing for those 30 days |
| Commercial use of a private converter | Either mode | Pro |
| Air-gapped catalogue, SLA, custom pairs, OEM | Server + contract | Enterprise |

Operative texts: [LICENSING.md](https://github.com/univec-ai/postvec/blob/main/LICENSING.md)
in the repository.

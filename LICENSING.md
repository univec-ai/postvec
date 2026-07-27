# Licensing

This repository holds two products under two licenses. This file is the
per-directory index; the license texts are where the table points.

| Path | License | Text |
|---|---|---|
| `postvec/` | PostgreSQL License | [LICENSE](LICENSE), [postvec/LICENSE](postvec/LICENSE) |
| `postvec-cli/` | PostgreSQL License | [LICENSE](LICENSE) |
| `packaging/postvec/` | PostgreSQL License | [LICENSE](LICENSE) |
| `engine/`, `shared/` | PostgreSQL License | [LICENSE](LICENSE) |
| `providers/` | PostgreSQL License | [LICENSE](LICENSE) |
| `proto/` | PostgreSQL License | [LICENSE](LICENSE) |
| `registry/schema/`, `registry/client/` | PostgreSQL License | [LICENSE](LICENSE) |
| `docs/`, `web/` | PostgreSQL License | [LICENSE](LICENSE) |
| `postvec-server/` | Business Source License 1.1 | [postvec-server/LICENSE](postvec-server/LICENSE) |

The release gate (`packaging/postvec/scripts/assert-versions.sh`) reads the
`postvec-server/` row above and refuses a release where it disagrees with the
crate manifest, the license text and the `SERVER_LICENSE` pin. Keep the row
in the form `` | `postvec-server/` | <SPDX id> | ... `` .

## PostgreSQL License: the extension and everything it links

The PostgreSQL License covers everything published as a postvec release
artifact: the extension, the `postvec` command, their packaging, the inference
engine embedded mode links into the database process, the provider gateway,
the wire contract and the registry crates. The grant is unconditional. It does
not depend on which mode you run postvec in, on how much traffic you serve,
or on whether you charge for what you build with it. The extension has no
license check, no telemetry and no phone-home.

## Business Source License 1.1: postvec-server

`postvec-server/` is a separate program, the standalone inference node for
remote mode. It is **source-available** under the Business Source License 1.1
(BSL), not open source. The parameters are in
[postvec-server/LICENSE](postvec-server/LICENSE); in short:

| Use | Terms |
|---|---|
| Development, testing, CI, staging, teaching, research | Free, for anyone |
| Production use by an individual for personal, noncommercial purposes | Free |
| Production evaluation by an organization | Free for one 30-day period per organization |
| Production use by or for an organization | Commercial license required: a [postvec Pro](https://univec.ai) subscription, or an enterprise order |
| Offering it to third parties as a hosted, managed or embedded service | Commercial license required: enterprise or platform agreement |
| Copying, modifying, forking, redistributing the source | Allowed under the same license, notices intact |

Each released version converts to the **PostgreSQL License** four years after
it is published (the BSL change date), and the same conversion happens
automatically on the fourth anniversary of a version's first public release.

The server links the PostgreSQL-licensed crates above and does not change
their terms. A compatible node written against the public `proto/` contract
owes nothing to this license. The server contains no license check and no
phone-home; compliance is contractual.

## Models

Models fetched by `postvec model pull` are not part of this repository and
carry their own terms, shown and recorded at pull time (`--accept-license`):

- The public catalogue: open-weight models under their upstream licenses.
- The private catalogue (Univec conversion models): the Univec Private
  Catalogue Model Terms, free for noncommercial use and evaluation; commercial
  use requires a subscription from https://univec.ai. Those terms attach to
  the model files, not to this software, and apply in either inference mode.

## Contributions

- Contributions to the PostgreSQL-licensed directories are accepted under the
  [Developer Certificate of Origin](https://developercertificate.org/)
  (`git commit -s`), see [CONTRIBUTING.md](CONTRIBUTING.md).
- Contributions to `postvec-server/` require a contributor license agreement
  granting Univec Ltd the right to license the code commercially. Until that
  agreement is published, external pull requests to `postvec-server/` are not
  accepted; open an issue instead.

## Third-party components

Redistributed third-party work keeps its own license; see [NOTICE](NOTICE).

## Licensor

Univec Ltd, Dublin, Ireland. Registered in Ireland, CRO number 812890.
Licensing questions: legal@univec.ai.

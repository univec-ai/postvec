# postvec packaging

Turns a commit of [`postvec/`](../../postvec),
[`postvec-cli/`](../../postvec-cli) and [`postvec-server/`](../../postvec-server)
into published installation artifacts: `.deb` and `.rpm` packages per
PostgreSQL major, PostgreSQL container images with postvec preinstalled
(including a local image that runs the inference engine and a bundled model
in-process) and, for remote mode, the `postvec-server` inference node as a
package and as an image of its own.

Two licences travel in one release. The extension, CLI and database images
use the PostgreSQL License. `postvec-server` uses the identifier pinned as
`SERVER_LICENSE` in `versions.env`: the package declares it, the image labels
it, the manifest records it under `licenses`, and `assert-versions.sh`
refuses a release where the crate, the licence text, the `LICENSING.md` index
and the pin disagree.

This directory is separate from UniVec's internal application build pipeline.
Public database packages have a different version, platform matrix, signing
boundary and rollback story.

---

## The two-minute version

```bash
# The fast checks — no Docker, no compiler, seconds:
tests/unit-test.sh

# Everything, for this machine's architecture (default: debian12 × PG 16/17/18):
packaging/postvec/scripts/release.sh
# scripts/release.sh --all-distros              # debian12 ubuntu2204 ubuntu2404 el9
# scripts/release.sh --distro ubuntu2404 --pg 18

# Or one cell at a time:
scripts/assert-versions.sh
scripts/build-onnxruntime-bundle.sh --arch amd64
# The node's dashboard, built once in the pinned node container (--host uses
# the host's npm instead) and packaged into postvec-server.
scripts/build-ui-bundle.sh
# `--with-server` makes this the shared-package cell: it also compiles the
# postvec-server node (PostgreSQL-independent, like the CLI beside it).
scripts/build-extension-stage.sh --distro debian12 --pg 18 --arch amd64 --with-server
# The bundled model is pulled from the registry, so it needs a postvec binary;
# the stage above just built one. `cargo build -p postvec-cli` also works.
scripts/build-model-bundle.sh    --cli build/debian12-pg18-amd64/cli/postvec
scripts/build-packages.sh        --distro debian12 --pg 18 --arch amd64
scripts/verify-package.sh        dist/common/debian12-amd64/*.deb \
                                 dist/noarch/debian12/*.deb \
                                 dist/extension/debian12-pg18-amd64/*.deb
scripts/build-image.sh --pg 18 --variant local --load
tests/image-smoke-test.sh ghcr.io/univec-ai/postvec:0.1.0-1-pg18-local
# The node's image, composed from the packages above, and tested on its own —
# then the remote database image is tested against it.
scripts/build-server-image.sh --arch amd64 --load
tests/server-image-test.sh ghcr.io/univec-ai/postvec-server:0.1.0-1
scripts/build-image.sh --pg 18 --variant remote --load
tests/image-smoke-test.sh --variant remote \
    --server-image ghcr.io/univec-ai/postvec-server:0.1.0-1 \
    ghcr.io/univec-ai/postvec:0.1.0-1-pg18-remote
scripts/write-release-manifest.sh --expect-distros debian12 \
                                 --expect-majors 18 --expect-arches amd64
```

Prerequisites: Docker, `python3`, `jq` and the usual binutils. The compiler
lives in a builder image and nfpm runs from a pinned container.

### What a user needs first

postvec's packages name an exact PostgreSQL major and pgvector 0.8, and **no
supported distribution ships that combination in its own archives**: Debian 12
carries PostgreSQL 15, Ubuntu 22.04 carries 14, EL9 carries 13 in a module that
shadows everything else. So `apt install ./postgresql-18-postvec_*.deb` on an
untouched host fails at dependency resolution — the PostgreSQL project's own
instructions require the same repositories for the same reason.

`scripts/postvec-prerequisites.sh` is published as a release asset and does
exactly that and nothing else. It installs no postvec package, touches no
cluster, writes no PostgreSQL configuration, is idempotent, and requires
consent.

**It is attested in its own right**, not merely listed in an attested
`SHA256SUMS`, because it is the one asset the instructions ask a user to
*execute* — and the documented order is verify, read, then run:

```bash
# Needs the GitHub CLI >= 2.49; `gh attestation` does not exist before that and
# a distribution-packaged gh is usually older.
gh attestation verify postvec-prerequisites.sh --repo univec-ai/postvec \
   --signer-workflow univec-ai/postvec/.github/workflows/postvec-release.yml
less postvec-prerequisites.sh
sudo bash ./postvec-prerequisites.sh --pg 18
```

`--signer-workflow`, because "signed by something in this repository" is a
weaker claim than "signed by this repository's release workflow".
`bash ./…`, because a GitHub release asset does not keep its executable bit.
`less`, because `--print` is still the program running — useful once you trust
it, not a substitute for reading it. The manifest records it under
`release_assets`, and the release job verifies both the attestation and that the
manifest's hash is the published file.

What it configures, per family:

| | |
|---|---|
| Debian, Ubuntu | the PGDG key — downloaded, **fingerprint-verified**, and re-verified if a keyring already exists rather than assumed — plus one apt source |
| EL9 | CodeReady Builder and EPEL (both spelled differently per distribution — see below), then the PGDG repository RPM: downloaded, its **signature checked in a throwaway RPM database holding only PGDG's fingerprint-verified key**, then installed from the local file. Plus `module disable postgresql` |

The repository RPM is *not* pinned by digest here, unlike in the builders:
upstream's documented URL is `-latest` and moves by design, so a digest would
break on republication. The key does not move, so the key is the trust root —
and the check runs against a database containing *only* that key, because
`rpm --checksig` against the host database proves the package is signed by some
key the host already trusts, which on a machine carrying several vendors' keys
is a much weaker statement than it looks.

**Two things are not called the same thing across the EL9 family**, and that is
where a script tested only on AlmaLinux quietly fails on the system it
advertises:

| | AlmaLinux / Rocky | CentOS Stream 9 | subscribed RHEL 9 |
|---|---|---|---|
| CodeReady Builder | `crb` via `dnf config-manager` | `crb` via `dnf config-manager` | `codeready-builder-for-rhel-9-<arch>-rpms` via `subscription-manager` |
| EPEL | `epel-release` | `epel-release` **and** `epel-next-release` | Fedora's release RPM **by URL** — no `epel-release` package exists to install by name |

The script generates the right commands for each, in the right order (CRB before
EPEL, which EPEL's RHEL instructions require), and `tests/unit-test.sh` asserts
the generated commands for **every** declared target without a container each.

But generating the right text and surviving execution are different claims, and
only one of them has evidence for CentOS Stream and RHEL — so on a distribution
postvec has not rehearsed, the script **prints the commands and refuses to run
them**, naming `--force-untested` as the deliberate override.

"Rehearsed" means something specific and mechanically checked:
`tests/bootstrap-test.sh` runs the bootstrap **twice** in a clean container of
each distribution it will execute on, and asserts the key fingerprint was
verified, the repository RPM's signature was checked (EL), PostgreSQL 18 and
pgvector resolve afterwards, and the second run recognised the existing
configuration. It runs in CI, one job per distribution, against *current* base
images rather than pinned ones — a PGDG or EPEL change that breaks a user's
first command should fail a build, not a user.

| | bootstrap executed | full clean-host install |
|---|---|---|
| Debian 12 | yes | yes |
| Ubuntu 22.04 | yes | matrix only (nightly / release) |
| Ubuntu 24.04 | yes | matrix only (nightly / release) |
| AlmaLinux 9 | yes | yes |
| Rocky 9 | yes | no — treated as AlmaLinux's rebuild |
| CentOS Stream 9, RHEL 9 | no — refuses to execute | no |

Rocky is `TESTED=1` because it is *run*, not because it is a rebuild.

`tests/package-install-body.sh` runs **that script**, not a copy of its logic:
the documented first command is the one the clean-host test executes, so a
bootstrap that stops working fails CI rather than failing a new user.

`docker buildx` is required for a release: the builders use BuildKit cache
mounts (a Rust build without a Cargo cache is unbearable) and `--output` to
extract a scratch export stage, and it is the only way to build for another
architecture or to push. Without it, `build-extension-stage.sh` strips the
cache mounts and copies the result out of a container instead — slower,
identical output, local use only.

`versions.env` refuses to build anything publishable while its publication
metadata still says `example`. For a throwaway local build:
`POSTVEC_ALLOW_PLACEHOLDER_METADATA=1`.

---

## What gets published

| Package | Arch | Contents |
|---|---|---|
| `postvec-cli` | native | `/usr/bin/postvec`, docs, licence |
| `postgresql-16-postvec` / `-17-` / `-18-` | native | `postvec.so`, control file, install + upgrade SQL |
| `postvec-server` | native | `/usr/bin/postvec-server`, the crate's systemd unit plus a drop-in that points it at `/opt/postvec`, `/etc/postvec-server/config.json` (conffile), the `postvec-server` account. **Licence: `SERVER_LICENSE`**, not PostgreSQL |
| `postvec-onnxruntime` | native | pinned CPU ONNX Runtime under `/opt/postvec/libs` |
| `postvec-model-<suffix>` | `all` / `noarch` | one reviewed model from the registry's public channel, under `/opt/postvec/models`. `postvec-model-minilm-l6-v2` today; the name follows `BUNDLED_MODEL_PKG_SUFFIX` |
| `postvec-extras` | `all` / `noarch` | metapackage: the two above, pinned exactly. Not a complete install — still needs the CLI and the extension (or, on a node host, `postvec-server`) |
| `postvec-cli-dbgsym` / `-debuginfo` | native | detached symbols for the CLI |
| `postvec-server-dbgsym` / `-debuginfo` | native | detached symbols for the node |
| `postgresql-NN-postvec-dbgsym` / `-debuginfo` | native | detached symbols for that major's library |

All three builds emit line tables (`debug = "line-tables-only"` for the
extension, `CARGO_PROFILE_RELEASE_DEBUG` for the CLI and the node, because
Cargo ignores a profile in a workspace member). The pipeline splits them into
the detached packages above, attaches a GNU debug link, and strips the shipped
binaries — so a crash inside the extension, which takes a PostgreSQL backend
with it, produces a backtrace that names a file and a line for anyone who
installs the matching `-dbgsym`. The CLI's and the node's symbols are their own
packages because `/usr/bin/postvec` and `/usr/bin/postvec-server` are each
owned by one package regardless of how many majors are installed.

### The inference node

`postvec-server` is what `postvec.mode = 'grpc'` dials. It is
PostgreSQL-independent, so it is compiled in the **shared-package cell** — the
one `build-extension-stage.sh --with-server` produces, one per distribution and
architecture — and lives in the `common` root beside the CLI. The 24 extension
cells never build it.

The package installs files and creates the service account; it enables and
starts nothing, and its `postinst` only prints the commands. The unit is the
crate's own `postvec-server/systemd/postvec-server.service`, verbatim; the one
thing a package knows that the unit cannot — that *these* packages put the
engine root at `/opt/postvec` — is a systemd drop-in beside it
(`server/packaged.conf`), so `postvec-server postvec-extras` is a serving node
and `postvec model pull` on the node writes where the node looks. The
configuration file is a conffile (`noreplace` on RPM): the minimal
`server/config.json`, with the fully commented example under
`/usr/share/doc/postvec-server/`.

A plain repository install of `postvec-server` is a ready local node: it
`Recommends` `postvec-extras` (the pinned runtime and the reviewed model —
which is why the runtime is not recommended a second time on its own) and the
version-bounded CLI. Recommends, never Depends: a node that serves only
external providers needs neither runtime nor model, the engine dlopen()s the
runtime rather than linking it — `inspect-elf.sh` asserts that for the node
exactly as it does for the extension — the daemon never calls the CLI, and a
`Depends` on the CLI would put `postgresql-common` on every node.
`--no-install-recommends` is the slim, model-free node. `verify-package.sh`
asserts the exact relation — the metapackage, both CLI bounds, nothing else. The documented install lines name every file explicitly,
because an install from local files cannot fetch a Recommends by itself.

Removal follows each package manager's rules for the configuration file, and
the `postrm` says which: Debian `remove` keeps the conffile and `purge`
deletes it; RPM removes an unedited `config.json` and keeps an edited one as
`config.json.rpmsave`. `tests/node-install-test.sh` edits the file before
removal and asserts the edit survives on both families. Certificates, the
engine root and the account are never touched.

The certificate pair lives beside the configuration,
`/etc/postvec-server/server.{crt,key}` (key `root:postvec-server 0640`), not
under the crate's `<root>/certs` default: the packaged engine root is
root-owned and read-only for the service account, so a pair could neither be
created nor read there. `verify-package.sh` extracts the packaged
`config.json` and fails a package whose `ssl` block regressed to the relative
default or disabled TLS.

`tests/node-install-test.sh` is the inference-host topology: the node bundle on
a clean machine with no PGDG repository and no PostgreSQL, with the CLI and
then without it, asserting that nothing PostgreSQL arrives, that the packaged
configuration serves over TLS as the service account, and that removal keeps
the certificate, the engine root and the account.

The **image** is a composition of the same packages — `postvec-server`,
`postvec-cli`, `postvec-onnxruntime`, the model and the metapackage — on the
pinned Debian 12 base, published to `SERVER_IMAGE_REPOSITORY` as `<release id>`
(immutable) and `latest` (moving). It is tested twice: on its own by
`tests/server-image-test.sh` (composition, unprivileged, admin port unexposed,
ready only once the model answers, licence label, clean drain) and as the
engine behind the remote database image's smoke test. The full clean-host
install test starts the packaged node as the service account against the
packaged runtime and model, over TLS, on every distribution.

RPM names follow the PGDG-RPM convention (`postgresql18-postvec`) and depend on
`pgvector_18`; Debian names follow the Debian convention
(`postgresql-18-postvec`, `postgresql-18-pgvector`).

| Image | Mode | Extra content |
|---|---|---|
| `<repo>:0.1.0-1-pg18-remote` | `grpc` (pinned - carries no engine assets) | extension, pgvector, CLI |
| `<repo>:0.1.0-1-pg18-local` | `embedded` (`grpc` still works) | + ONNX Runtime + the bundled model |
| `<server repo>:0.1.0-1` | the inference node | `postvec-server`, CLI, ONNX Runtime, the bundled model - from the packages |

Moving tags `pg18-remote` / `pg18-local` also exist. There is deliberately **no
`latest`** on the database images: it hides the PostgreSQL major, and a
major-version image change cannot upgrade a data directory in place. The node's
image has no major and no data directory, so its moving tag *is* `latest`.

Each published image is a multi-architecture index. Build provenance is attested
against the index — the digest a tag resolves to — and the **SBOMs are attested
against the child manifests, one per architecture**. That split is not
bookkeeping: a scanner pointed at an index documents whichever child it defaults
to (`linux/amd64`), so a single SPDX document attested against the index would be
a claim about the arm64 image that nothing ever produced.
`postvec-release.json` records the index digest and, under `platforms`, each
child's digest and its SBOM.

### Release identity

A release is `<version>-<packaging revision>` — `0.1.0-1` — everywhere:

| | |
|---|---|
| git tag | `postvec-v0.1.0-1` |
| package version | `0.1.0-1+deb12`, `0.1.0-1.el9` |
| image tag | `0.1.0-1-pg18-remote`, `0.1.0-1-pg18-local`; `postvec-server:0.1.0-1` |

The packaging revision is part of the identity because `PACKAGE_RELEASE` exists
to allow a rebuild that changes no source — a dependency-metadata fix, say. If
the image tag carried only the version, that rebuild would overwrite the image
somebody is already running, and "immutable tag" would be a claim rather than a
property. The release job refuses to start if a *published* GitHub release for
this identity already exists.

**Recovering an interrupted publication.** Seven versioned image manifests are
created in parallel, so a failure part-way through leaves some already public.
Preflight therefore does not treat an existing image tag as a reason to stop —
it logs which tags exist, says plainly that the run is a resumption, and lets
the image jobs enforce immutability where they can actually compare: each one
pushes to a staging repository first, then reuses the release tag only if its
digest is byte-identical, and fails hard if it is not. That tag is immutable
and already published; a differing digest means the release is spent, and the
fix is to bump `PACKAGE_RELEASE`.

Preflight logs in to the registry before it inspects anything, because an
authorization failure and an absent tag are the same "not found" from the
outside — and an unauthenticated probe would read every existing tag as
missing. If a probe still fails for a reason the script does not recognise, it
stops rather than guess.

The GitHub release itself is **replaced, not updated**. `overwrite_files`
replaces an asset of the same name and promises nothing about an asset the new
release does not have, so an interrupted run could leave a package from an
abandoned attempt attached — and `sha256sum --check SHA256SUMS` would pass, because
it verifies the files it was told about and is silent about an extra one. An
existing draft is therefore deleted (the tag is kept) before the new one is
created, and verification compares the published asset *set* against the release
directory as well as the checksums.

If the run dies after the release is published but before the moving tags are
advanced, the release itself is complete and correct. Run the
`postvec-moving-tags` workflow (`dry_run` first) to finish the job: it is
idempotent, and it verifies all seven tags rather than assuming the writes took.
It takes the **whole release tag**, in either namespace, so the same repair
applies to a disposable rehearsal that failed at the same point — which is part
of criterion 2, not an afterthought. Repository names and versioned digests come
from that release's own manifest, whose attestation is verified with
`--signer-workflow` before any of it is turned into a registry write; so a
rehearsal's moving tags land in the throwaway namespace without the workflow
being told about it.

It is two jobs, split on read versus write. Resolving the plan and reporting it
— `dry_run: true`, the default, and the safe thing to run when nobody is sure
what state the tags are in — needs no environment, no approval and no write
permission. Only the job that actually moves a tag is protected.

**Moving-tag writes are serialised across both workflows.** Three operations
mutate the same seven names — publishing release A, publishing release B, and
this repair — and the release workflow's own concurrency key includes the ref, so two
releases can run at once. Whichever finished last would decide where a
`pgNN-local` or `pgNN-remote` moving tag points, which is how a published
release gets moved backwards by an unrelated run finishing late. The release
workflow's `publish` job and this workflow's
`advance` job therefore share one job-level concurrency group, keyed by **image
repository** so a disposable rehearsal never serialises against production.

**Both write by digest, and write all seven.** The source is
`<repository>@<digest>` taken from the verified manifest, never the versioned
tag: an approval can take as long as it takes, and a tag is a mutable pointer.
The recorded "current" digest is shown to the reader of a dry run and is *not* a
decision input — skipping a tag because it looked correct at planning time would
leave it wrong if it changed during the wait, with verification reporting the
problem and nothing having repaired it. Re-pointing a tag at the digest it
already has costs one registry write and is otherwise a no-op, which is a better
trade than a race. Both then verify, because "the command did not fail" is not
"the tag points where it should".

**And neither stops at the first registry failure.** `scripts/advance-moving-tags.sh`
is shared by both: it attempts every write, then inspects every tag, and decides
the result from what the registry actually holds — a write that fails against a
tag already pointing at the right digest is not a failure, and a write that
reports success and leaves the wrong digest is. Under `set -e` an inline loop
aborted on the first error, so the remaining tags were never attempted, the
verification never ran and the recovery guidance never printed; an operator was
left with a red step and no idea which tags had moved. `tests/unit-test.sh`
drives the helper against a fake registry and asserts exactly that: one failed
write, every write still attempted, every tag still inspected, a non-zero exit,
and the guidance on screen.

Its preflight is the other half of the contract: every entry must be
`<repository>@sha256:<64 lowercase hex>`, and the source's digest must *equal*
the expected one. A substring test for `@sha256:` is not that check — it accepts
`repo@sha256:not-a-digest` — and a plan whose two halves disagree would
otherwise be written first and diagnosed afterwards, leaving a moving tag at a
digest nobody chose. Seven cases assert the refusal **and** that nothing was
written.
It reads which seven images a release consists of from that release's own
attested `postvec-release.json`, not from the default branch — the branch's
`versions.env` may have moved on, and "what is 0.1.0-1" is a question the
release already answered.

### Release acceptance criteria

These are blocking, not aspirational. Nothing published so far in this repository
has passed them, and until it has, this pipeline is a preview of a release
process rather than a release process.

| # | Criterion | Status |
|---|---|---|
| 1 | One green `mode: rehearse` **full-matrix** run: 4 distributions × 3 majors × 2 architectures, both package families, both image variants, the node package and image | runnable, not yet run |
| 2 | One green `mode: disposable-publication` run into a throwaway registry namespace: draft assets, attestations, moving tags, and the resumption path | runnable, not yet run |
| 3 | `MAINTAINER` is a real, monitored address | done |
| 4 | Native **upgrade tests** before the second public release (see below) | not applicable to 0.1.0 |

Both rehearsals are now single dispatches — see "Rehearsing a release" below.
Until they have run green, **the distribution story is a preview, not a
production claim.** Package design, the tested payloads and the runtime
packaging are production-grade; the release automation is unproven until it has
executed end to end, and the *adoption* experience is not production-grade until
signed repositories exist (§Repositories in
[install packages](https://postvec.dev/docs/install/packages)) — GitHub release assets have no
`apt upgrade`, no security-update delivery, no package-manager-native signature
verification and no key rotation.

What local evidence exists today, so nobody mistakes it for more:

- **Full clean-host install**, amd64 / PostgreSQL 18: Debian 12 and AlmaLinux 9,
  46 assertions each. Both images built and smoke-tested locally.
- **Bootstrap execution**: all five distributions it will run on (see above).

That is a fraction of the release matrix. It does **not** demonstrate a full
install on Ubuntu 22.04 or 24.04, PostgreSQL 16 or 17, any arm64 installation or
inference, the full 56-package/40-debug closure, seven multi-architecture
indexes with fourteen child SBOMs, GitHub-hosted attestations, exact draft
assets, publication resumption, or moving tags. The node package and image
have Debian 12, Ubuntu 22.04 and AlmaLinux 9 evidence on amd64 / PostgreSQL
18 and no more: packages built and verified (Debian lintian-clean of errors),
the node-only install with and without the CLI including edited-config
removal, the full clean-host install (Debian, EL9) starting the packaged node
over TLS with the packaged configuration, the image composed from the
packages and passing `tests/server-image-test.sh`, and the remote database
image embedding through the node image. No Ubuntu 24.04 install test here,
no arm64, no booted systemd, no CI run yet.

The fixture suite (`tests/unit-test.sh`) gives real confidence that the closure
logic *refuses* the right things, which is what synthetic tests are good for. It
is not evidence that the matrix builds, and it is not a substitute for criteria
1 and 2.

**Upgrade testing (criterion 4).** `assert-versions.sh` skips the upgrade-graph
check because there is no preceding release tag, which is correct for 0.1.0 and
must not still be true when 0.2.0 is published — the first upgrade must not be
exercised for the first time by users. Before the second release, add native
tests covering: package upgrade with an existing database and extension;
`ALTER EXTENSION postvec UPDATE`; both remote and embedded configurations;
roll-forward after an interrupted package transaction; and removal of obsolete
files with user configuration preserved.

### Rehearsing a release

**Publication is dispatch-only.** There is no tag-push trigger. Publishing on
`git push --tags` reads as convenient and makes the one thing this workflow must
never do — publish something nobody meant to publish — the consequence of an
ordinary git command. It also made the disposable-publication rehearsal
*unsafe*: pushing a scratch tag started a production publication against the
pinned repositories, and the rehearsal dispatch then queued behind the run you
were busy cancelling. Creating the tag and publishing it are two acts. Nothing
is weaker for it — the tag must still exist, still point at the commit built,
and still agree with the crate versions.

One dispatch input decides everything: `mode`.

**Criterion 1 — the full-matrix rehearsal.** Builds, packages, install-tests and
smoke-tests the whole matrix and pushes nothing: no staging image, no
attestation, no release.

```
ref:  main            (or any branch or commit — no tag needed)
mode: rehearse
```

Only the publishing modes require a tag at all — `publish` a `postvec-v*` one,
`disposable-publication` a `postvec-rehearsal-v*` one — because a rehearsal
exists to run **before** the tag is cut. Rehearsal jobs request no environment,
so they do not wait on an approval that guards steps the run has already skipped
— approving rehearsals is how approving a real release becomes a reflex. A
repository override in this mode is *refused*, because a run that pushes nothing
has nowhere to redirect.

**Criterion 2 — the disposable publication.** This one has to publish something
somewhere, so it publishes to a namespace nobody is running:

```
ref:                      postvec-rehearsal-v0.1.0-9001
mode:                     disposable-publication
image_repository:         ghcr.io/univec-ai/postvec-rehearsal
image_staging_repository: ghcr.io/univec-ai/postvec-rehearsal-staging
```

**The tag namespace is what binds the mode to an identity.**
`disposable-publication` requires a `postvec-rehearsal-v*` tag and `publish`
refuses one, so neither can produce the other's release. Sharing `postvec-v*`
would have meant a real tag dispatched in rehearsal mode publishing a GitHub
release under the real identity — after which the real publication is refused,
because a release is published once.

Both overrides are **required** in this mode and **refused** in `publish`, so
neither half-filling the form nor picking the wrong mode can redirect a real
release or quietly send a rehearsal into production. They are validated by the
same grammar as the pinned values, must differ from both pinned repositories,
and are logged as warnings. `build-image.sh` takes the same `--repository`, so
the image built, tested and pushed is one reference throughout.

The scratch tag is a real tag on a scratch branch with `PACKAGE_RELEASE` bumped
to something obviously disposable, so every gate — tag points at HEAD, versions
agree, release not already published — runs exactly as it will for the real
thing. Afterwards, delete the GitHub release, the tag, the scratch branch and the
two GHCR namespaces.

**The real release.**

```
ref:  postvec-v0.1.0-1
mode: publish
```

**Dispatch from the tag, not from a branch.** `ref` controls what
`actions/checkout` fetches; it does *not* control which version of the workflow
GitHub executes — that comes from the branch or tag chosen when dispatching. A
web dispatch from `main` naming a release tag would run `main`'s workflow and
attest `main`'s workflow against a tag's artifacts. Both publishing modes
therefore require the two to agree, which the web UI cannot express as
conveniently as the CLI can:

```bash
gh workflow run postvec-release.yml \
  --ref postvec-v0.1.0-1 \
  -f ref=postvec-v0.1.0-1 \
  -f mode=publish
```

`--ref` selects the workflow file's version; `-f ref=` is what gets checked out.
They must be the same tag, and the run prints the corrected command if they are
not. The disposable rehearsal is the same shape with its own tag namespace and
the two required overrides:

```bash
gh workflow run postvec-release.yml \
  --ref postvec-rehearsal-v0.1.0-9001 \
  -f ref=postvec-rehearsal-v0.1.0-9001 \
  -f mode=disposable-publication \
  -f image_repository=ghcr.io/univec-ai/postvec-rehearsal \
  -f image_staging_repository=ghcr.io/univec-ai/postvec-rehearsal-staging
```

The whole decision — which mode may publish, which tag namespace it requires,
which overrides it demands or refuses, and the dispatch-ref rule — lives in
`scripts/release-mode.sh` rather than in the workflow, so the mode × tag ×
override cross-product is regression-tested in `tests/unit-test.sh` instead of
being exercised for the first time by a release.

**The gate runs from the dispatch ref, before the released ref is checked out.**
It decides whether publication is enabled, whether the protected environment is
requested, and which repositories become job outputs — so running it from
`inputs.ref` would let a modified branch supply the code authorising its own
publication, and the dispatch-ref binding would defeat itself. The `validate`
job therefore checks out the dispatch ref, runs the gate, and only then checks
out what is being built. For publishing modes those are the same commit,
because that is what the gate enforces; a rehearsal genuinely needs the second
checkout. A missing dispatch ref is fatal for a publishing mode rather than a
skipped check: a gate that passes when it cannot tell is not a gate.

`scripts/release.sh` is the same sequence on one machine, for the host
architecture only.

### Where the packages land

```
dist/common/<distro>-<arch>/                 postvec-cli, postvec-server, ONNX Runtime (+ debug)
dist/noarch/<distro>/                        the model bundle and the metapackage
dist/extension/<distro>-pg<major>-<arch>/    postgresql-<major>-postvec (+ debug)
release/                                     the flat directory a user downloads
```

Three roots because the packages have three identities:

| Root | Keyed by | Because |
|---|---|---|
| `common` | distribution, architecture | the CLI, the node and ONNX Runtime are native code, identical for every PostgreSQL major |
| `noarch` | distribution | the model bundle and the metapackage are `all`/`noarch` — one build per distribution, and nothing about them is architectural |
| `extension` | distribution, major, architecture | `postvec.so` is compiled against one major's headers |

Anything that consumes packages — install tests, image assembly, the release
manifest — reads all three. The two mistakes this layout exists to prevent both
pass on the reference cell and fail everywhere else:

- reading only the major's own cell finds no CLI on PG 16 and 17, and passes on
  PG 18 where a flattened layout coincides;
- writing the `all` packages into *an* architecture's cell makes them invisible
  to the other architecture — an arm64 install test that finds no model, and an
  arm64 local image that cannot be built.

`build-packages.sh` removes an `all` package it finds in a `common` cell, so a
working tree from before the split does not leave a second copy for the release
manifest to collect.

### Why the CLI is its own package

A host may legitimately run PostgreSQL 16 and 18 side by side and install both
extension packages. If each carried `/usr/bin/postvec`, `dpkg` and RPM would
refuse the second install with a file conflict. One CLI package owns the
binary; each extension package depends on a compatible version of it
(`>= 0.1.0`, `< 0.2.0` — the next version allowed to break compatibility).
`tests/package-install-test.sh` asserts this against a real dual-major install.

### Why there is only one extension build

Public packages are compiled with `--features pgNN,embedded`. `postvec.mode`
defaults to `'embedded'`, and the library loads ONNX Runtime only in that
mode; `postvec.mode = 'grpc'` selects the thin remote client instead, so one
artifact serves both deployment shapes.
Publishing a separate "thin" package would put two different libraries at the
same path, make upgrades ambiguous, and let someone select embedded mode
against a library that cannot do it.

The thin build stays useful for development; it is not the public contract.

CPU only, for this line. GPU multiplies the CUDA/cuDNN/TensorRT/driver matrix
and belongs in a separately versioned distribution project.

---

## What the packages may not do

Installing a package installs files. That is the whole contract.

No maintainer script may restart, reload or reconfigure a cluster, connect to a
database, run `CREATE`/`ALTER`/`DROP EXTENSION`, edit `postgresql.conf`, invoke
`postvec setup`, download anything, or delete user data. The automation an
operator actually wants is an explicit, consented command:

```bash
sudo postvec setup --database app --grpc 192.0.2.20:33333 --http https://192.0.2.20:22222
postvec doctor --database app
```

That is also why no package ships `/etc/postvec/providers.d`, the directory the
external-provider connector files live in. It has to be `0700` and owned by the
account that owns the cluster — which is not always `postgres`, and which a
package would have to chown at unpack time, before PostgreSQL's own packages
have created it. `postvec setup --embedded` creates it instead (so does
`postvec provider add`), and neither package removal nor `postvec uninstall`
deletes it: it holds credentials the packages never created. An absent
directory is exactly the zero-config state, so nothing is missing until an
operator configures a provider.

`scripts/verify-package.sh` enforces this mechanically: it extracts the
maintainer scripts, strips comments and here-document bodies (a `postrm` that
*prints* "run postvec uninstall first" is exactly right; one that *runs* it is
the defect), and fails on any forbidden command. `tests/package-install-test.sh`
then proves it on a live system: no PostgreSQL process started, no
configuration written, no CLI state created.

`postvec-server` is held to the same rule with one addition a daemon package
cannot avoid: its `preinst` creates the `postvec-server` system account. It
still enables nothing, starts nothing and calls `systemctl` nowhere — the
`postinst` prints `systemctl enable --now postvec-server` for the operator to
run, from inside a here-document, which is exactly the distinction the scanner
makes. The install test asserts no node process exists after installation, and
the `postrm` deletes neither the engine root, nor the configuration, nor the
account.

### The lintian policy

The same script runs **lintian** on every `.deb` and fails on any error that
`lintian-exceptions.txt` does not cover. The exception list is short on purpose,
and each entry carries the reason it exists — an exception without one is
indistinguishable from a workaround somebody added in a hurry. Today it holds
exactly one tag: `dir-or-file-in-opt`, because the engine assets live under
`/opt/postvec`, which is what the FHS reserves `/opt/<provider>` for
and what `postvec.path` defaults to. Debian's rule is written for
packages *in the Debian archive*; satisfying it would mean shipping model
weights and a vendored runtime into a directory the distribution owns, which is
the outcome the rule exists to prevent.

Everything else lintian objected to was fixed rather than excepted:

- **every package now ships a changelog** (`/usr/share/doc/<pkg>/changelog.Debian.gz`,
  `changelog.gz` on RPM), rendered from `changelog.Debian` by
  `render_changelog` in `scripts/lib.sh`. nFPM has a `changelog:` field and it
  is deliberately unused: its `.deb` output omits the `<distribution>;
  urgency=` header the format requires, so dpkg and lintian both reject the
  result as "not a Debian changelog" — trading a missing file for a malformed
  one. `verify-package.sh` asserts the file's presence independently of
  lintian, so a package without one fails on any host, not only a Debian one;
- **the bundled model's Debian copyright** is a DEP-5 file rendered from
  `model/copyright.in` by `scripts/check-model-bundle.py`, out of the verified
  archive's own licence and provenance. For `apache-2.0` it references
  `/usr/share/common-licenses/Apache-2.0` rather than repeating the text, which
  lintian reports as an error; any other approved licence id inlines the
  archive's `LICENSE`, indented for DEP-5. RPM's `/usr/share/licenses` has no
  common copy and receives the full text either way, and the verbatim licence
  always travels with the weights under `/opt`.

The verdict is computed from lintian's output rather than delegated to
`--fail-on error`, so an accepted tag is *reported* — "accepted by
lintian-exceptions.txt: dir-or-file-in-opt (12)" — instead of vanishing behind
`--suppress-tags`. An exception the reader cannot see is indistinguishable from
a check that was never run.

---

## Layout

```
versions.env                   the one reviewed file: every pin lives here
changelog.Debian               the changelog every package ships, one entry
                               per release identity
lintian-exceptions.txt         lintian tags postvec accepts, with the reason
                               for each; anything else is a defect to fix
requirements-schema.txt        hash-locked validator for postvec-release.json
release-manifest.schema.json   the contract for postvec-release.json
model/                         the DEP-5 copyright template, and golden/
                               (one file per model name: the embeddings that
                               model was published with). The weights,
                               descriptor and licence come from the registry
nfpm/                          one package description per package
builders/                      hermetic compile environments (deb, rpm families)
docker/                        image, entrypoint, healthcheck, init, compose
                               examples, and the postvec-server image
server/                        what the postvec-server package adds to the
                               crate's own files: the systemd drop-in that
                               points the unit at /opt/postvec, and the
                               minimal conffile
scripts/                       the pipeline
tests/                         entrypoint unit tests, live package and image tests
```

`build/` and `dist/` (per-cell build output) and `release/` (the flat directory
a user downloads) are generated and git-ignored.

### The pipeline

| Script | Does |
|---|---|
| `assert-versions.sh` | refuses to start unless crate, control file, pins and tag agree |
| `build-onnxruntime-bundle.sh` | downloads, verifies, prunes ONNX Runtime into a payload |
| `build-model-bundle.sh` | pulls the bundled model from the registry's public channel with `postvec model pull`, verifies it against the pinned archive digest, and lays out the payload and its provenance |
| `check-model-bundle.py` | the network-free trust gate: pins, channel, licence policy, closure and descriptor facts, then emits `model-facts.env`, `SOURCE.json` and the rendered DEP-5 copyright |
| `build-extension-stage.sh` | compiles one cell in a builder image; splits debug info; runs the ELF gate. `--with-server` also compiles the node, for the shared-package cell |
| `inspect-elf.sh` | proves the real dynamic dependencies of the produced binaries |
| `elf-depends.sh` | runs the target distribution's own dependency generator (`dpkg-shlibdeps` / `rpmdeps`) and prints the package relations |
| `build-packages.sh` | renders the nfpm descriptions and emits `.deb`/`.rpm` |
| `verify-package.sh` | asserts package shape, ownership, dependencies, maintainer-script policy, and lintian against `lintian-exceptions.txt` |
| `build-image.sh` | assembles an image from already-built packages |
| `write-release-manifest.sh` | computes `postvec-release.json` and `SHA256SUMS` from the artifacts |
| `lint-nfpm-configs.sh` | renders every package description with stubs and parses the result |
| `audit.sh` | blocking `cargo audit` over both lockfiles, with dated exceptions |
| `build-server-image.sh` | assembles the postvec-server image from the Debian 12 packages — published, and the engine the remote-image test runs against |
| `verify-release-metadata.sh` | reads each package's own metadata and requires the release directory to agree with it |
| `release-mode.sh` | decides what a release run may do — mode, tag namespace, overrides, dispatch ref — and refuses the rest |
| `advance-moving-tags.sh` | points the moving tags at recorded digests, attempts every write, verifies every tag, and reports what actually landed |
| `postvec-prerequisites.sh` | published to users: configures PGDG (+ EPEL/CRB) with consent, and nothing else |
| `refresh-pins.sh` | re-resolves the pins and prints them; never edits (`ort model rustup protoc syft images buildx python`) |
| `strip-cache-mounts.py` | rewrites a builder Dockerfile for the classic builder, when buildx is absent |
| `release.sh` | all of the above, in order |

Three helpers wrap the GitHub side. None of them is a gate — every rule they
observe is enforced again inside the run, which is where it counts:

| Script | Does |
|---|---|
| `release-preflight.sh` | read-only: version consistency, working tree, `gh` version and auth, the `postvec-release` environment, tag state, whether the release or its six image tags already exist. Prints the dispatch command |
| `dispatch-release.sh` | starts `postvec-release` with `--ref` and `-f ref=` both derived from `versions.env`, so the two cannot disagree; requires typed confirmation for `publish` |
| `verify-published-release.sh` | checks a published release from outside: downloads assets, `SHA256SUMS`, `gh attestation verify --signer-workflow`, and every image digest and architecture |

And the workflows that drive them:

| Workflow | Runs on | Does |
|---|---|---|
| `postvec-release.yml` | **dispatch only** | the whole release: build, verify, install-test, image, manifest, publish. There is deliberately no tag-push trigger — see "Rehearsing a release" |
| `postvec-packaging-ci.yml` | pull request, nightly | one reference cell per PR; the full matrix nightly |
| `postvec-moving-tags.yml` | dispatch only | points the seven moving image tags at a published release — production or disposable rehearsal — and verifies them |
| `postvec-ci.yml`, `postvec-cli-ci.yml`, `postvec-server-ci.yml` | pull request | extension, CLI and node test suites (the release's `validate` job runs the CLI's and the node's again on the released commit) |

Two tools rather than four: **nfpm** emits both `.deb` and `.rpm` from one
package description, so a dependency rule or a file placement is stated once
instead of being kept in sync between a `debian/control` and an RPM spec. Where
the ecosystems genuinely differ — package naming, version-relation syntax,
licence directory — the difference is explicit in an `overrides:` block. If
nfpm ever cannot express something the contract needs, the answer is to switch
that package to `dpkg-buildpackage`/`rpmbuild`, not to weaken the contract.

nFPM has one gap that matters and is worked around explicitly: **it runs no
dependency generator.** It writes exactly the `depends` it is given, so a
package described only by its file list declares no libc dependency at all,
installs cleanly on a minimal host, and fails to load at first use.
`scripts/elf-depends.sh` therefore runs `dpkg-shlibdeps` or `rpmdeps` *inside a
container of the target distribution* — the answer differs per release — and
splices the result into the description. `verify-package.sh` fails any package
that contains a binary and declares no libc dependency, so the step cannot
silently stop happening. (This is how `postvec-onnxruntime` comes to require
`libstdc++6`, which nothing else in the release does.)

nfpm's own environment expansion also does not reach every field (notably
`contents[].src`), so descriptions are rendered by `render_nfpm_config` in
`scripts/lib.sh` from an explicit allowlist. An unset variable is a hard error
rather than an empty string that silently packages the wrong path. Rendered
descriptions are kept in `dist/<cell>/.rendered/` — when a package turns out to
contain the wrong thing, that is the first file to read.

---

## Pins

`versions.env` is the only place a version is chosen for the inputs this
project controls. Pinned there, by digest or exact version:

| Input | Pinned by |
|---|---|
| base images (PostgreSQL, builders) | multi-arch index digest |
| ONNX Runtime | SHA-256 of the release archive |
| `rustup-init` | SHA-256, per architecture |
| `protoc` | version + SHA-256, per architecture |
| Rust and `cargo-pgrx` | exact version, `--locked` builds |
| nFPM | image digest |
| Buildx | exact version |
| BuildKit | image digest |
| Syft | version + SHA-256 |
| the schema validator | `--require-hashes`, whole transitive set |
| the bundled model | registry archive digest + revision (`postvec model pull`) |

Two of those are easy to believe are pinned when they are not. **Buildx and
BuildKit** build every package and every image, and pinning
`docker/setup-buildx-action` to a commit pins the *action*: on a runner without
buildx it installs whatever release it considers current, and the container
driver pulls `moby/buildkit:<tag>`. Both are therefore named in `versions.env`
and applied through `.github/actions/postvec-buildx`, which is the only place
a workflow sets a builder up. And the **schema validator** that decides whether
`postvec-release.json` may be published had one pinned version and four
unpinned transitive dependencies; `requirements-schema.txt` hash-locks the whole
set, and `refresh-pins.sh python` regenerates it from the versions it already
names.

**What is not pinned, and why it matters.** Being honest about this is more
useful than a claim of hermeticity that does not hold:

- **PGDG packages** (`postgresql-NN`, `postgresql-server-dev-NN`,
  `postgresqlNN-devel`) are resolved from the live repository at build time.
  PGDG publishes no snapshots, so pinning them means mirroring them. What *is*
  pinned is the trust root — the Debian signing key's fingerprint and the EL9
  repository RPM's digest are both verified before use — and the release
  manifest records the PostgreSQL version each build cell actually resolved.
  Byte-for-byte rebuild of an old release is still not guaranteed.
- **The distribution's own base packages** in the builder images, for the same
  reason.
- **pgvector** is a runtime dependency resolved by the user's package manager,
  by design — the extension packages express a minimum version, not a pin.
Every action used by the release and packaging workflows is pinned to a commit
SHA with the version in a trailing comment, so a moved tag upstream cannot
change what runs.

Two consequences worth stating plainly: rebuilding an old release may not
produce identical bytes, and the supply chain includes PGDG's archive. The
release manifest and the attestations record what *was* built, which is what a
consumer actually verifies against.

`assert-versions.sh` fails closed on an unfilled placeholder, a malformed
digest, a crate version that disagrees with the pins, or (from the second
release onwards) a missing `postvec--<old>--<new>.sql` upgrade script.

`refresh-pins.sh` re-resolves everything and prints what it found. It never
edits `versions.env`: changing a pin changes what every user downloads, and
that is a review event, not an automated one.

---

## The bundled model

`sentence-transformers-all-minilm-l6-v2` — 384 dimensions, 256-token context,
Apache 2.0, small enough that the all-in-one image is a reasonable download, and
good enough that the first `postvec.search()` a user runs actually works.

**It comes from the postvec model registry's public channel**, acquired by
`scripts/build-model-bundle.sh` with the same `postvec model pull --path` a
user runs — not by a private downloader of packaging's own. That is the whole
point: one acquisition path, one index-schema implementation, one extraction
policy, one receipt format. The CLI validates the index and the dependency
graph, downloads resumably, verifies the **whole archive digest from byte
zero**, strict-extracts, and writes an install receipt; packaging then checks
that receipt against the release's pins and copies the archive-owned files.

`versions.env` holds five reviewed values and nothing else about the model:

```sh
BUNDLED_MODEL_NAME=sentence-transformers-all-minilm-l6-v2
BUNDLED_MODEL_REGISTRY_REVISION=2
BUNDLED_MODEL_ARCHIVE_SHA256=531cce26…7dbf
BUNDLED_MODEL_PKG_SUFFIX=minilm-l6-v2
BUNDLED_MODEL_BUNDLE_VERSION=1
```

The **digest is the pin**: the build refuses any other bytes and prints both
values plus `refresh-pins.sh model`. The revision is the human-readable
cross-check and becomes the model package's major version
(`postvec-model-minilm-l6-v2` `2.1.0`). Backend, licence, dimension, sequence
length and upstream provenance are *derived* from the verified archive —
restating them in `versions.env` would create a second source of truth that can
disagree with the bytes. They land in `build/payload-common/model-facts.env`,
which every later stage reads.

This is a fail-closed pin, not a historical-revision selector. Registry v1's
index exposes the current head and `postvec model pull` deliberately has no
revision flag, so once the head advances a clean pull cannot reconstruct an old
pin — the digest-keyed cache under `build/model-cache/sha256-<digest>/` and
`--from <engine root>` cover a local rebuild, and the release manifest records
what shipped. That is the same honest limitation as the live-PGDG rebuild one
above, stated rather than papered over.

Because that cache is the offline-rebuild path, promoting into it is written to
survive being killed. A verified pull is staged as `….incoming` on the cache's
own filesystem and then renamed into place, so the interruptible step — a
cross-filesystem copy — can never be the thing that overwrites a good cache;
and if a run dies between the two renames, the next one restores `….replaced`
rather than reaching for the network. A recovered cache is not trusted on the
strength of having been recovered: it goes through `model show --verify` and
`check-model-bundle.py` like any live pull. A `….incoming` left behind is
removed instead, because an interrupted copy is indistinguishable from a
complete one and a fresh pull is the right answer there.

**Public channel only.** The build asserts `access == "public"` in the receipt
and runs the pull with no selected credential: `POSTVEC_API_KEY` unset and a
scratch `XDG_CONFIG_HOME`. Root is different — the CLI intentionally uses
`/var/lib/postvec/auth.json` regardless of XDG — so the step *refuses* to run as
root while that store exists rather than reading around an operator's
credentials. A receipt carrying local licence-acknowledgement evidence is
refused too: one developer's acknowledgement on one host is not authority to
redistribute a model in a `.deb`.

**Only what the descriptor references is packaged**, and that is now the
publisher's `exclude` decision recorded in the registry rather than a list here.
Packaging never prunes an archive — doing so would invalidate the pull's own
per-file receipt — so an archive carrying unreferenced ONNX graphs is *refused*,
naming `registry-publish add … --exclude 'onnx/model_*.onnx'` as the fix.
`verify-package.sh` fails independently if a quantised or optimised variant
reaches a package.

**The receipt is build evidence, not package content.** `.postvec-install.json`
records `installed_at`, so shipping it would make an otherwise reproducible
package differ on every clean rebuild. The build verifies with it
(`postvec model show --verify`, the shared offline `Receipt::verify_files`),
copies only the files it lists, and leaves it behind. Stable provenance —
`SOURCE.json`, `model-files.sha256`, `LICENSE` and a rendered DEP-5 `copyright`
— goes to `/usr/share/doc/<package>/`, and nothing packaging generates is placed
inside the engine model directory.

That is checked from both ends. `tests/unit-test.sh` rebuilds the payload from a
receipt whose `installed_at` and `cli_version` differ and requires the bytes to
be identical — offline, in seconds, and it fails the moment the receipt is
copied or anything derived from its timestamps reaches `SOURCE.json` or
`model-facts.env`. The release acceptance run
additionally does **two forced clean pulls** of the same pinned head and
compares the payload hashes, which needs the registry and is therefore a
release step rather than a unit test.

The published descriptor lists `execution_providers: ["cuda","cpu"]`. A build
without the `ort-cuda` feature skips the provider and falls back to CPU, so this
is a **warning, not a fault** — expect one `CUDA execution provider was
requested, but the application was not compiled with the 'ort-cuda' feature.
Skipping.` line per model load in the local image's PostgreSQL log. Packaging
does not edit the archive to silence it.

**The model name is the only compatibility identifier a database has.** Every
registry row and every `postvec.models` entry stores the *name*; nothing
anywhere records the bundle version. A stored vector is only comparable to
vectors produced by the same weights, tokenizer, pooling, normalisation and
sequence length.

So the rule is narrower than "bump something": **any change to what the model
computes requires a new internal name.** `BUNDLED_MODEL_BUNDLE_VERSION` is
packaging metadata — use it for a repackaging that provably does not change the
numbers, and nothing else. Publishing changed behaviour under the same name
would make yesterday's vectors quietly stop meaning what today's mean, with no
error anywhere to say so.

`tests/model-golden-test.sh` is what makes that rule enforceable. It compares
the actual embeddings of a fixed set of inputs against
`model/golden/<model name>.json`, element by element, within a tolerance that
absorbs floating-point differences between CPUs but not behavioural ones.
Dimension, unit norm and "does the obvious sentence rank first" all survive a
tokenizer swap, a pooling change, a different opset, or a quantised graph
slipping into the bundle; comparing the numbers does not. The file is selected
**by name**, so a bundled-model change with no recorded goldens fails loudly
instead of comparing against the previous model's vectors. It runs on both
architectures in the release, and regenerating the goldens is a deliberate act
with a warning attached.

### Changing which model is bundled

A different *name* is a different vector space, so this is a deliberate,
reviewed sequence — not a version bump:

1. **Publish it on the public channel** (a UniVec-operated step), with
   `--exclude` globs for anything the descriptor does not reference.
2. **Edit five values** in `versions.env`: name, revision, digest, package
   suffix, and reset `BUNDLED_MODEL_BUNDLE_VERSION=1`. Everything else is
   derived. `scripts/refresh-pins.sh model` prints the revision and digest for
   the current head.
3. `scripts/build-model-bundle.sh` and **read what landed** — the script prints
   the package identity and the exact file list.
4. Build a reviewed local image and record the new model's goldens **once**:
   `tests/model-golden-test.sh --record <image>`. Review the diff, then run the
   ordinary (non-recording) golden test on **every** architecture in the
   release.
5. `scripts/release.sh --skip-images`, then a full local release.
6. Update the model name where documentation quotes it (`web/docs/`).

Renaming the package (`BUNDLED_MODEL_PKG_SUFFIX`) after a public release needs a
transitional package; before the first public release it does not.

**Adopting a new revision of the same model** — `registry-publish --update`
moved the head — is `scripts/refresh-pins.sh model`, paste the two lines,
rebuild, and **re-run the golden test without `--record`**. A revision that
changes the numbers under an unchanged name is exactly what goldens exist to
catch, and the correct response is a new name, not new goldens.

**Bundling more than one default model** is deliberately not designed. The
build *refuses* a non-trivial dependency closure rather than silently putting
several model identities into one binary package. When there is a real
second-model requirement, replace the single five-value record with a small
structured manifest and emit one package per record — not parallel shell lists,
where a name, revision, digest and package suffix can drift out of alignment.

---

## Upgrades

Package upgrades are two-phase, by nature:

1. install the new package — this replaces files only;
2. restart PostgreSQL so the postmaster maps the new `postvec.so`;
3. in every served database: `ALTER EXTENSION postvec UPDATE;`
4. `postvec doctor`.

Between (2) and (3) the running library is newer than the SQL in the database.
The worker refuses to work in that window: its version gate compares
`pg_extension.extversion` with the library version and parks — no claims, no
write-backs, no migration steps, and no heartbeat either, since the heartbeat
table is one of the tables whose shape is in question. The condition appears in
the server log and in `postvec doctor`'s `extension.version` check.

Container upgrades are the same, with one addition: initialisation scripts do
not run on an existing volume, so replacing an image never runs `ALTER
EXTENSION` for you.

```bash
docker compose pull db && docker compose up -d db
docker compose exec db psql -U "$POSTGRES_USER" -d "$POSTGRES_DB" -c 'ALTER EXTENSION postvec UPDATE'
docker compose exec db postvec-healthcheck
```

Rollback: roll the image or package back only when the release notes state the
SQL schema is backward-compatible; otherwise restore the backup taken before
`ALTER EXTENSION`. PostgreSQL has no general extension downgrade, and this
project will not pretend otherwise.

**A PostgreSQL major upgrade is not an image tag change.** Moving from
`pg17` to `pg18` against the same volume does not upgrade anything; use
`pg_upgrade` or dump/restore.

---

## Tests

| Test | Needs | Covers |
|---|---|---|
| `tests/unit-test.sh` | `python3` | the release's *refusals*: package and debug closure, artifact identity, architecture canonicalisation, build-cell set and recorded facts, image and per-platform-SBOM closure, base-image pins, release-tag identity per mode, the `versions.env` parser and publication grammar, the bootstrap's generated commands for every declared distribution, the bundled-model trust gate (pins, channel, licence policy, closure, descriptor facts) and the payload the bundle step produces — 132 assertions, no Docker, no network, seconds |
| `tests/bootstrap-test.sh` | Docker | the published bootstrap, run twice in a clean container of each of the five distributions it will execute on: fingerprint verified, repository RPM signature checked (EL), PostgreSQL and pgvector resolve afterwards, second run idempotent |
| `tests/healthcheck-test.sh` | nothing | the container health verdict, against stubbed `pg_isready`/`psql`: every property false in turn, an empty or truncated report, a NULL heartbeat, a psql that cannot connect, an empty or unknown `POSTVEC_MODE`, and that the SQL it runs writes nothing — 23 assertions, well under a second |
| `tests/entrypoint-test.sh` | a `postvec` binary | delegation, preload merge and validation, `_FILE` secrets, loopback contract, and that a relative binary path works (CI passes one) — 36 assertions, under a second |
| `tests/package-install-test.sh` | Docker | both families: the **published prerequisite bootstrap** (and its idempotency), dependency resolution, layout, dual-major coexistence, "the install changed nothing", a **real cluster** with `CREATE EXTENSION`, `setup`/`uninstall`, detached symbols, inference with the bundled model, removal — and `doctor`'s actual verdicts: which checks pass with no engine reachable, that it exits 1 and names the endpoint, and that it reports **healthy (exit 0)** once embedded configuration completes. The full run also installs `postvec-server` and starts the packaged node as its service account against the packaged runtime and model, over TLS |
| `tests/image-smoke-test.sh` | Docker, a built image | health, PID 1, published ports, embed → sync → search, remote-mode degradation, persistence, clean SIGTERM, startup failure modes |
| `tests/node-install-test.sh` | Docker | the inference host: node + runtime + model (+ CLI, or `--without-cli`) on a clean OS with no PostgreSQL, from local files and the distribution archive; nothing PostgreSQL arrives; the packaged configuration serves the packaged model over TLS as the service account; removal keeps configuration, engine root and account |
| `tests/server-image-test.sh` | Docker, a built postvec-server image | composed of exactly this release's packages, licence and version labels, unprivileged, admin port unexposed, `/ready` only once the model answers, `/config` advertises it, `postvec-server status` and the bundled CLI work inside, clean drain on SIGTERM |
| `tests/model-golden-test.sh` | Docker, a local image | the bundled model still produces the embeddings it was published with |

The remote image test needs a real engine to be meaningful:
`scripts/build-server-image.sh` assembles the **postvec-server** image from
this release's own packages — the node, the CLI, the same ONNX Runtime and the
same model bundle the local image carries — and `--server-image` makes the
test bring it up on a private network and drive embed → sync → search through
it. Without one the test *fails* rather than skipping: "the remote image works"
is not something to assert by omission.

Until 2026-08 this was a stand-in — `fixtures/inference-server`, an imitation
node written because no real server existed. Then it was a real node compiled
from source inside the image build, local only: nothing published it and no
manifest referenced it. Now it is the published artifact itself, so the engine
the remote image is tested against is the engine a fleet pulls.

`package-install-test.sh --minimal` installs only the CLI and the extension.
That is the dependency boundary a remote-mode user actually has, and running
only the full install would let a missing dependency in the extension package
hide behind something the metapackage pulled in.

Run them against exactly what will be published — pass an image *digest* in a
release job, not a tag.

---

## Where the rest lives

- Operator documentation: [postvec.dev/docs/install](https://postvec.dev/docs/install/)
- GitHub Releases / GHCR publication: this README, then [GitHub Releases](https://github.com/univec-ai/postvec/releases)
- Local source builds: [from source](https://postvec.dev/docs/install/source)
- The CLI's contract: [`postvec-cli/README.md`](../../postvec-cli/README.md)
- What postvec is and how it works: [postvec.dev/docs](https://postvec.dev/docs/)

#!/usr/bin/env bash
# Build postvec-release.json and SHA256SUMS from artifacts on disk.
#
#   write-release-manifest.sh [--dist DIR] [--build-info DIR]
#                             [--images images.json] [--git-tag TAG] [--out DIR]
#
# Everything is a pin or a hash of an artifact. Collect into one flat
# directory so `sha256sum --check SHA256SUMS` works on the names a user has.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

DIST="${PKG_DIR}/dist"
BUILD_INFO="${PKG_DIR}/build"
IMAGES_JSON=""
OUT=""
GIT_TAG_ARG=""
REQUIRE_IMAGES=""
EXPECT_DISTROS=""
EXPECT_MAJORS=""
EXPECT_ARCHES=""
EXPECT_IMAGES=""
EXPECT_SERVER_IMAGE=""
REQUIRE_SCHEMA=0
ALLOW_DIRTY=0
while (($#)); do
    case "$1" in
    --dist)           DIST="$2"; shift 2 ;;
    # Where the per-cell build-info.txt files live. Named rather than assumed,
    # so the build records are as much an explicit input as the artifacts —
    # and so the closure checks can be exercised against a fixture.
    --build-info)     BUILD_INFO="$2"; shift 2 ;;
    --images)         IMAGES_JSON="$2"; shift 2 ;;
    # The release tag this manifest describes. Stated rather than inferred from
    # the environment; see GIT_TAG below.
    --git-tag)        GIT_TAG_ARG="$2"; shift 2 ;;
    --out)            OUT="$2"; shift 2 ;;
    --require-images) REQUIRE_IMAGES=1; shift ;;
    --expect-distros) EXPECT_DISTROS="$2"; shift 2 ;;
    --expect-majors)  EXPECT_MAJORS="$2"; shift 2 ;;
    --expect-arches)  EXPECT_ARCHES="$2"; shift 2 ;;
    --expect-images)  EXPECT_IMAGES="$2"; shift 2 ;;
    # The repository the inference node's image must be recorded under.
    # Defaults to the pinned SERVER_IMAGE_REPOSITORY; a disposable publication
    # passes the derived `<override>-server`.
    --expect-server-image) EXPECT_SERVER_IMAGE="$2"; shift 2 ;;
    --require-schema) REQUIRE_SCHEMA=1; shift ;;
    # Local rehearsal only: lets the publishable-build checks run against a
    # working tree. The release workflow never passes it, and a manifest built
    # this way names a commit that is not what was built.
    --allow-dirty)    ALLOW_DIRTY=1; shift ;;
    -h|--help) sed -n '2,23p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done

load_versions
# The bundled model's package name, version and registry identity come from the
# facts the bundle step derived from the verified archive, not from versions.env
# — so the manifest's `bundled_model` block and its package closure describe the
# model that was actually built. They are read from the same build root the
# build-cell records come from, which is what `--build-info` names.
load_model_facts "${BUILD_INFO}/payload-common"
need python3 sha256sum
OUT="${OUT:-${PKG_DIR}/release}"

# The output directory is deleted and replaced, so it may only be somewhere
# this script is entitled to delete. `--out /home/me` should not be a way to
# lose a home directory to a typo.
OUT="$(realpath -m "${OUT}")"
case "${OUT}" in
"${PKG_DIR}"/*) : ;;
*)  die "--out must be inside ${PKG_DIR} (got ${OUT})
This directory is removed and rebuilt; it is not a general destination." ;;
esac
[[ "${OUT}" != "${PKG_DIR}" ]] || die "--out may not be ${PKG_DIR} itself"

[[ -d "${DIST}" ]] || die "no artifact directory: ${DIST}"

if [[ -n "${REQUIRE_IMAGES}" && -z "${IMAGES_JSON}" ]]; then
    die "--require-images was given but no --images file: a release manifest that
records no images cannot support the documented image verification steps"
fi

# A publishable release describes a commit, so it has to *be* one. A dirty tree
# means the manifest's git_commit names something that is not what was built.
if [[ -n "${REQUIRE_IMAGES}" ]] && (( ! ALLOW_DIRTY )); then
    if [[ -n "$(git -C "${REPO_ROOT}" status --porcelain 2>/dev/null)" ]]; then
        git -C "${REPO_ROOT}" status --short >&2
        die "the source tree has uncommitted changes; a publishable release must
describe exactly one commit"
    fi
fi

# ------------------------------------------------------- flat release directory
#
# Built into a fresh directory and moved into place at the end. Writing into
# an existing one would let a leftover artifact from an earlier run (a
# package from a version no longer being released) be collected, checksummed
# and published as part of this release, and it would look entirely valid.

# On the destination's own filesystem, so the final move is a rename. A
# half-written release directory is not a state worth having.
mkdir -p "$(dirname "${OUT}")"
STAGING="$(mktemp -d "$(dirname "${OUT}")/.postvec-release.XXXXXX")"
trap 'rm -rf "${STAGING}"' EXIT

# Every artifact, once, in one directory — the layout a user downloads into and
# the layout `sha256sum --check` expects. Duplicate basenames with different
# content are a build bug and stop the release here.
log "collecting artifacts into ${OUT}"
declare -A SEEN=()
while IFS= read -r artifact; do
    name="$(basename "${artifact}")"
    digest="$(sha256_of "${artifact}")"
    if [[ -n "${SEEN[${name}]+set}" ]]; then
        if [[ "${SEEN[${name}]}" != "${digest}" ]]; then
            die "two different builds of ${name} in this release:
  ${SEEN[${name}]}
  ${digest}
Architecture- and PG-independent packages must be built once, not rebuilt in
every matrix cell — see packaging/postvec/README.md §Release matrix."
        fi
        continue
    fi
    SEEN["${name}"]="${digest}"
    cp --preserve=timestamps "${artifact}" "${STAGING}/${name}"
done < <(find "${DIST}" -type f \( -name '*.deb' -o -name '*.rpm' \) | LC_ALL=C sort)

# Package SBOMs travel with their package, so they are collected in the same
# pass and recorded against it below.
while IFS= read -r sbom; do
    [[ -f "${sbom}" ]] || continue
    cp --preserve=timestamps "${sbom}" "${STAGING}/$(basename "${sbom}")"
done < <(find "${DIST}" -type f -name '*.spdx.json' | LC_ALL=C sort)

(( ${#SEEN[@]} )) || die "no .deb or .rpm artifacts under ${DIST}"
log "  ${#SEEN[@]} artifact(s)"

# The prerequisite bootstrap is a release *asset*, not a build artifact: it is
# the first thing a user runs, before any package exists on their host, so it is
# published with the release rather than linked out of a branch that can move
# under them. Collected here, before the manifest is computed, so it is recorded
# in postvec-release.json and in SHA256SUMS — and, in the release workflow,
# attested in its own right. A user is told to run it; "it hashes to what an
# attested checksum file says" is a chain, and the thing they execute deserves a
# signature of its own.
install -m 0755 "${PKG_DIR}/scripts/postvec-prerequisites.sh" \
                "${STAGING}/postvec-prerequisites.sh"

# From here on the manifest is computed over the flat directory, so the paths
# it records and the paths a user has are the same paths.
DIST="${STAGING}"

GIT_COMMIT="$(git -C "${REPO_ROOT}" rev-parse HEAD 2>/dev/null || echo)"
# Explicit first, ambient last.
#
# `GITHUB_REF_NAME` under `workflow_dispatch` is the branch or tag the *workflow
# file* was selected from, which need not be the thing being released: a web
# dispatch from `main` naming a release tag as an input would otherwise publish
# a manifest whose `git_tag` said `main`. So the caller states the tag, and the
# ambient value is only a fallback.
GIT_TAG="${GIT_TAG_ARG:-${POSTVEC_RELEASE_TAG:-${GITHUB_REF_NAME:-}}}"
MODEL_SOURCE_JSON="${BUILD_INFO}/payload-common${MODEL_DOC_DIR}/SOURCE.json"

# What the builders actually resolved, as opposed to what was asked for. PGDG
# publishes no snapshots, so the exact PostgreSQL and toolchain versions a cell
# used are only knowable from the cell itself — and are exactly what someone
# reproducing or auditing a release needs.
BUILD_CELLS_JSON="$(mktemp)"
python3 - "${BUILD_INFO}" > "${BUILD_CELLS_JSON}" <<'PY'
import hashlib, json, pathlib, re, sys

cells = []
for info in sorted(pathlib.Path(sys.argv[1]).glob("*/build-info.txt")):
    cell = info.parent.name
    # Two shapes. An extension cell names its PostgreSQL major, because that is
    # what varies across the 24 of them. A shared-package builder does not: it
    # builds the CLI and the engine assets, which are PostgreSQL-independent,
    # so it is identified by distribution and architecture alone. Its own
    # PostgreSQL major comes from build-info.txt, like every other fact about
    # the builder that produced it.
    match = re.match(r"^(?P<distro>[a-z0-9]+)-pg(?P<pg>\d+)-(?P<arch>amd64|arm64)$", cell)
    if not match:
        match = re.match(r"^common-(?P<distro>[a-z0-9]+)-(?P<arch>amd64|arm64)$", cell)
    if not match:
        continue
    facts = {}
    inventory = []
    section = None
    for line in info.read_text().splitlines():
        if line.startswith("[") and line.endswith("]"):
            section = line[1:-1]
            continue
        if section == "packages":
            if line.strip():
                inventory.append(line.strip())
        elif "=" in line:
            key, value = line.split("=", 1)
            facts[key] = value
    # The typed fields are set *after* the spread: build-info.txt also carries
    # a `pg_major=18`, and letting a string from a text file overwrite the
    # integer here is how the manifest stops matching its own schema.
    try:
        pg_major = int(match["pg"])
    except IndexError:
        # A shared builder: it still compiled against *some* major, and that
        # major is a fact about the build, so it has to come from the builder
        # rather than be invented here.
        pg_major = facts.get("pg_major")
        if pg_major is None or not pg_major.isdigit():
            sys.exit("%s: a shared builder must record its pg_major" % info)
        pg_major = int(pg_major)
    cell_record = {**facts,
        "cell": cell,
        "distro": match["distro"],
        "pg_major": pg_major,
        "arch": match["arch"],
    }
    if inventory:
        # The full installed-package list of the builder. It is what makes a
        # cell auditable at all: the repositories it resolved from are mutable,
        # so the only durable record of what went in is what came out.
        cell_record["packages"] = inventory
        cell_record["packages_sha256"] = hashlib.sha256(
            "\n".join(inventory).encode()).hexdigest()
    cells.append(cell_record)
print(json.dumps(cells))
PY

export DIST OUT GIT_COMMIT GIT_TAG MODEL_SOURCE_JSON IMAGES_JSON BUILD_CELLS_JSON \
       EXPECT_DISTROS EXPECT_MAJORS EXPECT_ARCHES EXPECT_IMAGES EXPECT_SERVER_IMAGE
# A publishable build is stricter: debug packages and SBOMs become mandatory,
# and the build-cell set is checked exactly. The python side reads this.
export REQUIRE_IMAGES

python3 - <<'PY'
import hashlib, json, os, pathlib, re, sys, datetime

dist = pathlib.Path(os.environ["DIST"])
out = pathlib.Path(os.environ["OUT"])

def env(name, default=None):
    value = os.environ.get(name, default)
    if value is None:
        sys.exit("write-release-manifest: %s is not set" % name)
    return value

def package_name(filename):
    """The package name, from the file name each format actually uses.

    deb: <name>_<version>_<arch>.deb    rpm: <name>-<version>-<release>.<arch>.rpm
    Splitting on the first digit would turn postgresql-18-postvec into
    "postgresql", which is a different package entirely.
    """
    if filename.endswith(".deb"):
        return filename.split("_", 1)[0]
    stem = filename[:-len(".rpm")].rsplit(".", 1)[0]   # drop .<arch>
    return stem.rsplit("-", 2)[0]                      # drop -<version>-<release>


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()

# Artifacts are discovered, not listed: a package that was built but forgotten
# in a hand-written list is a package with no published checksum.
# Everything is described by its own file name, which is the only thing a user
# holding a download has. deb: <name>_<version>_<arch>.deb, where the version
# carries the distribution tag (`0.1.0-1+deb12`); rpm:
# <name>-<version>-<release>.<arch>.rpm, where the release does (`1.el9`).
DISTRO_TAG = re.compile(r"(deb\d+|ubuntu[\d.]+|el\d+)")

# One spelling across the manifest. Each ecosystem names architectures its own
# way in file names — `x86_64` and `amd64` are the same machine — and a manifest
# that reported both would make "which architectures does this release cover"
# unanswerable without knowing the packaging conventions.
#
# Every place that compares an architecture goes through this, including the
# closure check below. Comparing a raw file-name architecture against a
# canonicalised one is a check that can only fail, and it failed silently for
# every RPM in the release.
ARCH_ALIASES = {
    "x86_64": "amd64", "amd64": "amd64",
    "aarch64": "arm64", "arm64": "arm64",
    "noarch": "all", "all": "all",
}


def canonical_arch(value):
    return ARCH_ALIASES.get(value, value)


artifacts, majors, arches, platforms = [], set(), set(), set()
for path in sorted(dist.glob("*")):
    if path.suffix not in (".deb", ".rpm") or not path.is_file():
        continue
    name = path.name

    if name.endswith(".deb"):
        arch = name.rsplit("_", 1)[1][: -len(".deb")]
    else:
        arch = name[: -len(".rpm")].rsplit(".", 1)[1]
    arch = canonical_arch(arch)

    platform_match = DISTRO_TAG.search(name)
    if not platform_match:
        sys.exit("%s does not identify its distribution in its file name" % name)

    pg_match = re.match(r"^postgresql-?(\d+)-postvec", name)
    pg_major = int(pg_match.group(1)) if pg_match else None

    record = {
        "name": name,
        "sha256": sha256(path),
        "size": path.stat().st_size,
        "platform": platform_match.group(1),
        "arch": arch,
        "format": path.suffix.lstrip("."),
        "package": package_name(name),
        "pg_major": pg_major,
    }
    # An SBOM published beside the package is only useful if the manifest says
    # which package it describes.
    sbom = path.with_name(name + ".spdx.json")
    if sbom.is_file():
        record["sbom"] = sbom.name
        record["sbom_sha256"] = sha256(sbom)
    artifacts.append(record)
    platforms.add(platform_match.group(1))
    if arch != "all":
        arches.add(arch)
    if pg_major:
        majors.add(pg_major)

if not artifacts:
    sys.exit("no .deb or .rpm artifacts under %s" % dist)
artifacts.sort(key=lambda a: a["name"])

# Release assets that are not packages. Today that is one file, and it is the
# one a user is told to execute — so it is described here rather than only
# appearing as a line in SHA256SUMS, and the release workflow attests it
# directly.
release_assets = []
BOOTSTRAP = "postvec-prerequisites.sh"
bootstrap_path = dist / BOOTSTRAP
if bootstrap_path.is_file():
    release_assets.append({
        "name": BOOTSTRAP,
        "sha256": sha256(bootstrap_path),
        "size": bootstrap_path.stat().st_size,
        "role": "prerequisites",
        "description": "Configures the PostgreSQL (PGDG) repositories these "
                       "packages need. Run it before installing anything.",
    })
elif os.environ.get("REQUIRE_IMAGES"):
    sys.exit("%s is missing: a release publishes the bootstrap its own "
             "instructions tell users to run" % BOOTSTRAP)

# The bundled model, as the registry describes it. The derived facts come from
# model-facts.env; the free-form provenance (`source`) comes from the payload's
# own SOURCE.json, which the bundle step rendered from the verified receipt —
# never from a shell variable, because free text read by shell is code.
model = {
    "name": env("MODEL_NAME"),
    "backend": env("MODEL_BACKEND"),
    "channel": "public",
    "registry_revision": int(env("MODEL_REGISTRY_REVISION")),
    "archive_sha256": env("MODEL_ARCHIVE_SHA256"),
    "archive_size": int(env("MODEL_ARCHIVE_SIZE")),
    "license": env("MODEL_LICENSE"),
    "target_dim": int(env("MODEL_TARGET_DIM")),
    "sequence_len": int(env("MODEL_SEQUENCE_LEN")),
    "bundle_version": int(env("MODEL_BUNDLE_VERSION")),
    "package": env("MODEL_PKG_NAME"),
}
source_json = pathlib.Path(os.environ["MODEL_SOURCE_JSON"])
if not source_json.is_file():
    sys.exit(
        "%s is missing: the manifest records the bundled model's provenance and "
        "per-file digests from the payload the packages were built from.\n"
        "Build it: scripts/build-model-bundle.sh" % source_json
    )
provenance = json.loads(source_json.read_text())
model["source"] = provenance["source"]
model["files_sha256"] = provenance["files_sha256"]

images = []
images_path = os.environ.get("IMAGES_JSON") or ""
if images_path:
    images = json.loads(pathlib.Path(images_path).read_text())

manifest_cells = json.loads(pathlib.Path(os.environ["BUILD_CELLS_JSON"]).read_text())

# The exact closure this release is supposed to contain. Two things make
# that stricter than a set membership test:
#
#   * exactly one artifact per (package, distro, arch). A stale build sitting
#     next to the current one collapses into the same tuple, so a set check
#     reports success and publishes both.
#   * each artifact's own identity: the version, the packaging revision, the
#     distribution suffix and the architecture in its file name must be this
#     release's. A file named postvec-cli_0.0.9-1+deb12_amd64.deb satisfies
#     "there is a postvec-cli for deb12/amd64" and is still wrong.

# The distribution *tag* that appears in artifact file names, per distribution
# id. Mirrors `distro_facts` in lib.sh, which is the definition of record.
DISTRO_TAGS = {
    "debian12": "deb12",
    "ubuntu2204": "ubuntu22.04",
    "ubuntu2404": "ubuntu24.04",
    "el9": "el9",
}

VERSION = env("POSTVEC_VERSION")
REVISION = env("PACKAGE_RELEASE")
MODEL_PACKAGE = env("MODEL_PKG_NAME")
MODEL_PACKAGE_VERSION = env("MODEL_PKG_VERSION")


def expected_packages():
    """{(package, distro_tag, arch): expected version} or None if not asked."""
    distro_ids = (os.environ.get("EXPECT_DISTROS") or "").split()
    expect_majors = [int(m) for m in (os.environ.get("EXPECT_MAJORS") or "").split()]
    expect_arches = (os.environ.get("EXPECT_ARCHES") or "").split()
    if not (distro_ids and expect_majors and expect_arches):
        return None
    wanted = {}
    for distro_id in distro_ids:
        if distro_id not in DISTRO_TAGS:
            sys.exit(f"unknown distribution {distro_id!r} in --expect-distros")
        family = "rpm" if distro_id.startswith("el") else "deb"
        distro = DISTRO_TAGS[distro_id]
        for arch in expect_arches:
            wanted[("postvec-cli", distro, arch)] = VERSION
            # The inference node: native, PostgreSQL-independent, one per
            # (distribution, architecture) like the CLI, and with symbols.
            wanted[("postvec-server", distro, arch)] = VERSION
            wanted[("postvec-onnxruntime", distro, arch)] = env("ORT_VERSION")
            for major in expect_majors:
                name = (f"postgresql{major}-postvec" if family == "rpm"
                        else f"postgresql-{major}-postvec")
                wanted[(name, distro, arch)] = VERSION
        # Architecture-independent: one per distribution, not per architecture.
        wanted[(MODEL_PACKAGE, distro, "all")] = MODEL_PACKAGE_VERSION
        wanted[(env("EXTRAS_METAPACKAGE"), distro, "all")] = VERSION
    return wanted


def artifact_identity(artifact):
    """(version, revision, distro_tag, arch) as the file name states them."""
    name = artifact["name"]
    if artifact["format"] == "deb":
        # <package>_<version>-<revision>+<distro>_<arch>.deb
        match = re.match(
            r"^.+_(?P<version>[0-9][^-_]*)-(?P<revision>\d+)\+(?P<distro>[a-z0-9.]+)_"
            r"(?P<arch>[a-z0-9_]+)\.deb$", name)
    else:
        # <package>-<version>-<revision>.<distro>.<arch>.rpm
        match = re.match(
            r"^.+-(?P<version>[0-9][^-]*)-(?P<revision>\d+)\.(?P<distro>el\d+)\."
            r"(?P<arch>[a-z0-9_]+)\.rpm$", name)
    if not match:
        return None
    return match.group("version"), match.group("revision"), match.group("distro"), match.group("arch")


# Packages built from postvec's own source carry symbols. The model bundle and
# the metapackage contain no ELF object at all, and postvec-onnxruntime
# redistributes an upstream binary whose symbols are not this project's to ship.
NO_DEBUG_PACKAGE = frozenset({
    MODEL_PACKAGE, env("EXTRAS_METAPACKAGE"), "postvec-onnxruntime",
})
DEBUG_SUFFIXES = ("-dbgsym", "-debuginfo")

wanted = expected_packages()
if wanted is not None:
    problems = []
    # Debug packages are part of the closure for a publishable build: both
    # builds emit line tables, so a missing one means the split silently did
    # not happen, and a release that advertises symbols without shipping them
    # is worse than one that never promised them.
    require_debug = bool(os.environ.get("REQUIRE_IMAGES"))

    def check_identity(artifact, expected_version, distro, arch):
        """The file name must state *this* release's identity, not merely parse.

        A file named postvec-cli_0.0.9-1+deb12_amd64.deb satisfies "there is a
        postvec-cli for deb12/amd64" and is still the wrong package.
        """
        identity = artifact_identity(artifact)
        if identity is None:
            problems.append("unparseable file name: %s" % artifact["name"])
            return
        version, revision, distro_tag, file_arch = identity
        if version != expected_version:
            problems.append(
                "%s is version %s, expected %s"
                % (artifact["name"], version, expected_version))
        if revision != REVISION:
            problems.append(
                "%s has packaging revision %s, expected %s"
                % (artifact["name"], revision, REVISION))
        if distro_tag != distro:
            problems.append(
                "%s says %s but was collected as %s"
                % (artifact["name"], distro_tag, distro))
        if canonical_arch(file_arch) != arch:
            problems.append(
                "%s is %s, expected %s" % (artifact["name"], file_arch, arch))

    def group(candidates, key_of):
        grouped = {}
        for artifact in candidates:
            grouped.setdefault(key_of(artifact), []).append(artifact)
        return grouped

    release_artifacts = [
        a for a in artifacts if not a["package"].endswith(DEBUG_SUFFIXES)
    ]
    debug_artifacts = [
        a for a in artifacts if a["package"].endswith(DEBUG_SUFFIXES)
    ]

    by_tuple = group(
        release_artifacts,
        lambda a: (a["package"], a["platform"], a["arch"]))
    # Keyed by the package the symbols belong to *and* by distribution and
    # architecture. Keying on the base name alone let one Debian/amd64 CLI debug
    # package satisfy all eight CLI tuples, and one extension debug package
    # satisfy every distribution and architecture for that major — a closure
    # check that could not fail is not a check.
    debug_by_tuple = group(
        debug_artifacts,
        lambda a: (a["package"].rsplit("-", 1)[0], a["platform"], a["arch"]))

    for key, found in sorted(by_tuple.items()):
        package, distro, arch = key
        if key not in wanted:
            problems.append(
                "unexpected: %s for %s/%s (%s)"
                % (package, distro, arch, ", ".join(a["name"] for a in found)))
            continue
        if len(found) > 1:
            problems.append(
                "duplicate: %d artifacts for %s on %s/%s — %s"
                % (len(found), package, distro, arch,
                   ", ".join(a["name"] for a in found)))
            continue
        check_identity(found[0], wanted[key], distro, arch)

    for key in sorted(set(wanted) - set(by_tuple)):
        problems.append("missing: %s for %s/%s" % key)

    # The debug closure, held to the same standard: exactly one artifact per
    # (package, distribution, architecture) that owes symbols, each carrying
    # this release's identity, and nothing else present.
    expected_debug = {
        key: version for key, version in wanted.items()
        if key[0] not in NO_DEBUG_PACKAGE
    }
    for key in sorted(expected_debug):
        package, distro, arch = key
        found = debug_by_tuple.get(key, [])
        if not found:
            if require_debug:
                problems.append(
                    "missing debug package for %s (%s/%s)" % key)
            continue
        if len(found) > 1:
            problems.append(
                "duplicate: %d debug packages for %s on %s/%s — %s"
                % (len(found), package, distro, arch,
                   ", ".join(a["name"] for a in found)))
            continue
        check_identity(found[0], expected_debug[key], distro, arch)

    for key in sorted(set(debug_by_tuple) - set(expected_debug)):
        problems.append(
            "unexpected debug package: %s for %s/%s (%s)"
            % (key[0], key[1], key[2],
               ", ".join(a["name"] for a in debug_by_tuple[key])))

    if require_debug:
        # `require_images` marks a publishable build; SBOMs are part of what
        # that publishes, so a package without one is an incomplete release.
        for artifact in artifacts:
            if not artifact.get("sbom") or not artifact.get("sbom_sha256"):
                problems.append("no SBOM recorded for %s" % artifact["name"])

    if problems:
        for problem in problems:
            print(problem, file=sys.stderr)
        sys.exit("the release does not contain exactly the expected packages")
    print("package closure verified: %d package(s) and %d debug package(s), "
          "each exactly once and at %s-%s"
          % (len(wanted), len(expected_debug) if require_debug else len(debug_by_tuple),
             VERSION, REVISION))

# The build cells: the exact set, for the same reason. 24 extension cells
# (distro × major × arch) plus 8 shared-package builders (distro × arch), each
# of which compiles its own PG 18 cell to produce the CLI.
if wanted is not None and os.environ.get("REQUIRE_IMAGES"):
    distro_ids = (os.environ.get("EXPECT_DISTROS") or "").split()
    expect_majors = [int(m) for m in (os.environ.get("EXPECT_MAJORS") or "").split()]
    expect_arches = (os.environ.get("EXPECT_ARCHES") or "").split()
    expected_cells = {
        "%s-pg%s-%s" % (distro, major, arch)
        for distro in distro_ids for major in expect_majors for arch in expect_arches
    } | {
        "common-%s-%s" % (distro, arch)
        for distro in distro_ids for arch in expect_arches
    }
    present_cells = {cell["cell"] for cell in manifest_cells}
    problems = []
    for cell in sorted(expected_cells - present_cells):
        problems.append("missing build record: %s" % cell)
    for cell in sorted(present_cells - expected_cells):
        problems.append("unexpected build record: %s" % cell)
    if problems:
        for problem in problems:
            print(problem, file=sys.stderr)
        sys.exit("the release does not describe exactly the expected build cells")
    print("build cells verified: %d record(s)" % len(present_cells))

# The build cells' recorded *facts*, not just their names.
#
# A cell-name set proves that 32 files arrived. It says nothing about what is
# in them, and the fields those files carry — the PostgreSQL version the cell
# resolved, the base it ran on, the compiler — are the whole reason the records
# exist: PGDG publishes no snapshots, so this is the only durable statement of
# what an artifact was built against. A fact that nothing checks is
# documentation, and documentation drifts.
#
# `base` is `${ID}-${VERSION_ID}` from the builder's own /etc/os-release, so an
# EL9 cell reports a point release (`almalinux-9.8`) and the check is a prefix.
DISTRO_BASE_PREFIX = {
    "debian12": "debian-12",
    "ubuntu2204": "ubuntu-22.04",
    "ubuntu2404": "ubuntu-24.04",
    "el9": "almalinux-9",
}

if wanted is not None and manifest_cells:
    problems = []
    for cell in sorted(manifest_cells, key=lambda c: c["cell"]):
        name = cell["cell"]
        major = cell["pg_major"]

        # The cell claims a major; the PostgreSQL it actually found must be it.
        # This is the check that catches a builder resolving PG 17 in the cell
        # named pg16 — which produces a package that installs and then refuses
        # to load.
        pg_version = cell.get("pg_version", "")
        if not re.match(r"^PostgreSQL %d\b" % major, pg_version):
            problems.append(
                "%s: recorded pg_version %r is not PostgreSQL %d"
                % (name, pg_version, major))

        expected_base = DISTRO_BASE_PREFIX.get(cell["distro"])
        if expected_base is None:
            problems.append("%s: unknown distribution %r" % (name, cell["distro"]))
        elif not str(cell.get("base", "")).startswith(expected_base):
            problems.append(
                "%s: built on %r, but %s means %s*"
                % (name, cell.get("base"), cell["distro"], expected_base))

        # The toolchain is pinned; a cell that resolved a different one is not
        # this release's build, whatever else is true of it.
        for field, pin in (("rustc", env("RUST_VERSION")),
                           ("cargo_pgrx", env("PGRX_VERSION"))):
            recorded = str(cell.get(field, ""))
            if pin not in recorded:
                problems.append(
                    "%s: %s is %r, expected the pinned %s"
                    % (name, field, recorded, pin))

        if not str(cell.get("glibc", "")).strip():
            problems.append("%s: no glibc recorded" % name)

    if problems:
        for problem in problems:
            print(problem, file=sys.stderr)
        sys.exit("a build cell's recorded facts contradict what it claims to be")
    print("build-cell facts verified: %d record(s)" % len(manifest_cells))

# The image closure: the same reasoning, for the seven published images — six
# database images in EXPECT_IMAGES, and the inference node's in its own
# repository. Seven copies of one descriptor satisfy "length == 7".
expect_repository = os.environ.get("EXPECT_IMAGES") or ""
if expect_repository:
    expect_majors = [int(m) for m in (os.environ.get("EXPECT_MAJORS") or "").split()]
    server_variant = env("SERVER_IMAGE_VARIANT")
    # The node's repository follows the database images' override, so a
    # disposable publication into `<x>` puts the node into `<x>-server` — the
    # same derivation the release workflow's `pins` step makes.
    server_repository = os.environ.get("EXPECT_SERVER_IMAGE") or env("SERVER_IMAGE_REPOSITORY")
    problems, seen_tags, seen = [], set(), {}
    for image in images:
        key = (image.get("pg_major"), image.get("variant"))
        if key in seen:
            problems.append("duplicate descriptor for PG %s %s" % key)
        seen[key] = image
        if image.get("variant") == server_variant:
            if image.get("name") != server_repository:
                problems.append("%s is not %s" % (image.get("name"), server_repository))
            if image.get("pg_major") is not None:
                problems.append("the server image claims PostgreSQL %s; it has no major"
                                % image.get("pg_major"))
            expected_tag = "%s-%s" % (VERSION, REVISION)
        else:
            if image.get("name") != expect_repository:
                problems.append("%s is not %s" % (image.get("name"), expect_repository))
            if image.get("variant") == env("LOCAL_IMAGE_VARIANT"):
                suffix = env("LOCAL_IMAGE_SUFFIX")
            else:
                suffix = env("REMOTE_IMAGE_SUFFIX")
            expected_tag = "%s-%s-pg%s%s" % (VERSION, REVISION, image.get("pg_major"), suffix)
        if image.get("tag") != expected_tag:
            problems.append("tag %r, expected %r" % (image.get("tag"), expected_tag))
        if image.get("tag") in seen_tags:
            problems.append("tag %s appears twice" % image.get("tag"))
        seen_tags.add(image.get("tag"))
        for required in ("digest", "base_digest"):
            if not image.get(required):
                problems.append("PG %s %s has no %s" % (key[0], key[1], required))

        # One SBOM per child manifest, not one per index. A scanner given a
        # multi-architecture index documents whichever child it defaults to
        # (linux/amd64), so a single document attested against the index is a
        # claim about arm64 that nothing produced and nothing checks.
        expected_platforms = {"linux/%s" % arch for arch in sorted(arches)}
        platforms = image.get("platforms") or []
        seen_platforms = {}
        for entry in platforms:
            platform = entry.get("platform")
            if platform in seen_platforms:
                problems.append(
                    "PG %s %s describes %s twice" % (key[0], key[1], platform))
            seen_platforms[platform] = entry
            for required in ("digest", "sbom", "sbom_sha256"):
                if not entry.get(required):
                    problems.append(
                        "PG %s %s %s has no %s" % (key[0], key[1], platform, required))
        for platform in sorted(expected_platforms - set(seen_platforms)):
            problems.append("PG %s %s has no %s SBOM" % (key[0], key[1], platform))
        for platform in sorted(set(seen_platforms) - expected_platforms):
            problems.append(
                "PG %s %s describes unexpected platform %s" % (key[0], key[1], platform))
        child_digests = [e.get("digest") for e in platforms]
        if len(set(child_digests)) != len(child_digests):
            problems.append(
                "PG %s %s: two architectures share a child digest" % key)
        if image.get("digest") in child_digests:
            problems.append(
                "PG %s %s: a child manifest has the index's own digest" % key)

    expected_keys = {(major, variant)
                     for major in expect_majors
                     for variant in (env("REMOTE_IMAGE_VARIANT"), env("LOCAL_IMAGE_VARIANT"))}
    expected_keys.add((None, server_variant))
    # `None` and an int do not order together; sort on the string form.
    for key in sorted(expected_keys - set(seen), key=str):
        problems.append("missing image: PG %s %s" % key)
    for key in sorted(set(seen) - expected_keys, key=str):
        problems.append("unexpected image: PG %s %s" % key)

    # The base each image was built on must be the digest this release pinned.
    # An image built on an unpinned base is not the image this release
    # describes, however correct its own content is. The database images sit
    # on the pinned postgres image for their major; the node sits on the
    # pinned Debian 12 base, which is also the builder its packages came from.
    for (major, variant), image in sorted(seen.items(), key=str):
        if variant == server_variant:
            pinned = os.environ.get("BUILD_BASE_DEBIAN12_DIGEST")
        else:
            pinned = os.environ.get("POSTGRES_IMAGE_PG%s_DIGEST" % major)
        if not pinned:
            # No pin to compare against is a hole in the check, not a pass.
            problems.append("no pinned base image for PG %s %s" % (major, variant))
        elif image.get("base_digest") != pinned:
            problems.append(
                "PG %s %s was built on %s, but this release pins %s"
                % (major, variant, image.get("base_digest"), pinned))

    digests = [i.get("digest") for i in images]
    if len(set(digests)) != len(digests):
        problems.append("two images share a digest; they cannot both be what they claim")

    if problems:
        for problem in problems:
            print(problem, file=sys.stderr)
        sys.exit("the release does not contain exactly the expected images")
    print("image closure verified: %d image(s)" % len(images))

manifest = {
    "schema": 1,
    "postvec_version": env("POSTVEC_VERSION"),
    "package_release": env("PACKAGE_RELEASE"),
    "release_id": "%s-%s" % (env("POSTVEC_VERSION"), env("PACKAGE_RELEASE")),
    "git_commit": env("GIT_COMMIT"),
    "source_date_epoch": int(env("SOURCE_DATE_EPOCH")),
    # Every reviewed pin, verbatim. A selection would drop the per-architecture
    # digests a reproducer needs.
    "inputs": {
        "rust": env("RUST_VERSION"),
        "cargo_pgrx": env("PGRX_VERSION"),
        "rustup": {
            "version": env("RUSTUP_VERSION"),
            "x86_64_sha256": env("RUSTUP_INIT_X86_64_SHA256"),
            "aarch64_sha256": env("RUSTUP_INIT_AARCH64_SHA256"),
        },
        "protoc": {
            "version": env("PROTOC_VERSION"),
            "x86_64_sha256": env("PROTOC_LINUX_X86_64_SHA256"),
            "aarch64_sha256": env("PROTOC_LINUX_AARCH64_SHA256"),
        },
        "nfpm": {"version": env("NFPM_VERSION"), "image": env("NFPM_IMAGE")},
        # Pinning the setup action does not pin these: without an explicit
        # version the action installs whatever buildx is current, and the
        # container driver pulls BuildKit by tag.
        "buildx": {"version": env("BUILDX_VERSION"), "buildkit_image": env("BUILDKIT_IMAGE")},
        "onnxruntime": {
            "version": env("ORT_VERSION"),
            "url_base": env("ORT_URL_BASE"),
            "x64_sha256": env("ORT_LINUX_X64_SHA256"),
            "aarch64_sha256": env("ORT_LINUX_AARCH64_SHA256"),
        },
        "pgvector_min": env("PGVECTOR_MIN_VERSION"),
        # PGDG's packages cannot be pinned by content — it publishes no
        # snapshots — so what is pinned is the trust root. The resolved
        # PostgreSQL version of each cell is under `build_cells`.
        "pgdg": {
            "debian_key_fingerprint": env("PGDG_DEBIAN_KEY_FINGERPRINT"),
            "el9_repo_rpm_x86_64_sha256": env("PGDG_EL9_REPO_RPM_X86_64_SHA256"),
            "el9_repo_rpm_aarch64_sha256": env("PGDG_EL9_REPO_RPM_AARCH64_SHA256"),
        },
        "base_images": {
            "postgres_pg16": env("POSTGRES_IMAGE_PG16_DIGEST"),
            "postgres_pg17": env("POSTGRES_IMAGE_PG17_DIGEST"),
            "postgres_pg18": env("POSTGRES_IMAGE_PG18_DIGEST"),
            "builder_debian12": env("BUILD_BASE_DEBIAN12_DIGEST"),
            "builder_ubuntu2204": env("BUILD_BASE_UBUNTU2204_DIGEST"),
            "builder_ubuntu2404": env("BUILD_BASE_UBUNTU2404_DIGEST"),
            "builder_el9": env("BUILD_BASE_EL9_DIGEST"),
        },
        "sbom": {
            "tool": "syft",
            "version": env("SYFT_VERSION"),
            "sha256": env("SYFT_LINUX_AMD64_SHA256"),
        },
    },
    # What each builder actually resolved — PostgreSQL version, compiler, glibc.
    # PGDG publishes no snapshots, so this is the only record of it.
    "build_cells": manifest_cells,
    # Kept at the top level as well: these three are what most consumers read.
    "rust": env("RUST_VERSION"),
    "cargo_pgrx": env("PGRX_VERSION"),
    "pgvector_min": env("PGVECTOR_MIN_VERSION"),
    "onnxruntime": env("ORT_VERSION"),
    # Every public package is the embedded-capable build.
    "extension_features": ["embedded", "onnx"],
    # The one artifact family under different terms. Recorded so a consumer
    # of the manifest need not open a package to learn which licence the node
    # carries; every other package and image is PostgreSQL-licensed.
    "licenses": {
        "default": "PostgreSQL",
        "postvec-server": env("SERVER_LICENSE"),
    },
    "postgresql_majors": sorted(majors),
    "architectures": sorted(arches),
    "bundled_model": model,
    "artifacts": artifacts,
    "release_assets": release_assets,
    "images": images,
}
if os.environ.get("GIT_TAG"):
    manifest["git_tag"] = os.environ["GIT_TAG"]
manifest["built_at"] = datetime.datetime.now(datetime.timezone.utc).replace(
    microsecond=0).isoformat().replace("+00:00", "Z")

(dist / "postvec-release.json").write_text(
    json.dumps(manifest, indent=2, sort_keys=True) + "\n")

# SHA256SUMS in the format `sha256sum --check` expects.
#
# It lists every asset in the release, which is what makes it useful for
# validating a *release*. It is not what a user has: somebody who downloaded two
# packages holds two of eighty files, and a plain `--check` reports the other
# seventy-eight as failures. The documented command is therefore
# `sha256sum --ignore-missing --check SHA256SUMS`, which verifies exactly what
# is present and still errors when nothing matched at all.
with open(dist / "SHA256SUMS", "w") as handle:
    for entry in sorted(artifacts + release_assets, key=lambda e: e["name"]):
        handle.write("%s  %s\n" % (entry["sha256"], entry["name"]))

print("%d artifact(s), %d image(s), PG %s, %s" % (
    len(artifacts), len(images),
    ",".join(str(m) for m in sorted(majors)) or "-",
    ",".join(sorted(arches))))
PY

# Validate against the published schema when a validator is available, so a
# manifest consumer's contract is checked at the point it is produced.
validate_schema() {
    python3 - "${PKG_DIR}/release-manifest.schema.json" "${STAGING}/postvec-release.json" <<'PY'
import json, sys

try:
    from jsonschema import Draft202012Validator
except ImportError:
    sys.exit("jsonschema is not installed, or is too old to expose Draft202012Validator")

schema = json.load(open(sys.argv[1]))
# Named explicitly rather than inferred from $schema: jsonschema 3.x does not
# know Draft 2020-12 and silently falls back to Draft 7, where `prefixItems`,
# `const` semantics and unevaluated keywords differ — so the manifest would
# "validate" against a schema nobody wrote.
Draft202012Validator.check_schema(schema)
errors = sorted(Draft202012Validator(schema).iter_errors(json.load(open(sys.argv[2]))),
                key=lambda e: list(e.path))
for error in errors:
    print("  %s: %s" % ("/".join(str(p) for p in error.path) or "<root>", error.message),
          file=sys.stderr)
if errors:
    sys.exit("the manifest does not match release-manifest.schema.json")
print("manifest validates against release-manifest.schema.json (Draft 2020-12)")
PY
}

if (( REQUIRE_SCHEMA )); then
    validate_schema || die "the manifest could not be validated.
A publishable manifest must be checked against its own schema; a build that
cannot validate must not publish. Install jsonschema >= 4."
elif python3 -c 'import jsonschema' 2>/dev/null; then
    validate_schema || warn "the manifest did not validate (not a publishable build)"
else
    warn "jsonschema is not installed — manifest not schema-validated"
fi

# Only now does the release directory exist. Anything that was there before is
# gone with it: a release directory holds one release.
rm -rf "${OUT}"
mv "${STAGING}" "${OUT}"
trap - EXIT
rm -f "${BUILD_CELLS_JSON}"

log "wrote ${OUT}/postvec-release.json and ${OUT}/SHA256SUMS"

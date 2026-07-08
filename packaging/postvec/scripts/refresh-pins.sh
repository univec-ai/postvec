#!/usr/bin/env bash
# Re-resolve the pinned inputs and print the versions.env lines they imply.
#
#   refresh-pins.sh              # everything
#   refresh-pins.sh images       # just the base-image digests
#   refresh-pins.sh ort model rustup images
#
# It prints; it never edits. Changing a pin means changing what every user
# downloads, and that is a review event — a script that silently rewrote
# versions.env would turn "verified input" into "whatever upstream serves
# today", which is the thing the pins exist to prevent.
#
# Compare the output with versions.env by hand, understand every difference,
# then edit deliberately.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

load_versions
need curl python3

want() { (($# == 0)) || [[ " ${TOPICS[*]} " == *" $1 "* ]]; }
TOPICS=("$@")
((${#TOPICS[@]})) || TOPICS=(ort model rustup protoc syft images buildx node python)

if want ort; then
    echo "# ONNX Runtime ${ORT_VERSION}"
    for pair in "x64:ORT_LINUX_X64_SHA256" "aarch64:ORT_LINUX_AARCH64_SHA256"; do
        arch="${pair%%:*}"; var="${pair##*:}"
        url="${ORT_URL_BASE}/onnxruntime-linux-${arch}-${ORT_VERSION}.tgz"
        digest="$(curl --fail --silent --show-error --location "${url}" | sha256sum | awk '{print $1}')"
        printf '%s=%s\n' "${var}" "${digest}"
    done
    echo
fi

if want model; then
    # The registry's current head for the bundled name, read by the same client
    # a user runs. Using the CLI rather than fetching the index here keeps the
    # index URL where it belongs — a compiled-in client constant — instead of
    # copying it into packaging, and it applies the same channel and extraction
    # policy a real install does.
    echo "# ${BUNDLED_MODEL_NAME} — the registry's current public head"
    cli=""
    for candidate in "${POSTVEC_CLI:-}" \
                     "${REPO_ROOT}/target/release/postvec" \
                     "${REPO_ROOT}/target/debug/postvec"; do
        [[ -n "${candidate}" && -x "${candidate}" ]] && { cli="${candidate}"; break; }
    done
    [[ -n "${cli}" ]] || die "no postvec CLI to read the registry with.
Build one: cargo build -p postvec-cli, or set POSTVEC_CLI=/path/to/postvec"

    tmp="$(mktemp_trusted_dir refresh-pins)"
    anonymous_model_pull "${cli}" "${tmp}" "${BUNDLED_MODEL_NAME}"
    python3 - "${tmp}/models" <<'PY'
import json, pathlib, sys

roots = [p for p in pathlib.Path(sys.argv[1]).glob("*/*/.postvec-install.json")]
receipts = {p.parent.name: json.loads(p.read_text()) for p in roots}
name = __import__("os").environ["BUNDLED_MODEL_NAME"]
receipt = receipts.get(name)
if receipt is None:
    sys.exit("the pull installed %s, not %s" % (sorted(receipts) or "nothing", name))
digest = receipt["archive_digest"]
digest = digest[len("sha256:"):] if digest.startswith("sha256:") else digest
print("BUNDLED_MODEL_REGISTRY_REVISION=%s" % receipt.get("revision", 1))
print("BUNDLED_MODEL_ARCHIVE_SHA256=%s" % digest)
if len(receipts) > 1:
    print("# note: the pull also installed %s — a closure is a packaging decision"
          % ", ".join(sorted(set(receipts) - {name})))
PY
    rm -rf "${tmp}"
    echo
fi

if want rustup; then
    echo "# rustup-init ${RUSTUP_VERSION}"
    for pair in "x86_64-unknown-linux-gnu:RUSTUP_INIT_X86_64_SHA256" \
                "aarch64-unknown-linux-gnu:RUSTUP_INIT_AARCH64_SHA256"; do
        triple="${pair%%:*}"; var="${pair##*:}"
        digest="$(curl --fail --silent --show-error \
            "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/${triple}/rustup-init.sha256" \
            | awk '{print $1}')"
        printf '%s=%s\n' "${var}" "${digest}"
    done
    echo
fi

if want protoc; then
    echo "# protoc ${PROTOC_VERSION}"
    for pair in "linux-x86_64:PROTOC_LINUX_X86_64_SHA256" \
                "linux-aarch_64:PROTOC_LINUX_AARCH64_SHA256"; do
        arch="${pair%%:*}"; var="${pair##*:}"
        url="https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/protoc-${PROTOC_VERSION}-${arch}.zip"
        printf '%s=%s\n' "${var}" \
            "$(curl --fail --silent --show-error --location "${url}" | sha256sum | awk '{print $1}')"
    done
    echo
fi

if want syft; then
    echo "# syft ${SYFT_VERSION}"
    url="https://github.com/anchore/syft/releases/download/v${SYFT_VERSION}/syft_${SYFT_VERSION}_linux_amd64.tar.gz"
    printf 'SYFT_LINUX_AMD64_SHA256=%s\n' \
        "$(curl --fail --silent --show-error --location "${url}" | sha256sum | awk '{print $1}')"
    echo
fi

# The registry API rather than `docker pull`: a digest must be resolved from the
# registry, not from whatever is in a local image cache. `${1}` is the full
# repository path — `library/debian` for an official image, `moby/buildkit` for
# anything else.
resolve() {
    local repo="$1" tag="$2" token
    token="$(curl --fail --silent --show-error \
        "https://auth.docker.io/token?service=registry.docker.io&scope=repository:${repo}:pull" \
        | python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])')"
    curl --fail --silent --show-error --head \
        --header "Authorization: Bearer ${token}" \
        --header "Accept: application/vnd.oci.image.index.v1+json,application/vnd.docker.distribution.manifest.list.v2+json" \
        "https://registry-1.docker.io/v2/${repo}/manifests/${tag}" \
        | tr -d '\r' | awk 'tolower($1) == "docker-content-digest:" {print $2}'
}

if want images; then
    echo "# base images (multi-arch index digests)"
    for major in $(supported_pg_majors); do
        printf 'POSTGRES_IMAGE_PG%s_DIGEST=%s\n' "${major}" "$(resolve library/postgres "${major}-bookworm")"
    done
    printf 'BUILD_BASE_DEBIAN12_DIGEST=%s\n'   "$(resolve library/debian bookworm-slim)"
    printf 'BUILD_BASE_UBUNTU2204_DIGEST=%s\n' "$(resolve library/ubuntu 22.04)"
    printf 'BUILD_BASE_UBUNTU2404_DIGEST=%s\n' "$(resolve library/ubuntu 24.04)"
    printf 'BUILD_BASE_EL9_DIGEST=%s\n'        "$(resolve library/almalinux 9)"
    echo
fi

if want node; then
    # Same rule as BuildKit: the pinned node line, re-resolved — not the
    # newest major.
    echo "# node, for the postvec-server dashboard"
    node_ref="${NODE_IMAGE%@*}"; node_repo="${node_ref%%:*}"
    printf 'NODE_IMAGE=%s@%s\n' "${node_ref}" "$(resolve "${node_repo#docker.io/}" "${node_ref##*:}")"
    echo
fi

if want buildx; then
    # Re-resolves the digest of the *currently pinned* BuildKit tag rather than
    # chasing the newest release: moving to a new BuildKit line is a decision,
    # and this script exists to detect a moved tag, not to make that decision.
    echo "# buildx / BuildKit (${BUILDX_VERSION})"
    buildkit_ref="${BUILDKIT_IMAGE%@*}"
    printf 'BUILDKIT_IMAGE=%s@%s\n' "${buildkit_ref}" \
        "$(resolve "${buildkit_ref%%:*}" "${buildkit_ref##*:}")"
    echo "# latest upstream releases, for comparison only:"
    for repo in docker/buildx moby/buildkit; do
        printf '#   %-16s %s\n' "${repo}" \
            "$(curl --fail --silent --show-error "https://api.github.com/repos/${repo}/releases/latest" \
                | python3 -c 'import json,sys; print(json.load(sys.stdin)["tag_name"])')"
    done
    echo
fi

if want python; then
    # The schema validator's whole dependency set, hash-locked. Every
    # distribution PyPI publishes for each pinned version is listed, so the
    # lock does not silently become a claim about one runner's architecture
    # and Python version.
    echo "# packaging/postvec/requirements-schema.txt — replace the body below the header"
    python3 - "${PKG_DIR}/requirements-schema.txt" <<'PY'
import json, re, sys, urllib.request

# The pinned versions are read back from the lock file itself: this script
# re-resolves hashes, it does not choose versions. A version bump is a review
# event, made by editing the file.
pins = re.findall(r"^([A-Za-z0-9_.-]+)==([^\s\\]+)", open(sys.argv[1]).read(), re.M)
if not pins:
    sys.exit("no pinned requirements found in %s" % sys.argv[1])
for name, version in pins:
    url = "https://pypi.org/pypi/%s/%s/json" % (name, version)
    with urllib.request.urlopen(url) as response:
        release = json.load(response)
    digests = sorted({f["digests"]["sha256"] for f in release["urls"]})
    if not digests:
        sys.exit("%s==%s publishes no files" % (name, version))
    print("%s==%s \\" % (name, version))
    for index, digest in enumerate(digests):
        print("    --hash=sha256:%s%s"
              % (digest, "" if index == len(digests) - 1 else " \\"))
    print()
PY
fi

log "compare the above with packaging/postvec/versions.env — this script changes nothing"

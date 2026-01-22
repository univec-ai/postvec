#!/usr/bin/env bash
# Render every nfpm package description with stub values and check the result.
#
#   scripts/lint-nfpm-configs.sh
#
# The templates are not valid YAML on their own — `${SHLIB_DEPENDS}` expands to
# a block of list items, so it necessarily sits at column zero before
# rendering. Parsing the template would therefore fail, and skipping the check
# would leave a whole class of typo (a mis-indented `contents:` entry, a
# duplicated key) to be discovered by a release. Render first, then parse.
#
# This also catches a placeholder the renderer does not know about: the
# allowlist in lib.sh is the same one build-packages.sh fills, so a `${FOO}`
# nobody sets fails here rather than three minutes into a matrix job.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

load_versions
need python3

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

# Plausible stubs, not real values: this checks shape, not content. They are
# shaped like the real thing (a package name looks like a package name) so that
# a template using one in the wrong place still stands out.
export CELL_DIR="${WORK}/cell" \
       ORT_PAYLOAD_DIR="${WORK}/payload" \
       MODEL_PAYLOAD_DIR="${WORK}/payload-common" \
       PKGLIBDIR=/usr/lib/postgresql/18/lib \
       EXTENSIONDIR=/usr/share/postgresql/18/extension \
       EXTENSION_PACKAGE=postgresql-18-postvec \
       DEBUG_PACKAGE=postgresql-18-postvec-dbgsym \
       DEBUG_SUFFIX=dbgsym \
       PG_MAJOR=18 \
       PKG_RELEASE=1+deb12 \
       PKG_ARCH=amd64 \
       NOARCH=all \
       LICENSE_DST=/usr/share/doc \
       LICENSE_NAME=copyright \
       CHANGELOG_FILE="${WORK}/changelog.Debian.gz" \
       CHANGELOG_NAME=changelog.Debian.gz \
       MODEL_LICENSE_SRC="${WORK}/model-copyright" \
       SOURCE_DATE_ISO=1970-01-01T00:00:00Z \
       MODEL_PKG_NAME=postvec-model-example \
       MODEL_PKG_VERSION=2.1.0 \
       MODEL_DOC_DIR=/usr/share/doc/postvec-model-example \
       MODEL_NAME=vendor-example-model \
       MODEL_BACKEND=onnx-runtime \
       MODEL_LICENSE=Apache-2.0 \
       MODEL_TARGET_DIM=384 \
       MODEL_SEQUENCE_LEN=256 \
       MODEL_REGISTRY_REVISION=2 \
       MODEL_ARCHIVE_SHA256=0000000000000000000000000000000000000000000000000000000000000000 \
       SHLIB_DEPENDS='      - libc6 (>= 2.34)'

failed=0
for template in "${PKG_DIR}"/nfpm/*.yaml; do
    name="$(basename "${template}")"
    rendered="${WORK}/${name}"
    if ! render_nfpm_config "${template}" "${rendered}"; then
        printf '  \033[1;31mFAIL\033[0m  %s: unresolved placeholders\n' "${name}" >&2
        failed=1
        continue
    fi
    if python3 - "${rendered}" "${name}" <<'PY'
import sys
try:
    import yaml
except ImportError:
    sys.exit("pyyaml is required to lint the package descriptions")

path, name = sys.argv[1], sys.argv[2]
document = yaml.safe_load(open(path))

problems = []
for required in ("name", "arch", "version", "maintainer", "description"):
    if not document.get(required):
        problems.append(f"missing {required}")
# Every content entry must say where it comes from and where it goes; a `dst`
# with no `src` silently packages nothing.
for entry in document.get("contents", []):
    if not entry.get("dst"):
        problems.append(f"a contents entry has no dst: {entry}")
    if not entry.get("src"):
        problems.append(f"a contents entry has no src: {entry}")
if problems:
    for problem in problems:
        print(f"  FAIL  {name}: {problem}", file=sys.stderr)
    sys.exit(1)
PY
    then
        printf '  ok    %s\n' "${name}"
    else
        failed=1
    fi
done

if (( failed )); then
    die "package descriptions did not lint"
fi
log "all package descriptions render and parse"

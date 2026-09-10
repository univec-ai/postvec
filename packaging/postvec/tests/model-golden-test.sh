#!/usr/bin/env bash
# Prove the bundled model still produces the same embeddings.
#
#   tests/model-golden-test.sh <image>            # check model/golden/<name>.json
#   tests/model-golden-test.sh --record <image>   # regenerate the goldens
#
# This is the check that protects stored data. A model bundle's name is a
# database compatibility identifier: `postvec.models` and every registry row
# store it, and a vector is only comparable to vectors produced by the same
# weights, tokenizer, pooling and normalisation. The package version is not
# part of that identity; nothing in a database records it. A bundle whose
# numbers moved must be published under a new internal name.
#
# Dimension, finiteness, unit norm and "does the obvious sentence rank first"
# all survive a tokenizer change, a pooling change, a different opset, or a
# quantised graph slipping into the bundle. Comparing the numbers catches
# those.
#
# Tolerance is per-element and absolute: ONNX Runtime does not promise
# bit-identical results across CPU microarchitectures, so this must pass on
# amd64 and arm64 without being loose enough to miss a behavioural change.

set -Eeuo pipefail

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(cd "${TESTS_DIR}/.." && pwd)"

# Goldens are keyed by model name, and the name comes from the reviewed pin —
# not from inside a single golden file. Reading the name out of the goldens
# would make a bundled-model change compare the new model's vectors against the
# old model's file and pass, which is exactly the accident this test exists to
# prevent.
MODEL="$(sed -n 's/^BUNDLED_MODEL_NAME=//p' "${PKG_DIR}/versions.env")"
[[ -n "${MODEL}" ]] || { echo "no BUNDLED_MODEL_NAME in ${PKG_DIR}/versions.env" >&2; exit 2; }
GOLDEN="${PKG_DIR}/model/golden/${MODEL}.json"

RECORD=0
IMAGE=""
while (($#)); do
    case "$1" in
    --record) RECORD=1; shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) IMAGE="$1"; shift ;;
    esac
done
[[ -n "${IMAGE}" ]] || { echo "usage: model-golden-test.sh [--record] <image>" >&2; exit 2; }

command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }

if [[ ! -f "${GOLDEN}" ]] && (( ! RECORD )); then
    cat >&2 <<EOF
no goldens recorded for '${MODEL}' — expected ${GOLDEN#"${PKG_DIR}/"}

A bundled model without recorded embeddings has no behaviour gate at all. If
this model is genuinely new, record them once from a reviewed image and review
the diff:

    tests/model-golden-test.sh --record ${IMAGE}

If it is not new, the goldens file was renamed or removed — restore it. Never
re-record to make an existing model pass: vectors already stored under this
name came from the numbers in that file.
EOF
    exit 1
fi

RUN_ID="postvec-golden-$$"
trap 'docker rm --force "${RUN_ID}" >/dev/null 2>&1 || true' EXIT

# The sentences are fixed inputs, not a sample: changing them invalidates every
# recorded vector. They exercise ordinary prose, a near-duplicate (small
# distances are where drift shows first), punctuation and a short fragment.
TEXTS=(
    "Postgres can run this embedding locally"
    "Postgres can run this embedding locally."
    "migrating embedding models normally requires re-embedding all source text"
    "the office plants need watering twice a week"
    "vector"
)

docker run --detach --name "${RUN_ID}" \
    --env POSTGRES_PASSWORD=golden --env POSTGRES_USER=app --env POSTGRES_DB=app \
    "${IMAGE}" >/dev/null

deadline=$(( SECONDS + 300 ))
until [[ "$(docker inspect --format '{{.State.Health.Status}}' "${RUN_ID}" 2>/dev/null)" == healthy ]]; do
    (( SECONDS < deadline )) || { docker logs --tail 30 "${RUN_ID}" >&2; echo "image never became healthy" >&2; exit 1; }
    sleep 2
done

embed() {
    docker exec "${RUN_ID}" psql -U app -d app -tAX -v ON_ERROR_STOP=1 \
        -c "SELECT array_to_json(postvec.embed(\$\$${1}\$\$, '${MODEL}'))"
}

observed='[]'
for text in "${TEXTS[@]}"; do
    vector="$(embed "${text}")"
    observed="$(jq --argjson v "${vector}" --arg t "${text}" \
        '. + [{text: $t, vector: $v}]' <<<"${observed}")"
done

if (( RECORD )); then
    if [[ ! -f "${GOLDEN}" ]]; then
        # First recording for a newly bundled model. The prose and the
        # tolerance are the contract; only the numbers are measured.
        mkdir -p "$(dirname "${GOLDEN}")"
        cat > "${GOLDEN}" <<'EOF'
{
  "_comment": [
    "Golden embeddings for the bundled model, recorded from the reviewed",
    "registry archive. tests/model-golden-test.sh checks a built image",
    "against these.",
    "",
    "The tolerance is per-element absolute error. ONNX Runtime does not",
    "promise bit-identical results across CPU microarchitectures, so exact",
    "equality would fail on hardware that is working correctly.",
    "",
    "Regenerate with: tests/model-golden-test.sh --record <image>. Doing so",
    "is a deliberate act: if the numbers moved, the model changed, and a",
    "changed model needs a new internal name."
  ],
  "model": null,
  "dimensions": null,
  "element_tolerance": 0.001,
  "cases": []
}
EOF
    fi
    # `model` and `dimensions` are recorded, not assumed: the lint job checks
    # both without a container, so they have to be facts of the file rather
    # than something a reader has to fill in after recording.
    jq --argjson cases "${observed}" \
       --arg model "${MODEL}" \
       '.cases = $cases | .model = $model
        | .dimensions = ($cases[0].vector | length)' "${GOLDEN}" > "${GOLDEN}.new"
    mv "${GOLDEN}.new" "${GOLDEN}"
    echo "recorded $(jq '.cases | length' "${GOLDEN}") golden case(s) into ${GOLDEN}"
    echo
    echo "These numbers are now a compatibility contract. If you regenerated"
    echo "them because the model changed, publish it under a new internal name:"
    echo "vectors already stored under '${MODEL}' were produced by the old one."
    exit 0
fi

jq -e '.cases | length > 0' "${GOLDEN}" >/dev/null || {
    cat >&2 <<EOF
no goldens recorded for '${MODEL}' — ${GOLDEN#"${PKG_DIR}/"} has no cases.

Record them from a reviewed image before the first release:
    tests/model-golden-test.sh --record ${IMAGE}
EOF
    exit 1
}
# The file has to be the right model's. It is selected by name, so this can
# only fire if a file was hand-edited or copied — which is worth catching
# loudly rather than comparing against another model's vectors.
[[ "$(jq -r .model "${GOLDEN}")" == "${MODEL}" ]] \
    || { echo "${GOLDEN} records model '$(jq -r .model "${GOLDEN}")', not '${MODEL}'" >&2; exit 1; }

# The comparison itself: per-element absolute difference against the recorded
# vectors, reported as the first thing anyone would want to know.
GOLDEN="${GOLDEN}" python3 - "${observed}" <<'PY'
import json, os, sys

golden = json.load(open(os.environ["GOLDEN"]))
observed = json.loads(sys.argv[1])
expected = {case["text"]: case["vector"] for case in golden["cases"]}
tolerance = golden["element_tolerance"]

problems = []
for case in observed:
    text, vector = case["text"], case["vector"]
    want = expected.get(text)
    if want is None:
        problems.append((text, "no golden vector is recorded for this input"))
        continue
    if len(want) != len(vector):
        problems.append((text, f"dimension {len(vector)}, expected {len(want)}"))
        continue
    if any(v != v or v in (float("inf"), float("-inf")) for v in vector):
        problems.append((text, "the vector contains a NaN or an infinity"))
        continue
    drift = max(abs(a - b) for a, b in zip(vector, want))
    if drift > tolerance:
        problems.append((text, f"largest element difference {drift:.6f} exceeds {tolerance}"))

if problems:
    for text, problem in problems:
        print(f"FAIL  {text!r}: {problem}", file=sys.stderr)
    print(f"""
The bundled model no longer produces the embeddings it was published with.
Vectors already stored under {golden["model"]!r} came from the old one, so
this is a different model rather than a new build of the same one — publish it
under a new internal name and require an explicit migration.

If the change really is intentional and the name really is new, re-record:
    tests/model-golden-test.sh --record <image>
""", file=sys.stderr)
    sys.exit(1)

print(f"ok    {len(observed)} golden case(s) match within {tolerance}")
PY

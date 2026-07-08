#!/usr/bin/env bash
# Build the postvec-server dashboard into a packageable payload.
#
#   build-ui-bundle.sh            # in the pinned node container (releases)
#   build-ui-bundle.sh --host     # with the host's npm, for iteration
#
# Writes build/payload-ui/opt/postvec/server/ui/ — the directory the node
# serves by default (<root>/server/ui) — plus, under
# usr/share/doc/postvec-server/, the third-party notices of every runtime npm
# dependency the bundle inlines and a SOURCE.json recording what was built
# from what. Architecture-independent: built once per release.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

HOST_BUILD=0
while (($#)); do
    case "$1" in
    --host) HOST_BUILD=1; shift ;;
    -h|--help) sed -n '2,11p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done

load_versions
need git find sha256sum

SRC="${REPO_ROOT}/postvec-server/web-ui"
PAYLOAD="${PKG_DIR}/build/payload-ui"
DEST="${PAYLOAD}/opt/postvec/server/ui"
DOC="${PAYLOAD}/usr/share/doc/postvec-server"
[[ -f "${SRC}/package-lock.json" ]] || die "no lockfile at ${SRC}/package-lock.json"

rm -rf "${PAYLOAD}"
mkdir -p "${DEST}" "${DOC}"

# The build itself, as one script so the container and host paths are the
# same. It writes dist/ and the notices file into the output directory.
# Only runtime dependencies are inlined into the bundle, so only those are
# listed; the build tools (vite, esbuild, …) never ship.
read -r -d '' BUILD <<'SH' || true
set -eu
cd "$1"
npm ci --no-audit --no-fund --ignore-scripts
npm run build -- --outDir "$2"
npm ls --omit=dev --all --parseable 2>/dev/null | tail -n +2 | LC_ALL=C sort -u | node -e '
  const fs = require("fs"), p = require("path");
  const lines = fs.readFileSync(0, "utf8").trim().split("\n").filter(Boolean);
  for (const dir of lines) {
    const j = JSON.parse(fs.readFileSync(p.join(dir, "package.json"), "utf8"));
    const lic = typeof j.license === "string" ? j.license : (j.license && j.license.type) || "UNKNOWN";
    const text = ["LICENSE", "LICENSE.md", "LICENSE.txt", "LICENCE", "license", "License.txt"]
      .map(n => p.join(dir, n)).find(f => fs.existsSync(f));
    process.stdout.write(`${"=".repeat(72)}\n${j.name} ${j.version} — ${lic}\n${j.homepage || ""}\n\n`);
    if (text) process.stdout.write(fs.readFileSync(text, "utf8").trim() + "\n\n");
  }' > "$3"
SH

if ((HOST_BUILD)); then
    need npm node
    work="$(mktemp -d)"
    trap 'rm -rf "${work}"' EXIT
    cp -r "${SRC}/." "${work}/"
    rm -rf "${work}/node_modules" "${work}/dist"
    bash -c "${BUILD}" _ "${work}" "${DEST}" "${DOC}/dashboard-ThirdPartyNotices.txt"
    node_used="host $(node --version)"
else
    need docker
    # The source is mounted read-only and copied inside: npm writes
    # node_modules next to package.json, and a release must not leave a
    # host checkout's node_modules or dist behind. Both mounts are inside
    # the checkout, never /tmp: a snap-confined docker silently mounts an
    # empty directory for host paths it cannot read (see lib.sh), which the
    # lockfile check inside the container turns into a plain error.
    docker run --rm \
        --user "$(id -u):$(id -g)" \
        --env HOME=/tmp \
        --volume "${SRC}:/src:ro" \
        --volume "${PAYLOAD}:/payload:rw" \
        "${NODE_IMAGE}" \
        sh -euc '[ -f /src/package-lock.json ] || { echo "the checkout did not propagate into the container (snap-confined docker?)" >&2; exit 1; }
                 cp -r /src /tmp/build && rm -rf /tmp/build/node_modules /tmp/build/dist
                 bash -c "$1" _ /tmp/build /payload/opt/postvec/server/ui "/payload/usr/share/doc/postvec-server/$2"' \
        _ "${BUILD}" dashboard-ThirdPartyNotices.txt \
        || die "the dashboard build failed in ${NODE_IMAGE}"
    node_used="${NODE_IMAGE}"
fi

[[ -f "${DEST}/index.html" ]] || die "the build produced no index.html under ${DEST}"
[[ -s "${DOC}/dashboard-ThirdPartyNotices.txt" ]] || die "no third-party notices were generated"

ui_version="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "${SRC}/package.json")"
cat > "${DOC}/dashboard-SOURCE.json" <<JSON
{
  "component": "postvec-server dashboard",
  "version": "${ui_version}",
  "source": "postvec-server/web-ui",
  "git_revision": "$(git -C "${REPO_ROOT}" rev-parse HEAD)",
  "lockfile_sha256": "$(sha256_of "${SRC}/package-lock.json")",
  "built_with": "${node_used}",
  "installed_root": "/opt/postvec/server/ui",
  "files": {
$(manifest_of_tree "${DEST}" | awk '{printf "    \"%s\": \"%s\",\n", $2, $1}' | sed '$ s/,$//')
  }
}
JSON

normalize_tree "${PAYLOAD}"

log "dashboard ${ui_version} payload ready"
du -sh "${DEST}" | sed 's/^/  /'
find "${DEST}" -type f | sort | sed 's|^|  |'

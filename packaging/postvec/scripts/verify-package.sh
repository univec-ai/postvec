#!/usr/bin/env bash
# Assert the properties a postvec package must have, from the package file
# alone — before it is ever installed anywhere.
#
#   verify-package.sh dist/common/debian12-amd64/*.deb \
#                     dist/noarch/debian12/*.deb \
#                     dist/extension/debian12-pg18-amd64/*.deb
#   verify-package.sh dist/common/el9-amd64/*.rpm dist/noarch/el9/*.rpm \
#                     dist/extension/el9-pg18-amd64/*.rpm
#   verify-package.sh --assert-model-layout   # stdin: a package file listing
#
# This checks *shape*: what is inside, who owns it, what it depends on, and —
# the important one — that no maintainer script does anything to a PostgreSQL
# cluster or a database. Installing files is the package's whole job; anything
# else belongs to `postvec setup`, run by an operator who chose to run it.
#
# Live install testing is separate; see tests/package-install-test.sh.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

(($#)) || die "usage: verify-package.sh <package> [package...]
       verify-package.sh --assert-model-layout   # stdin: a package file listing"

load_versions

fail=0
problem() { printf '    \033[1;31mFAIL\033[0m  %s\n' "$*" >&2; fail=1; }
pass()    { printf '    ok    %s\n' "$*"; }

# Anything a maintainer script must never do. Each pattern is a real incident
# class: restarting a cluster mid-transaction, mutating a database without
# consent, silently reconfiguring a server, or fetching code at install time.
FORBIDDEN=(
    'systemctl'          'service '            'pg_ctlcluster'      'pg_ctl'
    'pg_conftool'        'postgresql.conf'     'shared_preload'
    'CREATE EXTENSION'   'ALTER EXTENSION'     'DROP EXTENSION'
    'psql'               'createdb'            'dropdb'
    'postvec setup'      'postvec uninstall'
    'postvec model'      'postvec login'
    'curl'               'wget'                'rm -rf'
)

check_maintainer_scripts() {
    local scripts="$1" pattern code found=0
    if [[ ! -s "${scripts}" ]]; then
        pass "no maintainer scripts"
        return
    fi

    # Scan executable code only. A postrm that *prints* "run postvec uninstall
    # before removing the package" is exactly the guidance the operator needs;
    # a postrm that *runs* it is the defect. So comments and here-document
    # bodies are removed before matching — text is not an action.
    code="$(python3 - "${scripts}" <<'PY'
import re, sys

lines = open(sys.argv[1]).read().splitlines()
out, terminator = [], None
for line in lines:
    if terminator is not None:
        if line.strip() == terminator:
            terminator = None
        continue
    match = re.search(r"<<-?\s*['\"]?([A-Za-z_][A-Za-z0-9_]*)['\"]?", line)
    out.append(re.sub(r"#.*", "", line))
    if match:
        terminator = match.group(1)
print("\n".join(out))
PY
)"

    for pattern in "${FORBIDDEN[@]}"; do
        if printf '%s\n' "${code}" | grep -F "${pattern}" >/dev/null; then
            problem "maintainer script *runs* '${pattern}' — packages install files, nothing else"
            found=1
        fi
    done
    (( found )) || pass "maintainer scripts touch no cluster, database or network"
}

# ------------------------------------------------------------------- lintian

# The reviewed exceptions: lintian tags postvec accepts, each with its reason
# recorded next to it. Anything not in this file is a defect to fix, not a
# finding to live with.
LINTIAN_EXCEPTIONS="${PKG_DIR}/lintian-exceptions.txt"

read_lintian_exceptions() {
    sed 's/#.*//' "${LINTIAN_EXCEPTIONS}" | awk 'NF { print $1 }'
}

# Run lintian and decide the verdict here rather than delegating it to
# `--fail-on error`, for two reasons.
#
# `--suppress-tags` would make an accepted tag vanish from the output, so
# nobody reviewing a build could see that /opt is still being used and why. An
# exception the reader cannot see is indistinguishable from a check that was
# never run. Classifying the output instead keeps every accepted tag on screen,
# named and counted.
#
# And `--fail-on none` guarantees lintian exits 0 whenever it *ran*, so a
# non-zero exit means lintian itself failed — which is worth distinguishing
# from "lintian ran and found nothing".
lintian_gate() {
    local pkg="$1" output line tag status
    local -a exceptions=() accepted=() unexpected=()

    # A missing file is a broken checkout, not an empty list. Treating absence
    # as "nothing is excepted" makes the gate stricter, which sounds fail-safe
    # and is not: it reports three dozen errors that read like a regression in
    # the packages and says nothing about the file that is actually missing.
    # (This happened — the repository root ignores *.txt, so the list needs an
    # explicit negation in packaging/postvec/.gitignore to be committed at all.)
    #
    # Checked here rather than inside read_lintian_exceptions, because that runs
    # in a process substitution: `die` there would exit the subshell, print to
    # stderr, and let the caller carry on with an empty list — which is the
    # exact failure this check exists to replace.
    [[ -f "${LINTIAN_EXCEPTIONS}" ]] || die "no ${LINTIAN_EXCEPTIONS}
The reviewed list of lintian tags these packages may trip is missing, so every
tag on it would be reported as an unexpected error. Restore the file, and check
that .gitignore does not exclude it — that is how it goes missing in CI while
still being present on the machine that wrote it."
    mapfile -t exceptions < <(read_lintian_exceptions)

    output="$(lintian --fail-on none --tag-display-limit 0 "${pkg}" 2>&1)"
    status=$?
    if (( status != 0 )); then
        problem "lintian could not inspect the package (exit ${status})"
        printf '%s\n' "${output}" | sed 's/^/      /' >&2
        return
    fi

    while IFS= read -r line; do
        [[ "${line}" == E:* ]] || continue
        tag="$(awk '{print $3}' <<<"${line}")"
        if (( ${#exceptions[@]} )) && printf '%s\n' "${exceptions[@]}" | grep -qxF "${tag}"; then
            accepted+=("${tag}")
        else
            unexpected+=("${line}")
        fi
    done <<<"${output}"

    # Accepted tags are reported even when everything passes: a reviewed
    # divergence from Debian policy should be visible in every build log.
    if (( ${#accepted[@]} )); then
        local unique
        unique="$(printf '%s\n' "${accepted[@]}" | sort | uniq -c \
                  | awk '{printf "%s (%s)", $2, $1; print ""}' | paste -sd', ')"
        printf '    note  lintian: accepted by lintian-exceptions.txt: %s\n' "${unique}"
    fi

    if (( ${#unexpected[@]} )); then
        printf '%s\n' "${unexpected[@]}" | sed 's/^/      /' >&2
        problem "lintian reported ${#unexpected[@]} error(s) that lintian-exceptions.txt does not cover"
    else
        pass "lintian: no unexpected errors"
    fi
}

# ------------------------------------------------------------ shared assertions

# Directory entries of models/<backend> and models/<backend>/<name> are not a
# layout defect. dpkg lists them with a trailing slash and the original check
# skipped those; rpm -qpl lists them without one, so the same well-formed tree
# was reported as packaged outside models/<backend>/<name>/. A path that
# prefixes another listed path is a directory the package created for the
# files beneath it, not a stray file at that location.
#
# ${1} newline-separated package file list (dpkg or rpm spelling).
check_model_engine_paths() {
    local files="$1" path prefix other skip
    while IFS= read -r path; do
        [[ -n "${path}" ]] || continue
        [[ "${path}" == */ ]] && continue
        prefix="${path#./}"
        skip=0
        while IFS= read -r other; do
            other="${other#./}"
            if [[ "${other}" == "${prefix}/"* ]]; then
                skip=1
                break
            fi
        done <<<"${files}"
        (( skip )) && continue
        grep -qE '^\.?/opt/postvec/models/[^/]+/[^/]+/.+' <<<"${path}" \
            || problem "packaged outside models/<backend>/<name>/: ${path}"
    done <<<"$(grep '/opt/postvec/models/' <<<"${files}" || true)"
}

# Every path a package puts under /opt/postvec must be inside the one engine
# subtree it owns (`libs` for the runtime, `models` for a model bundle). This
# is what catches a payload in a superseded layout: the files are all present
# and correctly named, one directory level away from where the engine looks.
# ${1} newline-separated file list, ${2} the subtree (libs | models)
check_engine_root_paths() {
    local files="$1" subtree="$2" path stray=0
    while IFS= read -r path; do
        [[ -n "${path}" ]] || continue
        # dpkg lists `./opt/…` with trailing slashes on directories; rpm lists
        # `/opt/…` with neither. Normalise both to `opt/…`.
        path="${path#./}"
        path="${path#/}"
        case "${path}" in
        opt/|opt/postvec/|opt/postvec|"opt/postvec/${subtree}"|"opt/postvec/${subtree}/"*) ;;
        *)  problem "outside /opt/postvec/${subtree}/: /${path}"; stray=1 ;;
        esac
    done <<<"$(grep -E '^\.?/opt(/|$)' <<<"${files}" || true)"
    (( stray )) || pass "everything under /opt/postvec is inside ${subtree}/"
}

# ${1} package basename, ${2} newline-separated file list, ${3} dependency text
check_contents() {
    local name="$1" files="$2" depends="$3"

    # Debug packages first: their names begin with the names of the packages
    # they accompany, so a later case would never be reached and the CLI's
    # debug package would be asked to contain /usr/bin/postvec.
    case "${name}" in
    postvec-cli-dbgsym[-_]*|postvec-cli-debuginfo[-_]*|\
    postvec-server-dbgsym[-_]*|postvec-server-debuginfo[-_]*|\
    postgresql*postvec-dbgsym[-_]*|postgresql*postvec-debuginfo[-_]*)
        # Detached symbols only. It must contain no library, no SQL and no
        # binary, or it would conflict with the package it is supposed to
        # accompany.
        if grep -qE 'postvec\.so$|postvec--.*\.sql$|/usr/bin/postvec(-server)?$' <<<"${files}"; then
            problem "the debug package duplicates files from the package it accompanies"
        else
            pass "carries only detached debug information"
        fi
        # The mirrored path is the point: gdb and coredumpctl look for
        # /usr/lib/debug<original path>, so anywhere else is unreachable.
        grep -qE '/usr/lib/debug/usr/(bin|lib|pgsql)' <<<"${files}" \
            || problem "debug information is not at a mirrored /usr/lib/debug path"
        ;;
    postvec-cli_*|postvec-cli-*)
        # deb listings are ./-prefixed, rpm listings are not.
        grep -qE '^\.?/usr/bin/postvec$' <<<"${files}" \
            || problem "the CLI package does not contain /usr/bin/postvec"
        pass "owns /usr/bin/postvec"
        ;;
    postvec-server_*|postvec-server-*)
        # The inference node: one binary, the crate's unit, the packaged
        # drop-in that points it at /opt/postvec, and a conffile. It must not
        # carry the CLI (that is postvec-cli's file, and the two are
        # co-installable) and must not carry engine assets (a node that
        # bundled a model would put two owners on one path).
        grep -qE '^\.?/usr/bin/postvec-server$' <<<"${files}" \
            || problem "the node package does not contain /usr/bin/postvec-server"
        if grep -qE '^\.?/usr/bin/postvec$' <<<"${files}"; then
            problem "the node package contains /usr/bin/postvec — that is postvec-cli's file"
        fi
        grep -qE '/systemd/system/postvec-server\.service$' <<<"${files}" \
            || problem "no systemd unit"
        grep -qE '/systemd/system/postvec-server\.service\.d/packaged\.conf$' <<<"${files}" \
            || problem "no packaged drop-in pointing the unit at /opt/postvec"
        grep -qE '^\.?/etc/postvec-server/config\.json$' <<<"${files}" \
            || problem "no /etc/postvec-server/config.json"
        if grep -qE '/opt/postvec/(models|libs)/' <<<"${files}"; then
            problem "the node package carries engine assets — those belong to postvec-onnxruntime and postvec-model-*"
        fi
        # Nothing of the extension either: a node host has no PostgreSQL
        # major, and the extension's files have one owner per major.
        if grep -qE 'postvec\.so$|postvec--.*\.sql$|postvec\.control$' <<<"${files}"; then
            problem "the node package carries extension files (postvec.so / SQL / control)"
        fi
        pass "owns /usr/bin/postvec-server, its unit, the drop-in and its configuration"

        # The packaged config names the certificate pair beside itself: the
        # crate's <root>/certs default is under root-owned, read-only
        # /opt/postvec, which the service account can neither write nor read a
        # 0600 key in. A config that regressed to the relative default would
        # install fine and refuse to start on every host — so the file itself
        # is read (${pkg_config}, extracted by the caller), not only listed.
        if grep -qE '^\.?/etc/postvec-server$' <<<"${files}" \
            || grep -qE '^\.?/etc/postvec-server/$' <<<"${files}"; then
            pass "owns /etc/postvec-server, where the certificate pair goes"
        else
            problem "does not own /etc/postvec-server"
        fi
        if [[ -z "${pkg_config:-}" ]]; then
            problem "could not read the packaged /etc/postvec-server/config.json"
        elif python3 - "${pkg_config}" <<'PY'
import json, sys
config = json.load(open(sys.argv[1]))
ssl = config.get("ssl") or {}
if ssl.get("cert") != "/etc/postvec-server/server.crt" \
        or ssl.get("key") != "/etc/postvec-server/server.key":
    sys.exit("ssl names %r / %r" % (ssl.get("cert"), ssl.get("key")))
if config.get("insecure"):
    sys.exit("the packaged configuration must not disable TLS")
PY
        then
            pass "the packaged configuration names /etc/postvec-server/server.{crt,key}"
        else
            problem "the packaged configuration does not name the pair under /etc/postvec-server"
        fi

        # The relationship to the rest of the release, as metadata: a plain
        # repository install is a ready local node (extras: the pinned
        # runtime and model) with the CLI, both weak so that a provider-only
        # or slim node stays possible. See the description's comment.
        if grep -q "${EXTRAS_METAPACKAGE}" <<<"${recommends:-}" \
            && grep -q 'postvec-cli' <<<"${recommends:-}"; then
            pass "recommends ${EXTRAS_METAPACKAGE} and postvec-cli"
        else
            problem "Recommends should name ${EXTRAS_METAPACKAGE} and postvec-cli: ${recommends:-<none>}"
        fi
        if grep -qE 'postvec-cli|postvec-model|postvec-onnxruntime|postvec-extras' <<<"${depends}"; then
            problem "hard-depends on another postvec package: ${depends}"
        else
            pass "hard-depends on no other postvec package"
        fi

        # A node host has no PostgreSQL. The generated dependencies are
        # libraries; a `postgresql` anywhere in Depends is the CLI's
        # postgresql-common leaking in through a Depends that should have
        # been a Recommends.
        if grep -qi 'postgresql' <<<"${depends}"; then
            problem "depends on PostgreSQL packaging: ${depends}"
        else
            pass "depends on nothing from PostgreSQL"
        fi

        # The TLS listener links OpenSSL, which the ELF gate excused in the
        # bare base image on the strength of this very dependency. If the
        # generator did not produce it, the excuse was unearned.
        if grep -qiE 'libssl|openssl' <<<"${depends}"; then
            pass "declares the OpenSSL runtime its TLS listener links"
        else
            problem "does not declare an OpenSSL dependency (libssl3 / openssl-libs): ${depends:-<none>}"
        fi

        # The one package in the release under a different licence. The
        # metadata must say so, and must say what versions.env pinned: a
        # description that quietly inherited the neighbouring package's
        # `license:` line would misstate the terms on every host.
        [[ -n "${pkg_license:-}" ]] || problem "could not read the package's declared licence"
        if [[ "${pkg_license:-}" == "${SERVER_LICENSE}" ]]; then
            pass "declares licence ${SERVER_LICENSE} (SERVER_LICENSE)"
        else
            problem "declares licence '${pkg_license:-}', but SERVER_LICENSE is ${SERVER_LICENSE}"
        fi
        ;;
    postgresql*postvec[-_]*)
        # The reason the CLI is a separate package at all: PG 16 and PG 18
        # packages must be co-installable, which they cannot be if both ship
        # the same binary path.
        if grep -q '/usr/bin/postvec$' <<<"${files}"; then
            problem "an extension package contains /usr/bin/postvec — PG majors could not coexist"
        else
            pass "contains no CLI binary (PG majors stay co-installable)"
        fi
        # The node is its own package; a database host with two majors and a
        # node would otherwise see three owners of one path.
        if grep -qE '/usr/bin/postvec-server$|/systemd/system/postvec-server' <<<"${files}"; then
            problem "an extension package contains the inference node — that is postvec-server's"
        else
            pass "contains no inference node"
        fi
        grep -q 'postvec\.so$'      <<<"${files}" || problem "no postvec.so"
        grep -q 'postvec\.control$' <<<"${files}" || problem "no postvec.control"
        grep -q "postvec--${POSTVEC_VERSION}\.sql$" <<<"${files}" \
            || problem "no install script postvec--${POSTVEC_VERSION}.sql"
        pass "carries the library, the control file and the install SQL"

        grep -q 'pgvector' <<<"${depends}" || problem "does not depend on pgvector"
        grep -q 'postvec-cli' <<<"${depends}" || problem "does not depend on postvec-cli"
        pass "depends on pgvector and a compatible postvec-cli"
        ;;
    postvec-onnxruntime[-_]*)
        grep -q 'libonnxruntime\.so' <<<"${files}" || problem "no libonnxruntime.so"
        grep -q '/opt/postvec/libs/' <<<"${files}" \
            || problem "runtime is not under the engine root the extension searches"
        check_engine_root_paths "${files}" libs
        pass "installs ONNX Runtime under /opt/postvec/libs"
        ;;
    postvec-model-*)
        # The canonical root, and only the canonical root. A package built from
        # a payload in a former layout (/opt/postvec/ninference/…) has every
        # file the checks below look for, at paths nothing loads.
        check_engine_root_paths "${files}" models
        grep -q '/opt/postvec/models/' <<<"${files}" \
            || problem "model is not under the engine root the extension searches"
        grep -q 'ninference\.hub\.json$' <<<"${files}" || problem "no engine descriptor"
        grep -q 'onnx/model\.onnx$'      <<<"${files}" || problem "no ONNX graph"
        grep -q 'SOURCE\.json$'          <<<"${files}" || problem "no provenance (SOURCE.json)"
        grep -q 'model-files\.sha256$'   <<<"${files}" || problem "no per-file digests"
        if grep -qE 'model_(O[0-9]|qint8|quint8)' <<<"${files}"; then
            problem "packages upstream optimisation/quantisation variants that the descriptor does not use"
        fi
        pass "one graph, its assets, provenance and digests — no unused variants"

        # Everything under the engine's model root must sit inside exactly one
        # models/<backend>/<name>/ directory. A file one level too high is
        # loaded by nothing and is invisible to `model rm`'s ownership rules.
        check_model_engine_paths "${files}"

        # The pull's own receipt and its transaction directories are build
        # evidence, not package content: the receipt records `installed_at`, so
        # shipping it would make a clean rebuild produce different bytes, and a
        # `.staging` path in a package means a partial model was captured.
        if grep -qE '(^|/)(\.postvec-install\.json|\.postvec\.lock|\.staging|\.trash|\.swap)(/|$)' \
                <<<"${files}"; then
            problem "packages the CLI install receipt or a transaction path — \
the package owns these files, dpkg/rpm supplies their integrity, and the receipt's \
timestamps would make the package unreproducible"
        else
            pass "no install receipt, lock or staging path"
        fi
        ;;
    postvec-extras[-_]*|"${EXTRAS_METAPACKAGE}"[-_]*)
        # A metapackage that grew files is a metapackage that will conflict
        # with the packages it is supposed to pull in.
        if grep -qvE '(/usr/share/(doc|licenses)/|/$)' <<<"${files}"; then
            problem "the metapackage contains files beyond its documentation"
        else
            pass "carries no payload of its own"
        fi
        grep -q 'postvec-onnxruntime' <<<"${depends}" || problem "does not depend on the runtime"
        grep -q 'postvec-model'       <<<"${depends}" || problem "does not depend on a model bundle"
        pass "pins the exact reviewed engine assets"
        ;;
    esac

    # Any package containing a shared object or an executable must declare the
    # libraries it needs. nFPM runs no dependency generator of its own, so this
    # is the check that catches a build where `scripts/elf-depends.sh` did not
    # run: the package would install cleanly and then fail to load.
    #
    # Detached debug objects are exempt: nothing loads them, and they depend on
    # the package whose symbols they carry rather than on any library.
    case "${name}" in
    *-dbgsym[-_]*|*-debuginfo[-_]*) return ;;
    esac
    if grep -qE '\.so($|\.)|/usr/bin/' <<<"${files}"; then
        if grep -qE 'libc6|libc\.so|glibc' <<<"${depends}"; then
            pass "declares generated ELF dependencies"
        else
            problem "contains binaries but declares no libc dependency: ${depends:-<none>}"
        fi
    fi
}

# The registry URL overrides must never be compiled into a shipped CLI. The
# override env-var names exist only in a `registry-test-overrides` build
# (postvec-cli/src/registry/urls.rs), so their absence from the binary's
# bytes proves the feature is off in this artifact.
check_no_registry_overrides() {
    local binary="$1" # an extracted /usr/bin/postvec
    if grep -q 'POSTVEC_REGISTRY_PUBLIC_INDEX_URL' "${binary}"; then
        problem "the CLI binary compiles in the registry-test-overrides env hooks"
    else
        pass "registry-test-overrides is not compiled in"
    fi
}

verify_deb() {
    local pkg="$1" name; name="$(basename "${pkg}")"
    need dpkg-deb
    printf '\n\033[1m%s\033[0m\n' "${name}"

    local info files depends owners scripts tmp
    tmp="$(mktemp -d)"; trap 'rm -rf "${tmp}"' RETURN
    info="$(dpkg-deb --info "${pkg}")"
    files="$(dpkg-deb --contents "${pkg}" | awk '{print $6}')"
    depends="$(sed -n 's/^ *Depends: //p' <<<"${info}")"
    recommends="$(sed -n 's/^ *Recommends: //p' <<<"${info}")"
    # The node's packaged configuration, for the checks that read it.
    pkg_config=""
    if [[ "${name}" == postvec-server_* ]]; then
        dpkg-deb --fsys-tarfile "${pkg}" \
            | tar -x -C "${tmp}" ./etc/postvec-server/config.json 2>/dev/null || true
        [[ -f "${tmp}/etc/postvec-server/config.json" ]] \
            && pkg_config="${tmp}/etc/postvec-server/config.json"
    fi
    owners="$(dpkg-deb --contents "${pkg}" | awk '{print $2}' | sort -u)"
    # The licence a package declares. Debian has no control field for it —
    # nfpm writes its `license:` into the copyright file — so it is read from
    # /usr/share/doc/<pkg>/copyright, which for this project's packages is
    # either the DEP-5 rendering (a `License:` line) or the verbatim text.
    pkg_license="$(dpkg-deb --fsys-tarfile "${pkg}" \
        | tar -xO --wildcards './usr/share/doc/*/copyright' 2>/dev/null \
        | sed -n 's/^License: //p' | head -1 || true)"

    # `0/0` and `root/root` are the same owner: dpkg-deb prints the numeric form
    # when the tar entry carries no user name, which is how nfpm writes a file
    # it generated itself rather than copied. Both are root; nothing else is.
    if [[ -z "$(grep -vE '^(root/root|0/0)$' <<<"${owners}")" ]]; then
        pass "all files are root-owned"
    else
        problem "non-root ownership: ${owners//$'\n'/, }"
    fi

    # Debian policy: /usr/share/doc/<package>/copyright must exist.
    grep -q "/usr/share/doc/[^/]*/copyright$" <<<"${files}" \
        && pass "ships a copyright file" \
        || problem "no /usr/share/doc/<package>/copyright"

    # Debian Policy §12.7. Asserted here as well as by lintian below, because
    # lintian only runs where it is installed and a package with no changelog
    # should fail on every host — including the developer machine that built it.
    grep -q "/usr/share/doc/[^/]*/changelog\.Debian\.gz$" <<<"${files}" \
        && pass "ships a changelog" \
        || problem "no /usr/share/doc/<package>/changelog.Debian.gz"

    dpkg-deb --ctrl-tarfile "${pkg}" | tar -x -C "${tmp}" 2>/dev/null || true
    : > "${tmp}/all-scripts"
    for s in preinst postinst prerm postrm; do
        [[ -f "${tmp}/${s}" ]] && cat "${tmp}/${s}" >> "${tmp}/all-scripts"
    done
    check_maintainer_scripts "${tmp}/all-scripts"

    check_contents "${name}" "${files}" "${depends}"

    if [[ "${name}" == postvec-cli_* ]]; then
        dpkg-deb --fsys-tarfile "${pkg}" | tar -x -C "${tmp}" ./usr/bin/postvec 2>/dev/null || true
        if [[ -f "${tmp}/usr/bin/postvec" ]]; then
            check_no_registry_overrides "${tmp}/usr/bin/postvec"
        else
            problem "could not extract /usr/bin/postvec for the override check"
        fi
    fi

    if command -v lintian >/dev/null 2>&1; then
        lintian_gate "${pkg}"
    else
        printf '    skip  lintian is not installed\n'
    fi
}

# `rpm` is not installed on a Debian/Ubuntu release runner, and requiring it
# would mean the RPM packages are only ever verified on an RPM host — which is
# exactly when nobody looks. Fall back to a container of the target
# distribution, which is also the more faithful inspector.
# Takes rpm's flags and then the package *basename*; the directory is mounted
# at a fixed path. (Rewriting an absolute path inside the argument list with
# bash pattern substitution looks tidier and is not: the pattern itself
# contains slashes, which is exactly where that syntax stops being readable.)
rpm_query() {
    local file="${*: -1}" flags=("${@:1:$#-1}")
    if [[ -n "${RPM_HOST_TOOL}" ]]; then
        "${RPM_HOST_TOOL}" "${flags[@]}" "${RPM_QUERY_DIR}/${file}"
    else
        timeout 120 docker run --rm --volume "${RPM_QUERY_DIR}:/pkgs:ro" \
            "${DIST_BASE_IMAGE:-almalinux:9@${BUILD_BASE_EL9_DIGEST}}" \
            rpm "${flags[@]}" "/pkgs/${file}"
    fi
}

verify_rpm() {
    local pkg="$1" name; name="$(basename "${pkg}")"
    printf '\n\033[1m%s\033[0m\n' "${name}"

    RPM_HOST_TOOL="$(type -P rpm || true)"
    RPM_QUERY_DIR="$(cd "$(dirname "${pkg}")" && pwd)"
    if [[ -z "${RPM_HOST_TOOL}" ]]; then
        need docker
        printf '    note  rpm is not installed; inspecting in a container\n'
    fi

    local files depends scripts
    files="$(rpm_query -qpl "${name}")"
    depends="$(rpm_query -qpR "${name}")"
    scripts="$(rpm_query -qp --scripts "${name}")"
    # RPM records the licence as a header field.
    pkg_license="$(rpm_query -qp --qf '%{LICENSE}' "${name}" 2>/dev/null || true)"
    recommends="$(rpm_query -qp --recommends "${name}" 2>/dev/null || true)"
    [[ -n "${files}" ]] || problem "could not read the package contents"
    # The node's packaged configuration, for the checks that read it.
    pkg_config=""
    if [[ "${name}" == postvec-server-[0-9]* ]]; then
        if command -v rpm2cpio >/dev/null 2>&1 && command -v cpio >/dev/null 2>&1; then
            local cdir; cdir="$(mktemp -d)"
            (cd "${cdir}" && rpm2cpio "${RPM_QUERY_DIR}/${name}" \
                | cpio -id --quiet --no-absolute-filenames) || true
            [[ -f "${cdir}/etc/postvec-server/config.json" ]] \
                && pkg_config="${cdir}/etc/postvec-server/config.json"
        else
            # No rpm2cpio on the host: read the file out of a container of
            # the target, the same way the metadata is read. The base image
            # has rpm2cpio but no cpio, so the (newc) archive is walked in
            # Python, which the image does have.
            local cfile; cfile="$(mktemp)"
            timeout 120 docker run --rm --volume "${RPM_QUERY_DIR}:/pkgs:ro" \
                "${DIST_BASE_IMAGE:-almalinux:9@${BUILD_BASE_EL9_DIGEST}}" \
                bash -c 'rpm2cpio "/pkgs/$1" | python3 -c "
import sys
data = sys.stdin.buffer.read()
pos = 0
while True:
    assert data[pos:pos+6] == b\"070701\", \"not a newc cpio archive\"
    fields = [int(data[pos+6+i*8:pos+14+i*8], 16) for i in range(13)]
    namesize, filesize = fields[11], fields[6]
    name = data[pos+110:pos+110+namesize-1].decode()
    start = pos + 110 + namesize
    start += (-start) % 4
    if name == \"TRAILER!!!\":
        break
    if name.lstrip(\"./\") == \"etc/postvec-server/config.json\":
        sys.stdout.buffer.write(data[start:start+filesize])
        break
    pos = start + filesize
    pos += (-pos) % 4
"' _ "${name}" > "${cfile}" 2>/dev/null || true
            [[ -s "${cfile}" ]] && pkg_config="${cfile}"
        fi
    fi

    printf '%s\n' "${scripts}" > /tmp/postvec-rpm-scripts.$$
    check_maintainer_scripts /tmp/postvec-rpm-scripts.$$
    rm -f /tmp/postvec-rpm-scripts.$$

    # Note: nFPM does *not* run RPM's automatic dependency generator — it
    # writes exactly the Requires it is given. The generated ones therefore
    # have to be present because scripts/elf-depends.sh put them there, which
    # `check_contents` asserts for every package carrying a binary.
    check_contents "${name}" "${files}" "${depends}"

    # `postvec-cli-[0-9]*` and not `postvec-cli-*`: the version field is what
    # separates the CLI package from postvec-cli-debuginfo, which carries no
    # binary by design — the check above asserts exactly that — and so can only
    # ever fail a check for one.
    if [[ "${name}" == postvec-cli-[0-9]* ]]; then
        if command -v rpm2cpio >/dev/null 2>&1 && command -v cpio >/dev/null 2>&1; then
            local xdir; xdir="$(mktemp -d)"
            # No member pattern, and --no-absolute-filenames, for two reasons.
            # nFPM writes payload member names absolute (`/usr/bin/postvec`)
            # where rpmbuild writes them relative (`./usr/bin/postvec`), and
            # cpio matches the stored spelling literally: a pattern for either
            # convention extracts nothing from the other, silently and with a
            # zero exit. And cpio's copy-in mode honours an absolute member
            # name by default, so a *matching* pattern would write through to
            # the host's real /usr/bin/postvec rather than into ${xdir} —
            # extracting the whole payload under --no-absolute-filenames is
            # both convention-independent and confined to the temporary tree.
            (cd "${xdir}" && rpm2cpio "${RPM_QUERY_DIR}/${name}" \
                | cpio -id --quiet --no-absolute-filenames) || true
            if [[ -f "${xdir}/usr/bin/postvec" ]]; then
                check_no_registry_overrides "${xdir}/usr/bin/postvec"
            else
                problem "could not extract /usr/bin/postvec for the override check"
            fi
            rm -rf "${xdir}"
        else
            printf '    skip  rpm2cpio and cpio are not both installed; the override check ran on the deb\n'
        fi
    fi

    if command -v rpmlint >/dev/null 2>&1; then
        rpmlint -i "${pkg}" || printf '    note  rpmlint reported findings (see above)\n'
    else
        printf '    skip  rpmlint is not installed\n'
    fi
    unset RPM_HOST_TOOL RPM_QUERY_DIR
}

# Fixture entry for tests/unit-test.sh: the layout check against a file listing
# rather than a package, so the rpm-vs-dpkg directory spelling can fail in
# seconds and without Docker.
if [[ "${1:-}" == --assert-model-layout ]]; then
    (($# == 1)) || die "usage: verify-package.sh --assert-model-layout"
    check_model_engine_paths "$(cat)"
    if (( fail )); then die "package verification failed"; fi
    exit 0
fi

for pkg in "$@"; do
    [[ -f "${pkg}" ]] || die "no such package: ${pkg}"
    case "${pkg}" in
    *.deb) verify_deb "${pkg}" ;;
    *.rpm) verify_rpm "${pkg}" ;;
    *)     die "not a package: ${pkg}" ;;
    esac
done

echo
if (( fail )); then die "package verification failed"; fi
log "package verification passed ($# package(s))"

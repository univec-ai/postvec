#!/usr/bin/env bash
# Configure the package repositories postvec's packages need.
# It does not install postvec, create clusters or edit PostgreSQL.
#
#   gh attestation verify postvec-prerequisites.sh --repo <repo>
#   less postvec-prerequisites.sh
#
#   sudo bash ./postvec-prerequisites.sh --pg 18
#   sudo bash ./postvec-prerequisites.sh --pg 18 --yes
#   bash ./postvec-prerequisites.sh --pg 18 --print
#
# Use `bash ./...`: a GitHub release asset drops the executable bit.
# On a distribution this does not test, it prints the commands and stops.
# `--force-untested` runs them anyway.
#
# Debian and Ubuntu: the PGDG signing key in /usr/share/keyrings
# (downloaded, fingerprint-checked and rechecked if a keyring is already
# there), one apt source and apt update.
#
# RHEL family: CodeReady Builder (`crb` on the rebuilds,
# `codeready-builder-for-rhel-9-<arch>-rpms` through subscription-manager
# on RHEL); EPEL (`epel-release` on the rebuilds, plus `epel-next-release`
# on CentOS Stream, Fedora's release RPM by URL on RHEL); the PGDG
# repository RPM, signature-checked against the fingerprint-verified key;
# and `module disable postgresql`, which otherwise shadows every PGDG
# package.
#
# Tested on Debian 12, Ubuntu 22.04, Ubuntu 24.04, AlmaLinux 9 and Rocky 9.
# CentOS Stream 9 and subscribed RHEL 9 are generated but not executed
# unless `--force-untested` is set.
#
# Idempotent: a second run changes nothing.

set -Eeuo pipefail

# Fingerprints are the trust root. TLS only proves which host answered.
# Kept identical to the PGDG fingerprint pins in versions.env.
PGDG_KEY_FINGERPRINT=B97B0AFCAA1A47F044F244A07FCC7D46ACCC4CF8
PGDG_KEY_URL=https://www.postgresql.org/media/keys/ACCC4CF8.asc
PGDG_RPM_KEY_FINGERPRINT=D4BF08AE67A0B4C7A1DBCCD240BCA2B408B40D20
PGDG_RPM_KEY_URL=https://download.postgresql.org/pub/repos/yum/keys/PGDG-RPM-GPG-KEY-RHEL
# aarch64 has its own key and file. Select by machine, same as the repository RPM URL.
PGDG_RPM_KEY_AARCH64_FINGERPRINT=B031F89FC983E98262906B6E177B343BB9738825
PGDG_RPM_KEY_AARCH64_URL=https://download.postgresql.org/pub/repos/yum/keys/PGDG-RPM-GPG-KEY-AARCH64-RHEL
KEYRING=/usr/share/keyrings/postgresql-archive-keyring.gpg
SOURCES=/etc/apt/sources.list.d/pgdg.list

# Overridable so tests can cover every distribution. It changes what is read, never what is written.
OS_RELEASE="${POSTVEC_OS_RELEASE:-/etc/os-release}"

# Overridable: the signing key, the CRB name and the repository RPM URL vary by machine.
ARCH="${POSTVEC_UNAME_M:-$(uname -m)}"
if [[ "${ARCH}" == aarch64 ]]; then
    PGDG_RPM_KEY_FINGERPRINT="${PGDG_RPM_KEY_AARCH64_FINGERPRINT}"
    PGDG_RPM_KEY_URL="${PGDG_RPM_KEY_AARCH64_URL}"
fi

PG_MAJOR=""
ASSUME_YES=0
PRINT_ONLY=0
FORCE_UNTESTED=0

usage() { sed -n '2,32p' "$0"; }

while (($#)); do
    case "$1" in
    --pg)      PG_MAJOR="$2"; shift 2 ;;
    --yes|-y)  ASSUME_YES=1; shift ;;
    --print|--dry-run) PRINT_ONLY=1; shift ;;
    # Run the documented commands on a distribution postvec does not test.
    --force-untested)  FORCE_UNTESTED=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'unknown argument: %s\n\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
done

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*" >&2; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

[[ -r "${OS_RELEASE}" ]] || die "cannot read ${OS_RELEASE}; this is not a supported host"
# shellcheck disable=SC1090,SC1091
. "${OS_RELEASE}"

# FAMILY picks the package manager. CRB_METHOD is separate: EL9 rebuilds use
# `crb` via dnf, subscribed RHEL uses subscription-manager.
FAMILY=""; CRB_METHOD=""; EPEL_METHOD=""; TESTED=0
case "${ID}:${VERSION_ID%%.*}" in
debian:12)           FAMILY=deb; TESTED=1 ;;
ubuntu:22|ubuntu:24) FAMILY=deb; TESTED=1 ;;
almalinux:9|rocky:9) FAMILY=rpm; CRB_METHOD=dnf; EPEL_METHOD=package; TESTED=1 ;;
centos:9)            FAMILY=rpm; CRB_METHOD=dnf; EPEL_METHOD=package-with-next ;;
rhel:9)              FAMILY=rpm; CRB_METHOD=subscription-manager; EPEL_METHOD=url ;;
*)
    # Not a refusal. PGDG covers more than postvec tests.
    case "${ID_LIKE:-}" in
    *debian*)        FAMILY=deb ;;
    *rhel*|*fedora*) FAMILY=rpm; CRB_METHOD=dnf; EPEL_METHOD=package ;;
    esac
    [[ -n "${FAMILY}" ]] || die "unsupported distribution: ${PRETTY_NAME:-${ID} ${VERSION_ID}}
postvec publishes packages for Debian 12, Ubuntu 22.04, Ubuntu 24.04 and EL9."
    warn "${PRETTY_NAME:-${ID} ${VERSION_ID}} is not one of postvec's tested targets"
    warn "  treating it as the ${FAMILY} family; PGDG may or may not support it"
    ;;
esac

if [[ -n "${PG_MAJOR}" && ! "${PG_MAJOR}" =~ ^(16|17|18)$ ]]; then
    die "--pg must be 16, 17 or 18 (got '${PG_MAJOR}')"
fi

# Build the plan as text and show it before anything runs. `--print` pastes this.

plan_deb() {
    local codename="${VERSION_CODENAME:-}"
    [[ -n "${codename}" ]] || die "${OS_RELEASE} has no VERSION_CODENAME"
    cat <<EOF
# 1. tools this needs
apt-get update
apt-get install -y --no-install-recommends ca-certificates curl gnupg

# 2. the PGDG signing key, verified against its published fingerprint
#    (${PGDG_KEY_FINGERPRINT})
curl -fsSL ${PGDG_KEY_URL} -o /tmp/pgdg.asc
fprs="\$(gpg --show-keys --with-colons /tmp/pgdg.asc | awk -F: '\$1 == "fpr" {print \$10}')"
grep -qxF ${PGDG_KEY_FINGERPRINT} <<<"\$fprs" || { echo "wrong key"; exit 1; }
gpg --dearmor -o ${KEYRING} /tmp/pgdg.asc

# 3. the PGDG archive for ${codename}
echo "deb [signed-by=${KEYRING}] https://apt.postgresql.org/pub/repos/apt ${codename}-pgdg main" \\
  > ${SOURCES}
apt-get update
EOF
}

# CodeReady Builder: `crb` on EL9 rebuilds, subscription-manager on RHEL.
plan_crb() {
    case "${CRB_METHOD}" in
    subscription-manager)
        printf 'subscription-manager repos --enable "codeready-builder-for-rhel-9-%s-rpms"\n' \
            "${ARCH}" ;;
    *)
        printf 'dnf config-manager --set-enabled crb\n' ;;
    esac
}

# EPEL: the package on rebuilds, a URL on RHEL, plus epel-next-release on CentOS Stream.
EPEL_RELEASE_URL=https://dl.fedoraproject.org/pub/epel/epel-release-latest-9.noarch.rpm
plan_epel() {
    case "${EPEL_METHOD}" in
    url)               printf 'dnf install -y %s\n' "${EPEL_RELEASE_URL}" ;;
    package-with-next) printf 'dnf install -y epel-release epel-next-release\n' ;;
    *)                 printf 'dnf install -y epel-release\n' ;;
    esac
}

plan_rpm() {
    local arch; arch="${ARCH}"
    cat <<EOF
# 1. tools this needs
dnf install -y dnf-plugins-core

# 2. PGDG's EL9 packages depend on CodeReady Builder and on EPEL.
#    CRB first: on RHEL, EPEL's own instructions require it.
$(plan_crb)
$(plan_epel)

# 3. the PGDG RPM signing key, verified against its published fingerprint
#    (${PGDG_RPM_KEY_FINGERPRINT})
curl -fsSL ${PGDG_RPM_KEY_URL} -o /tmp/pgdg-rpm.key
fprs="\$(gpg --show-keys --with-colons /tmp/pgdg-rpm.key | awk -F: '\$1 == "fpr" {print \$10}')"
grep -qxF ${PGDG_RPM_KEY_FINGERPRINT} <<<"\$fprs" || { echo "wrong key"; exit 1; }
# 4. the PGDG repository RPM, downloaded and signature-checked against that one
#    key before install. The URL is '-latest' by upstream's design, so its
#    digest is not a pin — the key is. The check runs in a throwaway RPM
#    database holding only this key, so it proves *this* signer and not merely
#    'some key the host already trusts'.
curl -fsSL https://download.postgresql.org/pub/repos/yum/reporpms/EL-9-${arch}/pgdg-redhat-repo-latest.noarch.rpm \\
  -o /tmp/pgdg-repo.rpm
mkdir -p /tmp/pgdg-checkdb
rpm --dbpath /tmp/pgdg-checkdb --initdb
rpm --dbpath /tmp/pgdg-checkdb --import /tmp/pgdg-rpm.key
rpm --dbpath /tmp/pgdg-checkdb --checksig /tmp/pgdg-repo.rpm
rpm --import /tmp/pgdg-rpm.key
dnf install -y /tmp/pgdg-repo.rpm

# 5. the distribution's own module would otherwise shadow every PGDG package
dnf -y module disable postgresql
EOF
}

case "${FAMILY}" in
deb) PLAN="$(plan_deb)" ;;
rpm) PLAN="$(plan_rpm)" ;;
esac

if (( PRINT_ONLY )); then
    printf '%s\n' "${PLAN}"
    exit 0
fi

printf '\n\033[1mpostvec prerequisites — %s\033[0m\n\n' "${PRETTY_NAME:-${ID} ${VERSION_ID}}" >&2
printf '%s\n' "${PLAN}" | sed 's/^/    /' >&2
printf '\n' >&2

# An untested distribution is printed, not executed, unless --force-untested.
if (( ! TESTED )) && (( ! FORCE_UNTESTED )); then
    die "postvec has not rehearsed this bootstrap on ${PRETTY_NAME:-${ID} ${VERSION_ID}}.

CI executes it on Debian 12, Ubuntu 22.04, Ubuntu 24.04, AlmaLinux 9 and
Rocky 9. The commands printed above are the documented ones for this
distribution — run them yourself, or rerun with --force-untested to have this
script run them.

Nothing was changed."
fi

[[ "$(id -u)" == 0 ]] || die "this must run as root (it configures system package repositories)"

if (( ! ASSUME_YES )); then
    if [[ ! -t 0 ]]; then
        die "not a terminal and --yes was not given; nothing was changed.
Run with --yes to proceed unattended, or --print to see the commands only."
    fi
    read -r -p "Configure these repositories? [y/N] " reply
    case "${reply}" in [yY]|[yY][eE][sS]) ;; *) die "nothing was changed" ;; esac
fi

# Capture fingerprints. grep -q on a pipe under pipefail dies with SIGPIPE
# on a match, so the natural pipeline fails on success.
fingerprints_of() {
    gpg --show-keys --with-colons "$1" 2>/dev/null | awk -F: '$1 == "fpr" {print $10}' || true
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1
$2"
}

require_fingerprint() {
    local file="$1" want="$2" what="$3" found
    found="$(fingerprints_of "${file}")"
    grep -qxF "${want}" <<<"${found}" || die "${what} is not ${want}.
It offered: ${found:-<none>}
Nothing was installed. Do not proceed until you know why."
}

configure_deb() {
    export DEBIAN_FRONTEND=noninteractive
    say "installing ca-certificates, curl and gnupg"
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends ca-certificates curl gnupg >/dev/null

    # Check the fingerprint of a new download or a keyring already on disk.
    local tmp; tmp="$(mktemp -d)"
    trap 'rm -rf "${tmp}"' RETURN

    if [[ -s "${KEYRING}" ]]; then
        if fingerprints_of "${KEYRING}" | grep -qxF "${PGDG_KEY_FINGERPRINT}"; then
            say "${KEYRING} is already in place and is ${PGDG_KEY_FINGERPRINT}"
        else
            die "${KEYRING} already exists and is not the PGDG archive key.
It contains: $(fingerprints_of "${KEYRING}" | tr '\n' ' ')
Expected:    ${PGDG_KEY_FINGERPRINT}
Nothing was changed. Remove or investigate that file before rerunning."
        fi
    else
        say "fetching the PGDG signing key"
        curl -fsSL --retry 3 --retry-delay 2 -o "${tmp}/pgdg.asc" "${PGDG_KEY_URL}" \
            || die "could not download ${PGDG_KEY_URL}"
        require_fingerprint "${tmp}/pgdg.asc" "${PGDG_KEY_FINGERPRINT}" "the PGDG archive key"
        say "key fingerprint verified: ${PGDG_KEY_FINGERPRINT}"
        gpg --dearmor -o "${KEYRING}" "${tmp}/pgdg.asc"
        chmod 0644 "${KEYRING}"
    fi

    local line="deb [signed-by=${KEYRING}] https://apt.postgresql.org/pub/repos/apt ${VERSION_CODENAME}-pgdg main"
    if [[ -f "${SOURCES}" ]] && grep -qxF "${line}" "${SOURCES}"; then
        say "${SOURCES} already names ${VERSION_CODENAME}-pgdg"
    else
        say "writing ${SOURCES}"
        printf '%s\n' "${line}" > "${SOURCES}"
        chmod 0644 "${SOURCES}"
    fi

    say "apt-get update"
    apt-get update -qq
}

configure_rpm() {
    local tmp; tmp="$(mktemp -d)"
    trap 'rm -rf "${tmp}"' RETURN

    say "installing dnf-plugins-core"
    dnf install -y -q dnf-plugins-core >/dev/null

    # CodeReady Builder before EPEL. On RHEL the EPEL RPM install requires it.
    case "${CRB_METHOD}" in
    subscription-manager)
        say "enabling codeready-builder-for-rhel-9-${ARCH}-rpms"
        need_cmd subscription-manager \
            "this is RHEL, where CodeReady Builder is enabled through subscription-manager"
        subscription-manager repos --enable "codeready-builder-for-rhel-9-${ARCH}-rpms" \
            || die "could not enable CodeReady Builder. On RHEL this needs an active
subscription; on a rebuild such as AlmaLinux or Rocky the repository is called
'crb' and this script would have used dnf instead." ;;
    *)
        say "enabling crb"
        dnf config-manager --set-enabled crb >/dev/null ;;
    esac

    # Rebuilds ship epel-release. Subscribed RHEL needs Fedora's release RPM by URL.
    case "${EPEL_METHOD}" in
    url)
        say "installing EPEL from ${EPEL_RELEASE_URL}"
        dnf install -y -q "${EPEL_RELEASE_URL}" >/dev/null ;;
    package-with-next)
        say "installing epel-release and epel-next-release"
        dnf install -y -q epel-release epel-next-release >/dev/null ;;
    *)
        say "installing epel-release"
        dnf install -y -q epel-release >/dev/null ;;
    esac

    local repos
    repos="$(dnf repolist --enabled 2>/dev/null)" || repos=""
    if grep -q '^pgdg' <<<"${repos}"; then
        say "the PGDG repository is already configured"
    else
        # The repository URL is -latest and moves. The key is the trust root:
        # check its fingerprint, then the RPM signature, before install.
        say "fetching the PGDG RPM signing key"
        curl -fsSL --retry 3 --retry-delay 2 -o "${tmp}/pgdg-rpm.key" "${PGDG_RPM_KEY_URL}" \
            || die "could not download ${PGDG_RPM_KEY_URL}"
        require_fingerprint "${tmp}/pgdg-rpm.key" "${PGDG_RPM_KEY_FINGERPRINT}" \
            "the PGDG RPM signing key"
        say "key fingerprint verified: ${PGDG_RPM_KEY_FINGERPRINT}"

        say "fetching the PGDG repository RPM"
        curl -fsSL --retry 3 --retry-delay 2 -o "${tmp}/pgdg-repo.rpm" \
            "https://download.postgresql.org/pub/repos/yum/reporpms/EL-9-${ARCH}/pgdg-redhat-repo-latest.noarch.rpm" \
            || die "could not download the PGDG repository RPM"

        # Check the signature in a throwaway RPM database that holds only this
        # key. The host database would accept any key it already trusts.
        local checkdb="${tmp}/rpmdb"
        mkdir -p "${checkdb}"
        rpm --dbpath "${checkdb}" --initdb
        rpm --dbpath "${checkdb}" --import "${tmp}/pgdg-rpm.key"
        rpm --dbpath "${checkdb}" --checksig "${tmp}/pgdg-repo.rpm" \
            || die "the PGDG repository RPM is not signed by ${PGDG_RPM_KEY_FINGERPRINT}.
Nothing was installed. Do not proceed until you know why."
        say "repository RPM signature verified against ${PGDG_RPM_KEY_FINGERPRINT} alone"

        # Only now into the host database, so dnf can verify the packages the
        # repository goes on to serve.
        rpm --import "${tmp}/pgdg-rpm.key"
        dnf install -y -q "${tmp}/pgdg-repo.rpm" >/dev/null
    fi

    say "disabling the distribution's postgresql module"
    dnf -qy module disable postgresql >/dev/null 2>&1 || true
}

case "${FAMILY}" in
deb) configure_deb ;;
rpm) configure_rpm ;;
esac

# Confirm the packages resolve. A command that exited 0 is not the same thing.

check_available() {
    local major="$1" name found=1 policy
    case "${FAMILY}" in
    deb)
        for name in "postgresql-${major}" "postgresql-${major}-pgvector"; do
            # Captured, not piped into grep -q: pipefail plus SIGPIPE fails on a match.
            policy="$(apt-cache policy "${name}" 2>/dev/null)" || policy=""
            if [[ "${policy}" == *"Candidate:"* && "${policy}" != *"Candidate: (none)"* ]]; then
                say "  ${name} is available"
            else
                warn "  ${name} is NOT available"
                found=0
            fi
        done ;;
    rpm)
        for name in "postgresql${major}-server" "pgvector_${major}"; do
            if dnf --quiet list --available "${name}" >/dev/null 2>&1 \
               || dnf --quiet list --installed "${name}" >/dev/null 2>&1; then
                say "  ${name} is available"
            else
                warn "  ${name} is NOT available"
                found=0
            fi
        done ;;
    esac
    return $(( found ? 0 : 1 ))
}

if [[ -n "${PG_MAJOR}" ]]; then
    say "checking that PostgreSQL ${PG_MAJOR} and pgvector resolve"
    check_available "${PG_MAJOR}" || die "the repositories are configured but PostgreSQL ${PG_MAJOR}
does not resolve. PGDG may not carry that major for this distribution."
    printf '\n\033[1;32mready\033[0m — nothing is installed yet. Install the packages you downloaded;\nPostgreSQL %s and pgvector come in as their dependencies:\n\n' "${PG_MAJOR}" >&2
    case "${FAMILY}" in
    deb) printf '    sudo apt install ./postvec-cli_*.deb ./postgresql-%s-postvec_*.deb\n\n' "${PG_MAJOR}" >&2 ;;
    rpm) printf '    sudo dnf install ./postvec-cli-*.rpm ./postgresql%s-postvec-*.rpm\n\n' "${PG_MAJOR}" >&2 ;;
    esac
else
    printf '\n\033[1;32mready\033[0m — rerun with --pg <major> to confirm that major resolves.\n\n' >&2
fi

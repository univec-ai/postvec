#!/usr/bin/env bash
# Configure the package repositories postvec's packages need, and nothing else.
#
#   gh attestation verify postvec-prerequisites.sh --repo <repo>   # first
#   less postvec-prerequisites.sh                                  # then read it
#
#   sudo bash ./postvec-prerequisites.sh --pg 18          # do it, after asking
#   sudo bash ./postvec-prerequisites.sh --pg 18 --yes    # do it, unattended
#        bash ./postvec-prerequisites.sh --pg 18 --print  # show the commands only
#
# On a distribution postvec does not test, it prints the commands and refuses to
# run them; `--force-untested` overrides that deliberately.
#
# `bash ./…`, not `./…`: a GitHub release asset does not keep its executable
# bit, so a freshly downloaded copy is not directly runnable.
#
# ---------------------------------------------------------------------------
#
# **Why this exists.** postvec's packages name an exact PostgreSQL major —
# `postgresql-18-postvec` depends on `postgresql-18` and
# `postgresql-18-pgvector` — and no supported distribution ships that
# combination in its own archives at the versions required. Debian 12 carries
# PostgreSQL 15; Ubuntu 22.04 carries 14; EL9 carries 13 in a module that
# shadows anything else; pgvector 0.8 is not in any of them for every major.
# So `apt install ./postgresql-18-postvec_*.deb` on an untouched host fails at
# dependency resolution, and that is a property of the platform, not of these
# packages: the PostgreSQL project's own instructions require the same
# repositories for the same reason.
#
#   https://www.postgresql.org/download/linux/debian/
#   https://www.postgresql.org/download/linux/redhat/
#
# This script does exactly what those pages tell you to do, and stops. It
# installs no postvec package, touches no cluster, and writes no PostgreSQL
# configuration — `postvec setup` is where consented change to a database
# happens, and it is a separate command for that reason.
#
# **If you would rather not run a script you downloaded**, that is a reasonable
# position, and the honest answer is `less` — `--print` is still this program
# running, so it is a convenience for after you trust it, not a substitute for
# reading it. The commands it prints are short enough to follow by hand.
#
# **What it changes**, and nothing else:
#
#   Debian / Ubuntu   the PGDG signing key in /usr/share/keyrings — downloaded,
#                     fingerprint-verified, and re-verified if a keyring is
#                     already there — one apt source, and apt update
#   RHEL family       CodeReady Builder (`crb` on the rebuilds,
#                     `codeready-builder-for-rhel-9-<arch>-rpms` through
#                     subscription-manager on RHEL itself); EPEL (the
#                     `epel-release` package on the rebuilds, plus
#                     `epel-next-release` on CentOS Stream, Fedora's release RPM
#                     by URL on RHEL); the PGDG repository RPM,
#                     signature-checked against PGDG's fingerprint-verified RPM
#                     key before install; and `module disable postgresql`, which
#                     otherwise shadows every PGDG package
#
# **It runs on Debian 12, Ubuntu 22.04, Ubuntu 24.04, AlmaLinux 9 and Rocky 9,
# and each of those is executed by tests/bootstrap-test.sh in CI** — twice, in a
# clean container, asserting the fingerprint check ran and PostgreSQL resolves
# afterwards. Rocky is on the list because it is run, not because it is
# AlmaLinux's rebuild.
#
# CentOS Stream 9 and subscribed RHEL 9 differ in real ways — RHEL has no
# `epel-release` package and needs Fedora's release RPM by URL, CentOS Stream
# also wants `epel-next-release` — so the commands are generated correctly for
# each and asserted by tests/unit-test.sh, but they are *printed rather than
# executed* unless you pass --force-untested. Generating the right text and
# surviving execution are different claims, and only one of them has evidence
# for those two.
#
# It is idempotent: running it twice changes nothing the second time.

set -Eeuo pipefail

# PGDG's signing keys, and the fingerprints that make them *the* keys.
#
# Downloading a key over TLS proves you reached a host. Checking its fingerprint
# against a value published somewhere the host does not control is what makes it
# the key you meant. Both are kept identical to versions.env
# (PGDG_DEBIAN_KEY_FINGERPRINT, PGDG_RPM_KEY_FINGERPRINT), which
# tests/unit-test.sh asserts, so the two copies cannot drift.
PGDG_KEY_FINGERPRINT=B97B0AFCAA1A47F044F244A07FCC7D46ACCC4CF8
PGDG_KEY_URL=https://www.postgresql.org/media/keys/ACCC4CF8.asc
PGDG_RPM_KEY_FINGERPRINT=D4BF08AE67A0B4C7A1DBCCD240BCA2B408B40D20
PGDG_RPM_KEY_URL=https://download.postgresql.org/pub/repos/yum/keys/PGDG-RPM-GPG-KEY-RHEL
# PGDG signs each architecture with its own key and publishes them as separate
# files: the x86_64 repository RPM is signed by ...08B40D20 above, the aarch64
# one by ...B9738825 here. A key verifies exactly one architecture, so this is
# selected the way the repository RPM URL already is — by machine, not by
# distribution. Verifying an aarch64 download against the x86_64 fingerprint
# fails closed, which is correct behaviour and a completely useless message.
PGDG_RPM_KEY_AARCH64_FINGERPRINT=B031F89FC983E98262906B6E177B343BB9738825
PGDG_RPM_KEY_AARCH64_URL=https://download.postgresql.org/pub/repos/yum/keys/PGDG-RPM-GPG-KEY-AARCH64-RHEL
KEYRING=/usr/share/keyrings/postgresql-archive-keyring.gpg
SOURCES=/etc/apt/sources.list.d/pgdg.list

# Overridable so the command-generation tests can cover every declared
# distribution without a container per distribution. It changes what is *read*,
# never what is written.
OS_RELEASE="${POSTVEC_OS_RELEASE:-/etc/os-release}"

# Read once, and overridable for the same reason: the signing key, the CRB
# repository name and the repository RPM's URL all vary by machine, and a test
# runner only ever has one of the two architectures under it.
ARCH="${POSTVEC_UNAME_M:-$(uname -m)}"
if [[ "${ARCH}" == aarch64 ]]; then
    PGDG_RPM_KEY_FINGERPRINT="${PGDG_RPM_KEY_AARCH64_FINGERPRINT}"
    PGDG_RPM_KEY_URL="${PGDG_RPM_KEY_AARCH64_URL}"
fi

PG_MAJOR=""
ASSUME_YES=0
PRINT_ONLY=0
FORCE_UNTESTED=0

usage() { sed -n '2,64p' "$0"; }

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

# --------------------------------------------------------------- what we are on

[[ -r "${OS_RELEASE}" ]] || die "cannot read ${OS_RELEASE}; this is not a supported host"
# shellcheck disable=SC1090,SC1091
. "${OS_RELEASE}"

# `FAMILY` selects the package manager. `CRB_METHOD` selects how the builder
# repository PGDG depends on is enabled, which is *not* a property of the family:
# every EL9 rebuild calls it `crb` and enables it with dnf, but subscribed RHEL
# calls it `codeready-builder-for-rhel-9-<arch>-rpms` and enables it through
# subscription-manager. Getting that wrong produces a script that works on
# AlmaLinux, is advertised for RHEL, and fails there — so the two are separate
# decisions here.
FAMILY=""; CRB_METHOD=""; EPEL_METHOD=""; TESTED=0
case "${ID}:${VERSION_ID%%.*}" in
debian:12)           FAMILY=deb; TESTED=1 ;;
ubuntu:22|ubuntu:24) FAMILY=deb; TESTED=1 ;;
almalinux:9|rocky:9) FAMILY=rpm; CRB_METHOD=dnf; EPEL_METHOD=package; TESTED=1 ;;
centos:9)            FAMILY=rpm; CRB_METHOD=dnf; EPEL_METHOD=package-with-next ;;
rhel:9)              FAMILY=rpm; CRB_METHOD=subscription-manager; EPEL_METHOD=url ;;
*)
    # Not a refusal: PGDG supports more than postvec tests, and an operator on
    # a near neighbour knows more about their host than this script does.
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

# ------------------------------------------------------------------ the plan
#
# Built as text first and *shown* before anything runs, because a script that
# configures package repositories as root should be readable before it is
# trusted — and because `--print` has to produce something a person can paste.

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

# The CodeReady Builder repository, by the name and mechanism this distribution
# actually uses. Every EL9 rebuild calls it `crb`; subscribed RHEL does not.
plan_crb() {
    case "${CRB_METHOD}" in
    subscription-manager)
        printf 'subscription-manager repos --enable "codeready-builder-for-rhel-9-%s-rpms"\n' \
            "${ARCH}" ;;
    *)
        printf 'dnf config-manager --set-enabled crb\n' ;;
    esac
}

# EPEL, likewise. `dnf install epel-release` works on the rebuilds because they
# carry the package in `extras`; **subscribed RHEL does not**, and the documented
# path there is Fedora's release RPM by URL. CentOS Stream additionally wants
# `epel-next-release`, which carries builds against the *next* minor.
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

# An unrehearsed path is printed, not executed.
#
# Configuring system repositories as root on a distribution this project has
# never actually run against is not a thing to do on a user's behalf on the
# strength of "the commands look right". The commands above *are* the documented
# ones for this system and are asserted by tests/unit-test.sh — but generating
# the right text and surviving execution are different claims, and only one of
# them has evidence. So: run them yourself, or say explicitly that you accept an
# untested path.
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

# ----------------------------------------------------------------- do the work

# Every fingerprint in a key file, armoured or binary. Captured rather than
# piped into `grep -q`: under `set -o pipefail` that pipeline reports the
# *producer's* death by SIGPIPE when grep exits early on a match, so the shape
# that reads most naturally is the shape that fails on success. Not a thing to
# risk on a signing key.
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

    # Check the fingerprint of whatever is about to be trusted: a file just
    # downloaded, or a keyring already on disk.
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

    # CodeReady Builder first, then EPEL: on RHEL, EPEL's own instructions
    # require CRB to be enabled before its release RPM is installed.
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

    # `dnf install epel-release` resolves on the rebuilds, which ship the
    # package in `extras`. On subscribed RHEL it does not exist under that name
    # and the documented path is Fedora's release RPM by URL — a difference that
    # only shows up when the script actually runs, which is why the untested
    # distributions do not run it by default (see the gate above).
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
        # The Debian path verifies a pinned key fingerprint before trusting an
        # archive; this is the same thing for RPM. The repository RPM's own
        # digest is *not* the pin here: upstream's documented URL is `-latest`
        # and moves by design, so pinning it would break on republication. The
        # key does not move, so the key is the trust root — imported after its
        # fingerprint is checked, and then used to check the RPM's signature
        # before anything is installed.
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

        # Checked against **only** the key just verified.
        #
        # `rpm --checksig` against the host database proves the package is
        # signed by *some* key the host already trusts, which on a machine with
        # other vendors' keys installed is a much weaker statement than it
        # looks — and importing the PGDG key into the host database first makes
        # it weaker still, because then the check cannot distinguish "signed by
        # PGDG" from "signed by anything else already there". So the signature
        # is checked in a throwaway RPM database that contains exactly one key.
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

# ------------------------------------------------------------------- confirm it
#
# "The commands did not fail" is not the same as "the packages are now
# reachable". This asks the package manager the question the next command will
# ask, so a misconfiguration surfaces here rather than three commands later.

check_available() {
    local major="$1" name found=1 policy
    case "${FAMILY}" in
    deb)
        for name in "postgresql-${major}" "postgresql-${major}-pgvector"; do
            # Captured rather than piped into `grep -q`; see the fingerprint
            # check above for why that pipeline lies under `pipefail`.
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
    printf '\n\033[1;32mready\033[0m — now install the packages you downloaded:\n\n' >&2
    case "${FAMILY}" in
    deb) printf '    sudo apt install ./postvec-cli_*.deb ./postgresql-%s-postvec_*.deb\n\n' "${PG_MAJOR}" >&2 ;;
    rpm) printf '    sudo dnf install ./postvec-cli-*.rpm ./postgresql%s-postvec-*.rpm\n\n' "${PG_MAJOR}" >&2 ;;
    esac
else
    printf '\n\033[1;32mready\033[0m — rerun with --pg <major> to confirm that major resolves.\n\n' >&2
fi

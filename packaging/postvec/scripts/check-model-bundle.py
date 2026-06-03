#!/usr/bin/env python3
"""Decide whether a pulled registry model may be packaged.

    check-model-bundle.py <model-dir> <metadata-dir>

model-dir is an installed engine model directory. metadata-dir receives
model-facts.env, SOURCE.json, copyright and receipt-files.nul only if
every check passes.

Does not replace `postvec model show --verify`. Pins arrive in the
environment so the caller cannot half-supply them. model-facts.env is
sourced by later shell, so no free-form registry `source` in it.
Never edits ninference.hub.json.
"""

from __future__ import annotations

import json
import os
import pathlib
import re
import sys
import urllib.parse

RECEIPT_FILE = ".postvec-install.json"
DESCRIPTOR_FILE = "ninference.hub.json"

# The receipt and index contracts this packaging release understands. A newer
# CLI or a newer index may add fields whose meaning packaging cannot guess, and
# guessing is how an unreviewed policy change ships in a package.
KNOWN_RECEIPT_SCHEMA = 1
KNOWN_REGISTRY_SCHEMA = 1

# The one backend the published extension is built with. Candle models are
# perfectly good registry entries and cannot be loaded by these packages.
REQUIRED_BACKEND = "onnx-runtime"
REQUIRED_MODEL_TYPE = "embed"

# Paths a completed pull never leaves behind. If one is present, the directory
# is mid-transaction rather than installed, and copying it would package a
# partial model.
FORBIDDEN_PATH_PARTS = (".staging", ".trash", ".swap", ".postvec.lock")

# Licences packaging is willing to redistribute inside a public package,
# spelled the way Debian and RPM metadata want to see them. This is a reviewed
# packaging policy, not a registry rule: the registry's `license` is a free
# token, and "some permissive-looking id" is not authority to put weights in a
# `.deb`. Adding an id here is the review event.
#
# `apache-2.0` renders the DEP-5 block that references Debian's common copy —
# repeating the Apache text in a copyright file is a lintian error. Every other
# approved id inlines the archive's own LICENSE, indented for DEP-5.
APACHE_2_0_BLOCK = """\
 Licensed under the Apache License, Version 2.0 (the "License"); you may not
 use these files except in compliance with the License. You may obtain a copy
 of the License at
 .
     http://www.apache.org/licenses/LICENSE-2.0
 .
 Unless required by applicable law or agreed to in writing, software
 distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
 WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied. See the
 License for the specific language governing permissions and limitations under
 the License.
 .
 On Debian systems the complete text of the Apache License version 2.0 can be
 found in /usr/share/common-licenses/Apache-2.0."""

APPROVED_LICENSES = {
    # registry id      SPDX spelling     DEP-5 block (None -> inline LICENSE)
    "apache-2.0": ("Apache-2.0", APACHE_2_0_BLOCK),
    "mit": ("MIT", None),
    "bsd-2-clause": ("BSD-2-Clause", None),
    "bsd-3-clause": ("BSD-3-Clause", None),
    "isc": ("ISC", None),
}

MODEL_NAME_RE = re.compile(r"^[a-z0-9][a-z0-9._-]*$")
PKG_SUFFIX_RE = re.compile(r"^[a-z0-9][a-z0-9.+-]*$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
# A DEP-5 Upstream-Name is free text in principle; holding it to a token keeps
# it out of the "free-form registry string rendered into a control file"
# category entirely.
UPSTREAM_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._+-]*$")

problems: list[str] = []


def problem(message: str) -> None:
    problems.append(message)


def die(message: str) -> "None":
    print("check-model-bundle: %s" % message, file=sys.stderr)
    raise SystemExit(2)


def required_env(name: str) -> str:
    value = os.environ.get(name, "")
    if not value:
        die("%s is unset; the caller must supply every expectation" % name)
    return value


def normalize_digest(value: str) -> str:
    """The 64 hex characters, from either representation.

    A receipt writes `sha256:<hex>`; `versions.env` pins the bare hex. Both
    spellings mean the same thing and neither is wrong, so the comparison
    normalizes explicitly rather than relying on one side's formatting.
    """
    return value[len("sha256:") :] if value.startswith("sha256:") else value


def single_line_https_url(value: str) -> bool:
    if not value or len(value) > 2048:
        return False
    if any(character in value for character in "\r\n\t ") or any(
        ord(character) < 0x20 or ord(character) == 0x7F for character in value
    ):
        return False
    if not value.startswith("https://"):
        return False
    parsed = urllib.parse.urlsplit(value)
    return bool(parsed.scheme == "https" and parsed.netloc and "@" not in parsed.netloc)


def dep5_block(text: str) -> str:
    """Indent a licence text for a DEP-5 `License:` paragraph."""
    lines = []
    for line in text.replace("\r\n", "\n").replace("\r", "\n").rstrip("\n").split("\n"):
        stripped = line.rstrip()
        lines.append(" ." if not stripped else " " + stripped)
    return "\n".join(lines)


def main() -> int:
    if len(sys.argv) != 3:
        die("usage: check-model-bundle.py <model-dir> <metadata-dir>")
    model_dir = pathlib.Path(sys.argv[1])
    metadata_dir = pathlib.Path(sys.argv[2])

    want_name = required_env("BUNDLED_MODEL_NAME")
    want_revision = required_env("BUNDLED_MODEL_REGISTRY_REVISION")
    want_digest = required_env("BUNDLED_MODEL_ARCHIVE_SHA256")
    pkg_suffix = required_env("BUNDLED_MODEL_PKG_SUFFIX")
    bundle_version = required_env("BUNDLED_MODEL_BUNDLE_VERSION")

    if not MODEL_NAME_RE.match(want_name):
        die("BUNDLED_MODEL_NAME %r is not a registry model name" % want_name)
    if not PKG_SUFFIX_RE.match(pkg_suffix):
        die("BUNDLED_MODEL_PKG_SUFFIX %r is not a Debian-safe token" % pkg_suffix)
    if not SHA256_RE.match(want_digest):
        die("BUNDLED_MODEL_ARCHIVE_SHA256 is not 64 lowercase hex characters")
    if not re.match(r"^[1-9][0-9]*$", want_revision):
        die("BUNDLED_MODEL_REGISTRY_REVISION must be a positive integer")
    if not re.match(r"^[1-9][0-9]*$", bundle_version):
        die("BUNDLED_MODEL_BUNDLE_VERSION must be a positive integer")

    receipt_path = model_dir / RECEIPT_FILE
    if not receipt_path.is_file():
        die(
            "no %s in %s.\nThe model was not installed by `postvec model pull`, so "
            "nothing here can be verified.\nRe-run scripts/build-model-bundle.sh, or "
            "point --from at a real engine root." % (RECEIPT_FILE, model_dir)
        )
    try:
        receipt = json.loads(receipt_path.read_text())
    except (OSError, ValueError) as error:
        die("cannot read %s: %s" % (receipt_path, error))

    descriptor_path = model_dir / DESCRIPTOR_FILE
    if not descriptor_path.is_file():
        die("no %s in %s; a registry archive always carries one" % (DESCRIPTOR_FILE, model_dir))
    try:
        descriptor = json.loads(descriptor_path.read_text())
    except (OSError, ValueError) as error:
        die("cannot read %s: %s" % (descriptor_path, error))

    # ---------------------------------------------------------- the contracts

    receipt_schema = receipt.get("schema_version")
    if receipt_schema != KNOWN_RECEIPT_SCHEMA:
        problem(
            "receipt schema_version is %r, not %d — the receipt contract was written by a "
            "newer postvec CLI than this packaging release understands; update packaging "
            "deliberately rather than packaging fields it cannot read"
            % (receipt_schema, KNOWN_RECEIPT_SCHEMA)
        )
    registry_schema = receipt.get("registry_schema_version")
    if registry_schema != KNOWN_REGISTRY_SCHEMA:
        problem(
            "registry_schema_version is %r, not %d — the index contract was written by a "
            "newer registry than this packaging release understands"
            % (registry_schema, KNOWN_REGISTRY_SCHEMA)
        )

    # ------------------------------------------------------------- the pin

    name = receipt.get("name")
    if name != want_name:
        problem(
            "the installed model is named %r, but versions.env pins "
            "BUNDLED_MODEL_NAME=%s" % (name, want_name)
        )

    backend = receipt.get("backend")
    if backend != REQUIRED_BACKEND:
        problem(
            "backend is %r; the shipped extension is built with the engine's `onnx` "
            "feature and has only the %s backend, so it could never load this model"
            % (backend, REQUIRED_BACKEND)
        )

    model_type = receipt.get("model_type")
    if model_type != REQUIRED_MODEL_TYPE:
        problem(
            "model_type is %r; the bundled model must be directly embeddable "
            "(%s), because postvec.embed() is what the image promises"
            % (model_type, REQUIRED_MODEL_TYPE)
        )

    access = receipt.get("access")
    if access != "public":
        problem(
            "access is %r; a packaged model must be publicly redistributable. A private "
            "model inside a public package would be a redistribution decision made by "
            "accident — promote it in the registry (`registry-publish --promote`) if it "
            "really should ship" % (access,)
        )

    if receipt.get("license_accepted_at") or receipt.get("license_acceptance_method"):
        problem(
            "the receipt carries local licence-acknowledgement evidence "
            "(license_accepted_at=%r, license_acceptance_method=%r). One developer's "
            "acknowledgement on one host is not authority to redistribute a model in a "
            "package; this packaging release supports no-acknowledgement entries only"
            % (
                receipt.get("license_accepted_at"),
                receipt.get("license_acceptance_method"),
            )
        )

    installed_digest = normalize_digest(str(receipt.get("archive_digest", "")))
    if installed_digest != want_digest:
        problem(
            "archive digest mismatch\n"
            "    installed  %s\n"
            "    pinned     %s (versions.env: BUNDLED_MODEL_ARCHIVE_SHA256)\n"
            "  The registry head is not the one this release was cut against. Review the "
            "change, then: scripts/refresh-pins.sh model" % (installed_digest, want_digest)
        )

    installed_revision = receipt.get("revision", 1)
    if str(installed_revision) != want_revision:
        problem(
            "registry revision mismatch\n"
            "    installed  %s\n"
            "    pinned     %s (versions.env: BUNDLED_MODEL_REGISTRY_REVISION)\n"
            "  The name's head has moved. Review what changed, then: "
            "scripts/refresh-pins.sh model" % (installed_revision, want_revision)
        )

    for field in ("dependencies", "postvec_requires"):
        closure = receipt.get(field) or []
        if closure:
            problem(
                "%s is not empty (%s). A dependency closure is a deliberate packaging "
                "decision — one package per model identity — not something to absorb "
                "silently into one binary package. See "
                "docs/postgres/packaging-registry-models.md §8" % (field, ", ".join(closure))
            )

    # -------------------------------------------------- required package facts

    license_id = receipt.get("license")
    if not license_id:
        problem(
            "the receipt records no `license`; a package must state one in its metadata "
            "and in its copyright file"
        )
        license_spdx, license_block = None, None
    elif license_id not in APPROVED_LICENSES:
        problem(
            "licence %r is not in this packaging release's approved set (%s). Adding one "
            "is a reviewed decision in scripts/check-model-bundle.py, not a build-time "
            "guess" % (license_id, ", ".join(sorted(APPROVED_LICENSES)))
        )
        license_spdx, license_block = None, None
    else:
        license_spdx, license_block = APPROVED_LICENSES[license_id]

    identity = receipt.get("identity")
    if not isinstance(identity, dict):
        problem(
            "the receipt carries no `identity` block, so the vector space this model "
            "produces is unknown; it predates the identity contract and must be "
            "re-pulled with a current postvec CLI"
        )
        identity = {}
    receipt_target_dim = identity.get("target_dim")
    if receipt_target_dim is None:
        problem(
            "identity.target_dim is absent; the package metadata, the metapackage "
            "description and the install test all state the dimension"
        )

    params = descriptor.get("params")
    if not isinstance(params, dict):
        problem("%s has no `params` object" % DESCRIPTOR_FILE)
        params = {}
    sequence_len = params.get("sequence_len")
    if sequence_len is None:
        problem(
            "descriptor params.sequence_len is absent; it is a package fact and part of "
            "the model's compatibility identity"
        )

    source = receipt.get("source")
    if not source or not single_line_https_url(str(source)):
        problem(
            "the registry `source` is %r, which is not one single-line absolute https:// "
            "URL. The Debian provenance document needs a stable upstream source; "
            "registry `source` is otherwise free-form, so packaging refuses what it "
            "cannot put in a control file" % (source,)
        )
        upstream_name = None
    else:
        last = urllib.parse.urlsplit(str(source)).path.rstrip("/").rsplit("/", 1)[-1]
        upstream_name = last.split("@", 1)[0]
        if not UPSTREAM_NAME_RE.match(upstream_name):
            problem(
                "cannot derive a DEP-5 Upstream-Name from source %r (got %r)"
                % (source, upstream_name)
            )
            upstream_name = None

    # ------------------------------------------- receipt against the descriptor

    if descriptor.get("name") != want_name:
        problem(
            "the descriptor names itself %r; the engine resolves models by directory "
            "name and would refuse the load" % (descriptor.get("name"),)
        )
    if not descriptor.get("enabled"):
        problem("the descriptor is not enabled — the engine would skip this model")
    if descriptor.get("backend") != backend:
        problem(
            "descriptor backend %r disagrees with the receipt's %r"
            % (descriptor.get("backend"), backend)
        )
    if params.get("model_type") != model_type:
        problem(
            "descriptor params.model_type %r disagrees with the receipt's %r"
            % (params.get("model_type"), model_type)
        )
    for field in ("source_model", "target_model", "source_dim", "target_dim"):
        if identity.get(field) != params.get(field):
            problem(
                "identity field %s disagrees: the receipt records %r, the descriptor "
                "the engine will read declares %r"
                % (field, identity.get(field), params.get(field))
            )

    # ------------------------------------------------------- the packaged graph

    graphs = sorted(
        str(path.relative_to(model_dir)) for path in model_dir.rglob("*.onnx") if path.is_file()
    )
    file_path = descriptor.get("file_path")
    if len(graphs) != 1 or graphs[0] != file_path:
        problem(
            "the extracted directory carries %d ONNX graph(s) (%s) but the descriptor "
            "references only %r.\n  Unreferenced graphs are a publication decision, not "
            "a packaging one: republish with\n"
            "    registry-publish add %s … --exclude 'onnx/model_*.onnx'\n"
            "  Packaging never prunes an archive — doing so would invalidate the pull's "
            "per-file receipt." % (len(graphs), ", ".join(graphs) or "none", file_path, want_name)
        )

    # ------------------------------------------------------- the archive's files

    receipt_files = receipt.get("files")
    if not isinstance(receipt_files, list) or not receipt_files:
        problem("the receipt lists no files; there is nothing to package")
        receipt_files = []

    relative_paths: list[str] = []
    files_sha256: dict[str, str] = {}
    for entry in receipt_files:
        if not isinstance(entry, dict):
            problem("a receipt `files` entry is not an object: %r" % (entry,))
            continue
        relative = entry.get("path")
        digest = entry.get("sha256")
        if not isinstance(relative, str) or not relative:
            problem("a receipt `files` entry has no path: %r" % (entry,))
            continue
        if (
            relative.startswith("/")
            or ".." in pathlib.PurePosixPath(relative).parts
            or "\\" in relative
            or any(ord(character) < 0x20 for character in relative)
        ):
            problem("receipt file path %r is not a safe relative path" % (relative,))
            continue
        if relative == RECEIPT_FILE:
            problem("the receipt lists itself as an archive file")
            continue
        parts = pathlib.PurePosixPath(relative).parts
        if any(part in FORBIDDEN_PATH_PARTS for part in parts):
            problem("receipt file path %r is inside a transaction directory" % (relative,))
            continue
        if not isinstance(digest, str) or not SHA256_RE.match(digest):
            problem("receipt file %r has no sha256" % (relative,))
            continue
        if not (model_dir / relative).is_file():
            problem("receipt file %r is not present in the model directory" % (relative,))
            continue
        relative_paths.append(relative)
        files_sha256[relative] = digest

    if "LICENSE" not in files_sha256:
        problem(
            "the archive carries no LICENSE. A registry archive always does — the "
            "publisher adds one when the model directory lacks it — so its absence is a "
            "publication defect, not something packaging may substitute for"
        )

    on_disk = {
        str(path.relative_to(model_dir))
        for path in model_dir.rglob("*")
        if path.is_file() and str(path.relative_to(model_dir)) != RECEIPT_FILE
    }
    for extra in sorted(on_disk - set(relative_paths)):
        problem(
            "%r is in the model directory but not in the receipt; `model show --verify` "
            "should already have refused this" % (extra,)
        )

    if problems:
        print(
            "check-model-bundle: %d problem(s) with the pulled model at %s"
            % (len(problems), model_dir),
            file=sys.stderr,
        )
        for item in problems:
            print("  - %s" % item, file=sys.stderr)
        return 1

    # ------------------------------------------------------------- the outputs

    pkg_name = "postvec-model-%s" % pkg_suffix
    doc_dir = "/usr/share/doc/%s" % pkg_name
    pkg_version = "%s.%s.0" % (want_revision, bundle_version)

    metadata_dir.mkdir(parents=True, exist_ok=True)

    (metadata_dir / "model-facts.env").write_text(
        "# Derived from the verified registry archive by "
        "scripts/check-model-bundle.py.\n"
        "# Every value is a validator-constrained token, integer, digest or a path\n"
        "# derived from one. The registry's free-form `source` string is deliberately\n"
        "# absent: this file is read by shell, and free text read by shell is code.\n"
        "MODEL_PKG_NAME=%s\n"
        "MODEL_PKG_VERSION=%s\n"
        "MODEL_DOC_DIR=%s\n"
        "MODEL_NAME=%s\n"
        "MODEL_BACKEND=%s\n"
        "MODEL_REGISTRY_REVISION=%s\n"
        "MODEL_ARCHIVE_SHA256=%s\n"
        "MODEL_ARCHIVE_SIZE=%d\n"
        "MODEL_LICENSE=%s\n"
        "MODEL_TARGET_DIM=%d\n"
        "MODEL_SEQUENCE_LEN=%d\n"
        "MODEL_BUNDLE_VERSION=%s\n"
        % (
            pkg_name,
            pkg_version,
            doc_dir,
            want_name,
            backend,
            want_revision,
            want_digest,
            int(receipt["archive_size"]),
            license_spdx,
            int(receipt_target_dim),
            int(sequence_len),
            bundle_version,
        )
    )

    (metadata_dir / "SOURCE.json").write_text(
        json.dumps(
            {
                "component": "registry-model",
                "internal_name": want_name,
                "backend": backend,
                "model_type": model_type,
                "channel": access,
                "registry_revision": int(want_revision),
                "archive_sha256": want_digest,
                "archive_size": int(receipt["archive_size"]),
                "registry_schema_version": registry_schema,
                "source": source,
                "license": license_id,
                "target_dim": int(receipt_target_dim),
                "sequence_len": int(sequence_len),
                "bundle_version": int(bundle_version),
                "files_sha256": files_sha256,
            },
            indent=2,
            sort_keys=True,
        )
        + "\n"
    )

    template = pathlib.Path(__file__).resolve().parent.parent / "model" / "copyright.in"
    if license_block is None:
        license_block = dep5_block((model_dir / "LICENSE").read_text(errors="replace"))
    rendered = template.read_text()
    for placeholder, value in (
        ("@UPSTREAM_NAME@", upstream_name),
        ("@MODEL_NAME@", want_name),
        ("@SOURCE@", str(source)),
        ("@LICENSE@", license_spdx),
        ("@DOC_DIR@", doc_dir),
        ("@LICENSE_BLOCK@", license_block),
    ):
        rendered = rendered.replace(placeholder, value)
    left = re.search(r"@[A-Z_]+@", rendered)
    if left:
        die("model/copyright.in has an unrendered placeholder: %s" % left.group(0))
    (metadata_dir / "copyright").write_text(rendered)

    # NUL separated, so the copier never has to decide what a newline in a path
    # would have meant. (Nothing the strict extractor produces contains one;
    # the point is that the copier does not depend on that being true.)
    (metadata_dir / "receipt-files.nul").write_bytes(
        b"".join(path.encode() + b"\0" for path in sorted(relative_paths))
    )

    print(
        "  %s: revision %s, %s, %d file(s), %d dimensions, %s"
        % (want_name, want_revision, license_spdx, len(relative_paths), int(receipt_target_dim), access)
    )
    if descriptor.get("execution_providers") not in (None, ["cpu"]):
        print(
            "  note: the descriptor requests execution providers %s. A build without the\n"
            "        `ort-cuda` feature skips the provider and falls back to CPU, so this is\n"
            "        a warning: expect one \"CUDA execution provider was requested, but the\n"
            "        application was not compiled with the 'ort-cuda' feature. Skipping.\"\n"
            "        line per model load in the embedded image's PostgreSQL log."
            % (descriptor["execution_providers"],)
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())

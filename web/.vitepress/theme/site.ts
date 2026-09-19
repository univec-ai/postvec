export const SITE = {
  name: "postvec",
  title: "postvec",
  description:
    "Fused hybrid search for PostgreSQL: BM25 and semantic search in one SQL call, vectors kept in sync with text, and direct conversion between embedding formats. Inference runs locally inside PostgreSQL, on postvec-server, or through hosted embedding APIs.",
  url: "https://postvec.dev",
  github: "https://github.com/univec-ai/postvec",
  githubRepo: "univec-ai/postvec",
  releaseTag: "postvec-v0.2.0-2",
  ghcr: "ghcr.io/univec-ai/postvec",
  // The inference node's image: its own repository, tagged with the bare
  // release id and a `latest` moving tag (no PostgreSQL major to hide).
  ghcrServer: "ghcr.io/univec-ai/postvec-server",
  // Packaging pin SERVER_LICENSE: the one artifact family not under the
  // PostgreSQL License.
  serverLicense: "BUSL-1.1",
  univec: "https://univec.ai",
  version: "0.2.0",
  release: "0.2.0-2",
  packageRelease: "2",
  onnxRuntimeVersion: "1.22.0",
  // Packaging pin: registry revision 2 + bundle 1 → 2.1.0
  bundledModelVersion: "2.1.0",
  releaseStage: "public beta",
  registryStage: "preview",
  signerWorkflow: "univec-ai/postvec/.github/workflows/postvec-release.yml",
  conversionPairs: "nearly 100",
} as const;

export const DISTROS = [
  { id: "debian12", label: "Debian 12", tag: "+deb12", family: "deb" as const },
  { id: "ubuntu2204", label: "Ubuntu 22.04", tag: "+ubuntu22.04", family: "deb" as const },
  { id: "ubuntu2404", label: "Ubuntu 24.04", tag: "+ubuntu24.04", family: "deb" as const },
  { id: "el9", label: "EL9 (Alma / Rocky / RHEL)", tag: ".el9", family: "rpm" as const },
] as const;

export const PG_MAJORS = [16, 17, 18] as const;
export const ARCHES = [
  { id: "amd64", deb: "amd64", rpm: "x86_64", label: "amd64" },
  { id: "arm64", deb: "arm64", rpm: "aarch64", label: "arm64" },
] as const;

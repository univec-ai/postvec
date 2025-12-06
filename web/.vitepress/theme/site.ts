export const SITE = {
  name: "postvec",
  title: "postvec",
  description:
    "Local embedding, hybrid search, and in-place vector migration for PostgreSQL.",
  url: "https://postvec.dev",
  github: "https://github.com/univec-ai/stack",
  githubRepo: "univec-ai/stack",
  releaseTag: "postvec-v0.1.0-1",
  ghcr: "ghcr.io/univec-ai/postvec",
  univec: "https://univec.ai",
  version: "0.1.0",
  release: "0.1.0-1",
  packageRelease: "1",
  onnxRuntimeVersion: "1.22.0",
  bundledModelVersion: "1.1.0",
  releaseStage: "preview",
  registryStage: "preview",
  signerWorkflow: "univec-ai/stack/.github/workflows/postvec-release.yml",
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

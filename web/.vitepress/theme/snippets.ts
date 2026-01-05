import { SITE } from "./site";
import type { PgMajor } from "./composables/pgMajor";

export type Tokens = {
  pg: string;
  release: string;
  version: string;
  vol: string;
  image: string;
  imageComplete: string;
  imageMoving: string;
  imageMovingRemote: string;
  pkgdeb: string;
  pkgel: string;
  debfile: string;
  rpmfile: string;
  clideb: string;
  clirpm: string;
  libdeb: string;
  shareded: string;
  libel: string;
  shareel: string;
  pgconfigdeb: string;
  pgconfigel: string;
  svcdeb: string;
  svcel: string;
  cluster: string;
  feature: string;
  stage: string;
  sqlfile: string;
};

export function tokens(pg: PgMajor): Tokens {
  return {
    pg: String(pg),
    release: SITE.release,
    version: SITE.version,
    vol: pg === 18 ? "/var/lib/postgresql" : "/var/lib/postgresql/data",
    image: `${SITE.ghcr}:${SITE.release}-pg${pg}`,
    imageComplete: `${SITE.ghcr}:${SITE.release}-pg${pg}-complete`,
    imageMoving: `${SITE.ghcr}:pg${pg}-complete`,
    imageMovingRemote: `${SITE.ghcr}:pg${pg}`,
    pkgdeb: `postgresql-${pg}-postvec`,
    pkgel: `postgresql${pg}-postvec`,
    debfile: `postgresql-${pg}-postvec_${SITE.release}+deb12_amd64.deb`,
    rpmfile: `postgresql${pg}-postvec-${SITE.release}.el9.x86_64.rpm`,
    clideb: `postvec-cli_${SITE.release}+deb12_amd64.deb`,
    clirpm: `postvec-cli-${SITE.release}.el9.x86_64.rpm`,
    libdeb: `/usr/lib/postgresql/${pg}/lib`,
    shareded: `/usr/share/postgresql/${pg}/extension`,
    libel: `/usr/pgsql-${pg}/lib`,
    shareel: `/usr/pgsql-${pg}/share/extension`,
    pgconfigdeb: `/usr/lib/postgresql/${pg}/bin/pg_config`,
    pgconfigel: `/usr/pgsql-${pg}/bin/pg_config`,
    svcdeb: `postgresql@${pg}-main`,
    svcel: `postgresql-${pg}`,
    cluster: `${pg}/main`,
    feature: `pg${pg}`,
    stage: `postvec-pg${pg}`,
    sqlfile: `postvec--${SITE.version}.sql`,
  };
}

export type FamilyId = "debian" | "el9";

export type FamilySnippet = {
  id: FamilyId;
  label: string;
  render: (t: Tokens) => string;
};

export type SnippetDef = {
  lang?: string;
  render?: (t: Tokens) => string;
  families?: FamilySnippet[];
};

export const SNIPPETS: Record<string, SnippetDef> = {
  "docker-quickstart": {
    lang: "bash",
    render: (t) =>
      [
        "docker run -d --name postvec \\",
        "  -e POSTGRES_PASSWORD=demo \\",
        "  -e POSTGRES_USER=app \\",
        "  -e POSTGRES_DB=app \\",
        "  -p 127.0.0.1:5433:5432 \\",
        `  ${t.imageComplete}`,
      ].join("\n"),
  },

  "docker-embedded": {
    lang: "bash",
    render: (t) =>
      [
        "docker run -d --name postvec \\",
        "  -e POSTGRES_PASSWORD_FILE=/run/secrets/postgres-password \\",
        "  -e POSTGRES_DB=app \\",
        `  -v postvec-data:${t.vol} \\`,
        "  -p 127.0.0.1:5432:5432 \\",
        `  ${t.imageComplete}`,
      ].join("\n"),
  },

  "docker-embedded-demo": {
    lang: "bash",
    render: (t) =>
      [
        "docker run -d --name postvec \\",
        "  -e POSTGRES_PASSWORD=demo \\",
        "  -e POSTGRES_DB=app \\",
        `  -v postvec-data:${t.vol} \\`,
        "  -p 127.0.0.1:5432:5432 \\",
        `  ${t.imageComplete}`,
      ].join("\n"),
  },

  "docker-remote": {
    lang: "bash",
    render: (t) =>
      [
        "docker run -d --name postvec \\",
        "  -e POSTGRES_PASSWORD=demo \\",
        "  -e POSTGRES_DB=app \\",
        "  -e POSTVEC_GRPC_ENDPOINTS=10.0.0.20:33333 \\",
        "  -e POSTVEC_HTTP_ENDPOINTS=https://10.0.0.20:22222 \\",
        `  -v postvec-data:${t.vol} \\`,
        "  -p 127.0.0.1:5432:5432 \\",
        `  ${t.image}`,
      ].join("\n"),
  },

  "docker-pull": {
    lang: "bash",
    render: (t) => `docker pull ${t.imageComplete}`,
  },

  "docker-tags": {
    lang: "text",
    render: (t) =>
      [
        `Pinned, embedded:  ${t.imageComplete}`,
        `Pinned, remote:    ${t.image}`,
        `Moving, embedded:  ${t.imageMoving}`,
        `Moving, remote:    ${t.imageMovingRemote}`,
      ].join("\n"),
  },

  prerequisites: {
    lang: "bash",
    render: (t) =>
      [
        "gh --version   # 2.49 or newer",
        "",
        "gh attestation verify postvec-prerequisites.sh \\",
        `  --repo ${SITE.githubRepo} \\`,
        `  --signer-workflow ${SITE.signerWorkflow}`,
        "less postvec-prerequisites.sh",
        `sudo bash ./postvec-prerequisites.sh --pg ${t.pg}`,
      ].join("\n"),
  },

  "packages-complete": {
    lang: "bash",
    families: [
      {
        id: "debian",
        label: "Debian / Ubuntu",
        render: (t) =>
          [
            "sudo apt install \\",
            `  ./${t.clideb} \\`,
            `  ./${t.debfile} \\`,
            "  ./postvec-onnxruntime_*.deb \\",
            "  ./postvec-model-minilm-l6-v2_*.deb \\",
            "  ./postvec-extras_*.deb",
          ].join("\n"),
      },
      {
        id: "el9",
        label: "EL9",
        render: (t) =>
          [
            "sudo dnf install \\",
            `  ./${t.clirpm} \\`,
            `  ./${t.rpmfile} \\`,
            "  ./postvec-onnxruntime-*.rpm \\",
            "  ./postvec-model-minilm-l6-v2-*.rpm \\",
            "  ./postvec-extras-*.rpm",
          ].join("\n"),
      },
    ],
  },

  "packages-remote": {
    lang: "bash",
    families: [
      {
        id: "debian",
        label: "Debian / Ubuntu",
        render: (t) =>
          [
            "sudo apt install \\",
            `  ./${t.clideb} \\`,
            `  ./${t.debfile}`,
          ].join("\n"),
      },
      {
        id: "el9",
        label: "EL9",
        render: (t) =>
          [
            "sudo dnf install \\",
            `  ./${t.clirpm} \\`,
            `  ./${t.rpmfile}`,
          ].join("\n"),
      },
    ],
  },

  "packages-verify-download": {
    lang: "bash",
    families: [
      {
        id: "debian",
        label: "Debian / Ubuntu",
        render: (t) =>
          [
            `SIGNER=${SITE.signerWorkflow}`,
            "",
            "sha256sum --ignore-missing --check SHA256SUMS",
            `gh attestation verify ${t.debfile} \\`,
            `  --repo ${SITE.githubRepo} --signer-workflow "$SIGNER"`,
          ].join("\n"),
      },
      {
        id: "el9",
        label: "EL9",
        render: (t) =>
          [
            `SIGNER=${SITE.signerWorkflow}`,
            "",
            "sha256sum --ignore-missing --check SHA256SUMS",
            `gh attestation verify ${t.rpmfile} \\`,
            `  --repo ${SITE.githubRepo} --signer-workflow "$SIGNER"`,
          ].join("\n"),
      },
    ],
  },

  "packages-verify-files": {
    lang: "bash",
    families: [
      {
        id: "debian",
        label: "Debian / Ubuntu",
        render: (t) =>
          [
            "postvec --version",
            `test -f ${t.libdeb}/postvec.so`,
            `test -f ${t.shareded}/postvec.control`,
          ].join("\n"),
      },
      {
        id: "el9",
        label: "EL9",
        render: (t) =>
          [
            "postvec --version",
            `test -f ${t.libel}/postvec.so`,
            `test -f ${t.shareel}/postvec.control`,
          ].join("\n"),
      },
    ],
  },

  "source-prereq": {
    lang: "bash",
    families: [
      {
        id: "debian",
        label: "Debian / Ubuntu",
        render: (t) =>
          [
            "rustc --version",
            "cargo pgrx --version",
            "protoc --version",
            `${t.pgconfigdeb} --version`,
          ].join("\n"),
      },
      {
        id: "el9",
        label: "EL9",
        render: (t) =>
          [
            "rustc --version",
            "cargo pgrx --version",
            "protoc --version",
            `${t.pgconfigel} --version`,
          ].join("\n"),
      },
    ],
  },

  "source-build": {
    lang: "bash",
    families: [
      {
        id: "debian",
        label: "Debian / Ubuntu",
        render: (t) =>
          [
            "cd postvec",
            "cargo pgrx package \\",
            "  --no-default-features \\",
            `  --features ${t.feature},embedded \\`,
            `  --pg-config ${t.pgconfigdeb}`,
            "",
            "cd ..",
            "cargo build --release -p postvec-cli",
          ].join("\n"),
      },
      {
        id: "el9",
        label: "EL9",
        render: (t) =>
          [
            "cd postvec",
            "cargo pgrx package \\",
            "  --no-default-features \\",
            `  --features ${t.feature},embedded \\`,
            `  --pg-config ${t.pgconfigel}`,
            "",
            "cd ..",
            "cargo build --release -p postvec-cli",
          ].join("\n"),
      },
    ],
  },

  "source-install-pgdg": {
    lang: "bash",
    families: [
      {
        id: "debian",
        label: "Debian / Ubuntu",
        render: (t) =>
          [
            `export PV_STAGE=postvec/target/release/${t.stage}`,
            "",
            "sudo install -m 0755 \\",
            `  "$PV_STAGE${t.libdeb}/postvec.so" \\`,
            `  ${t.libdeb}/postvec.so`,
            "sudo install -m 0644 \\",
            `  "$PV_STAGE${t.shareded}/postvec.control" \\`,
            `  "$PV_STAGE${t.shareded}/${t.sqlfile}" \\`,
            `  ${t.shareded}/`,
            "",
            "sudo install -m 0755 target/release/postvec /usr/local/bin/postvec",
          ].join("\n"),
      },
      {
        id: "el9",
        label: "EL9",
        render: (t) =>
          [
            `export PV_STAGE=postvec/target/release/${t.stage}`,
            "",
            "sudo install -m 0755 \\",
            `  "$PV_STAGE${t.libel}/postvec.so" \\`,
            `  ${t.libel}/postvec.so`,
            "sudo install -m 0644 \\",
            `  "$PV_STAGE${t.shareel}/postvec.control" \\`,
            `  "$PV_STAGE${t.shareel}/${t.sqlfile}" \\`,
            `  ${t.shareel}/`,
            "",
            "sudo install -m 0755 target/release/postvec /usr/local/bin/postvec",
          ].join("\n"),
      },
    ],
  },

  "setup-embedded": {
    lang: "bash",
    render: (t) =>
      [
        "sudo postvec setup --database app \\",
        "  --embedded --path /opt/postvec/ninference",
        "",
        "postvec model ls",
        "sudo postvec doctor --database app --deep",
      ].join("\n"),
  },

  "setup-embedded-cluster": {
    lang: "bash",
    families: [
      {
        id: "debian",
        label: "Debian / Ubuntu",
        render: (t) =>
          [
            `sudo postvec --cluster ${t.cluster} setup --database app \\`,
            "  --embedded --path /opt/postvec/ninference",
            "",
            `sudo postvec --cluster ${t.cluster} doctor --database app --deep`,
          ].join("\n"),
      },
      {
        id: "el9",
        label: "EL9",
        render: (t) =>
          [
            "sudo -u postgres postvec setup \\",
            `  --pg-config ${t.pgconfigel} \\`,
            "  --config-dir /path/included/by/postgresql.conf \\",
            "  --database app \\",
            "  --embedded --path /opt/postvec/ninference \\",
            "  --no-restart",
            "",
            `sudo systemctl restart ${t.svcel}.service`,
            "sudo -u postgres postvec doctor \\",
            `  --pg-config ${t.pgconfigel} \\`,
            "  --database app --deep",
          ].join("\n"),
      },
    ],
  },

  "upgrade-packages": {
    lang: "bash",
    families: [
      {
        id: "debian",
        label: "Debian / Ubuntu",
        render: (t) =>
          [
            `sudo apt install ./${t.pkgdeb}_NEWVERSION-1+deb12_amd64.deb`,
            `sudo systemctl restart ${t.svcdeb}`,
            "psql -d app -c 'ALTER EXTENSION postvec UPDATE'",
            `sudo postvec --cluster ${t.cluster} doctor --database app --deep`,
          ].join("\n"),
      },
      {
        id: "el9",
        label: "EL9",
        render: (t) =>
          [
            `sudo dnf install ./${t.pkgel}-NEWVERSION-1.el9.x86_64.rpm`,
            `sudo systemctl restart ${t.svcel}.service`,
            "psql -d app -c 'ALTER EXTENSION postvec UPDATE'",
            `sudo -u postgres postvec doctor --pg-config ${t.pgconfigel} --database app --deep`,
          ].join("\n"),
      },
    ],
  },

  "uninstall-packages": {
    lang: "bash",
    families: [
      {
        id: "debian",
        label: "Debian / Ubuntu",
        render: (t) =>
          [
            `sudo apt remove ${t.pkgdeb} postvec-cli`,
            "sudo apt remove postvec-extras postvec-model-minilm-l6-v2 \\",
            "  postvec-onnxruntime",
          ].join("\n"),
      },
      {
        id: "el9",
        label: "EL9",
        render: (t) =>
          [
            `sudo dnf remove ${t.pkgel} postvec-cli`,
            "sudo dnf remove postvec-extras postvec-model-minilm-l6-v2 \\",
            "  postvec-onnxruntime",
          ].join("\n"),
      },
    ],
  },

  "model-pull": {
    lang: "bash",
    render: (t) =>
      [
        "postvec model ls --available",
        "sudo postvec model pull baai-bge-m3 --dry-run",
        "sudo postvec model pull baai-bge-m3 --yes",
        "postvec model show baai-bge-m3 --verify",
        "# pull installs; it does not serve. Turn the model on:",
        "sudo postvec model activate baai-bge-m3 --yes",
        `sudo postvec --cluster ${t.cluster} doctor --database app --deep`,
      ].join("\n"),
  },
};

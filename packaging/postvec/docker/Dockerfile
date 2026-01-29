# The postvec images: a drop-in replacement for the official `postgres` image
# with postvec and pgvector preinstalled and preconfigured.
#
# Two targets:
#
#   remote    PostgreSQL + pgvector + postvec, talking to postvec-server nodes.
#             It carries no engine assets, so it pins POSTVEC_MODE=grpc — the
#             extension's own default is `embedded`, which this image cannot
#             serve.
#   complete  the same, plus ONNX Runtime and one embedding model. In-process
#             inference; POSTVEC_MODE=grpc still works. One container, both
#             modes.
#
# The build consumes the *already built and verified* release packages for this
# commit; it never compiles anything. That is what makes the image and the
# `.deb` files provably the same software, and it keeps a compiler, a Cargo
# cache and a source tree out of the final image.
#
# Build it through scripts/build-image.sh, which supplies
# every ARG from packaging/postvec/versions.env.
#
# Deliberately plain Dockerfile syntax — no BuildKit cache mounts. The builder
# images need them (a Rust compile without a cargo cache is unbearable); this
# one only installs a handful of local .deb files, so keeping it buildable by
# any Docker is worth more than the seconds a cache would save.

ARG PG_MAJOR=18
ARG DEBIAN_CODENAME=bookworm
ARG POSTGRES_IMAGE_DIGEST

# ---------------------------------------------------------------------- remote

FROM postgres:${PG_MAJOR}-${DEBIAN_CODENAME}@${POSTGRES_IMAGE_DIGEST} AS remote

ARG PG_MAJOR
ARG POSTVEC_VERSION
ARG SOURCE_DATE_EPOCH

# CI stages one architecture's packages here. They have already passed
# checksum, content and ELF verification — this is an install, not a download.
COPY dist/packages/ /tmp/postvec-packages/

# `apt-get install ./file.deb` (not `dpkg -i`) so pgvector and the CLI's own
# dependencies are resolved rather than assumed.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        "postgresql-${PG_MAJOR}-pgvector" \
        /tmp/postvec-packages/postvec-cli_*.deb \
        /tmp/postvec-packages/postgresql-${PG_MAJOR}-postvec_*.deb; \
    rm -rf /tmp/postvec-packages /var/lib/apt/lists/*; \
    postvec --version; \
    test -f "/usr/lib/postgresql/${PG_MAJOR}/lib/postvec.so"

COPY packaging/postvec/docker/postvec-entrypoint.sh   /usr/local/bin/postvec-entrypoint
COPY packaging/postvec/docker/postvec-healthcheck.sh  /usr/local/bin/postvec-healthcheck
COPY packaging/postvec/docker/20-create-postvec.sh    /docker-entrypoint-initdb.d/20-create-postvec.sh

RUN chmod 0755 \
        /usr/local/bin/postvec-entrypoint \
        /usr/local/bin/postvec-healthcheck \
        /docker-entrypoint-initdb.d/20-create-postvec.sh

# Load-bearing, not decoration: the extension defaults to `embedded`, and this
# variant deliberately ships without ONNX Runtime or a model. Pinning grpc is
# what makes an unconfigured `docker run` of this tag fail with "no endpoints"
# rather than with a missing engine root.
ENV POSTVEC_MODE=grpc \
    POSTVEC_CREATE_EXTENSION=1

ENTRYPOINT ["postvec-entrypoint"]
CMD ["postgres"]

# start-period is generous on purpose: loading a model is readiness, not
# liveness, and a cold start on a small machine is slow. Once healthy, a
# heartbeat older than the liveness interval plus a few poll ticks makes
# the container unhealthy.
HEALTHCHECK --interval=10s --timeout=10s --start-period=180s --retries=6 \
    CMD ["postvec-healthcheck"]

ARG IMAGE_SOURCE
ARG GIT_REVISION
ARG BUILD_DATE
LABEL org.opencontainers.image.title="postvec" \
      org.opencontainers.image.description="PostgreSQL with postvec and pgvector" \
      org.opencontainers.image.version="${POSTVEC_VERSION}" \
      org.opencontainers.image.revision="${GIT_REVISION}" \
      org.opencontainers.image.source="${IMAGE_SOURCE}" \
      org.opencontainers.image.licenses="PostgreSQL" \
      org.opencontainers.image.created="${BUILD_DATE}" \
      ai.univec.postvec.pg-major="${PG_MAJOR}" \
      ai.univec.postvec.mode="grpc"

# -------------------------------------------------------------------- complete

FROM remote AS complete

ARG PG_MAJOR
ARG POSTVEC_VERSION
ARG BUNDLED_MODEL_NAME
ARG BUNDLED_MODEL_BACKEND
ARG BUNDLED_MODEL_REGISTRY_REVISION
ARG BUNDLED_MODEL_PACKAGE_VERSION
ARG ORT_VERSION

# The same package files an operator would install on a host, so the image's
# engine assets and the published packages are provably identical content.
COPY dist/complete/ /tmp/postvec-complete/

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        /tmp/postvec-complete/postvec-onnxruntime_*.deb \
        /tmp/postvec-complete/postvec-model-*.deb \
        /tmp/postvec-complete/postvec-extras_*.deb; \
    rm -rf /tmp/postvec-complete /var/lib/apt/lists/*; \
    test -d "/opt/postvec/ninference/models/${BUNDLED_MODEL_BACKEND}/${BUNDLED_MODEL_NAME}"; \
    ls /opt/postvec/ninference/libs/onnxruntime/lib/libonnxruntime.so

# LD_LIBRARY_PATH is for ONNX Runtime's own adjacent libraries; the extension
# itself finds libonnxruntime by searching the engine root and dlopen()ing it.
ENV POSTVEC_MODE=embedded \
    POSTVEC_NINFERENCE_PATH=/opt/postvec/ninference \
    POSTVEC_EMBEDDED_MODELS=${BUNDLED_MODEL_NAME} \
    LD_LIBRARY_PATH=/opt/postvec/ninference/libs/onnxruntime/lib

# The model label is <name>@<registry revision>, which is the identity a reader
# can look up in the registry — a package version is a packaging fact and says
# nothing about which bytes are inside.
LABEL ai.univec.postvec.mode="embedded" \
      ai.univec.postvec.variant="complete" \
      ai.univec.postvec.model-bundle="${BUNDLED_MODEL_NAME}@${BUNDLED_MODEL_REGISTRY_REVISION}" \
      ai.univec.postvec.model-package-version="${BUNDLED_MODEL_PACKAGE_VERSION}" \
      ai.univec.postvec.onnxruntime="${ORT_VERSION}" \
      org.opencontainers.image.description="PostgreSQL with postvec, pgvector, the CLI, ONNX Runtime and the default model. One container; both inference modes."

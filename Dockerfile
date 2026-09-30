# syntax=docker/dockerfile:1
#
# One ferrule bot per container (docs/docker.md, docs/m44-managed-mode.md).
#
#   docker build -t ferrule .                                   # from source
#   docker build --target runtime-browser -t ferrule:browser .  # with Chromium
#
# The release workflow builds from the release's static binaries instead:
#   --build-arg FERRULE_BIN_FROM=prebuilt, with dist/<arch>/ferrule in the context.

ARG FERRULE_BIN_FROM=build
ARG DEBIAN=debian:bookworm-slim

# ---- the binary, compiled here: static, against musl ------------------------
# Distribution packages are left unpinned on purpose, here and below: a
# rebuild picks up their security fixes (hadolint DL3008/DL3018).
# hadolint global ignore=DL3008,DL3018
FROM rust:1.98-alpine AS build
RUN apk add --no-cache musl-dev perl make
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p ferrule-cli \
    && mkdir /out && cp target/release/ferrule /out/ferrule

# ---- the binary, from the release (dist/amd64/ferrule, dist/arm64/ferrule) --
# hadolint ignore=DL3006
FROM ${DEBIAN} AS prebuilt
ARG TARGETARCH
COPY dist/${TARGETARCH}/ferrule /out/ferrule
RUN chmod 0755 /out/ferrule

# hadolint ignore=DL3006
FROM ${FERRULE_BIN_FROM} AS bin

# ---- the bot ----------------------------------------------------------------
# hadolint ignore=DL3006
FROM ${DEBIAN} AS runtime
# A real userland for the agent's shell; tini reaps its children and passes
# SIGTERM on, even without `docker run --init`.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        tini bash coreutils findutils grep sed git curl ca-certificates tzdata procps \
    && rm -rf /var/lib/apt/lists/*
RUN groupadd --gid 10001 ferrule \
    && useradd --uid 10001 --gid 10001 --home-dir /data/home --no-create-home \
        --shell /bin/bash ferrule \
    && mkdir -p /data /etc/ferrule \
    && chown 10001:10001 /data
COPY --from=bin /out/ferrule /usr/local/bin/ferrule
ENV FERRULE_MANAGED=1 \
    FERRULE_DATA_DIR=/data \
    FERRULE_CONFIG=/data/ferrule.toml \
    FERRULE_DASHBOARD_BIND=0.0.0.0 \
    FERRULE_DASHBOARD_PORT=8080 \
    FERRULE_HTTP_BIND=0.0.0.0 \
    HOME=/data/home \
    LANG=C.UTF-8
USER 10001:10001
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8080
EXPOSE 8788
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD ["ferrule", "health", "--probe"]
ENTRYPOINT ["/usr/bin/tini", "--", "ferrule"]
CMD ["gateway", "--workspace", "/data/workspace"]

# ---- the bot with a browser -------------------------------------------------
FROM runtime AS runtime-browser
USER root
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
ARG TARGETARCH
# agent-browser's own static (musl) binary from its npm tarball; no Node.
# Pinned, and checked against the registry's integrity string.
ARG AGENT_BROWSER_VERSION=0.38.1
ARG AGENT_BROWSER_SHA512=k58FCz0yUOCANoNkMiqJe+H2y6r6sUZazqXsWF+MYq1iRC42PjtLcBoag6SSTOD/FRQppvPDvE5HDYEhclvnhw==
RUN apt-get update \
    && apt-get install -y --no-install-recommends chromium fonts-liberation fonts-noto-core \
    && rm -rf /var/lib/apt/lists/* \
    && case "${TARGETARCH}" in amd64) ab=x64 ;; arm64) ab=arm64 ;; *) echo "no agent-browser for ${TARGETARCH}" >&2; exit 1 ;; esac \
    && curl -fsSL -o /tmp/ab.tgz \
        "https://registry.npmjs.org/agent-browser/-/agent-browser-${AGENT_BROWSER_VERSION}.tgz" \
    && echo "${AGENT_BROWSER_SHA512}" | base64 -d | od -An -tx1 | tr -d ' \n' > /tmp/want \
    && sha512sum /tmp/ab.tgz | cut -d' ' -f1 | tr -d '\n' > /tmp/got \
    && cmp /tmp/want /tmp/got \
    && tar -xzf /tmp/ab.tgz -C /tmp "package/bin/agent-browser-linux-musl-${ab}" \
    && install -m 0755 "/tmp/package/bin/agent-browser-linux-musl-${ab}" /usr/local/bin/agent-browser \
    && rm -rf /tmp/ab.tgz /tmp/want /tmp/got /tmp/package \
    && agent-browser --version
# Chrome's own sandbox needs user namespaces, which Docker's default seccomp
# profile blocks: Chrome runs with --no-sandbox inside ferrule's (Landlock)
# sandbox. Set 1 with a profile that allows them (docs/docker.md).
ENV CHROME_PATH=/usr/bin/chromium \
    FERRULE_BROWSER=1 \
    FERRULE_BROWSER_CHROME_SANDBOX=0
USER 10001:10001

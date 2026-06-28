# syntax=docker/dockerfile:1.9
#
# =============================================================================
# catalerum — multi-stage build for lint / test / release image.
#
# Driven by `docker buildx bake` (docker-bake.hcl) and the GitLab pipeline
# (.gitlab-ci.yml). Stages `lint`, `test` and `build` share one source tree so
# clippy, the test suite and the release binary all compile from the same base.
# The llmleaf SDK (`llmleaf-client`) comes from crates.io like any other dep.
# All crates are pre-downloaded once in the `fetch` stage, keyed on the
# manifests only, so source edits never re-download the dependency graph.
#
# Multi-arch: every compile stage is pinned to $BUILDPLATFORM and the release
# binary is CROSS-compiled for $TARGETARCH (see `build-base`), so building
# e.g. linux/arm64 from an amd64 runner never falls back to QEMU emulation.
# Only the final `runtime` stages are target-platform. Exception: the
# `runtime-sandbox` stage RUNs target-arch apt/pip and therefore DOES need
# QEMU binfmt when baking a foreign platform (see .gitlab-ci.yml).
# =============================================================================

# ---- base toolchain ---------------------------------------------------------
# `slim-bookworm` == current stable, matching rust-toolchain.toml (channel =
# "stable") and >= the repo MSRV (1.85). Pin to e.g. rust:1.90-slim-bookworm for
# a reproducible toolchain. Debian 12 (bookworm) glibc matches the distroless
# runtime below.
FROM --platform=$BUILDPLATFORM rust:slim-bookworm AS base
# System build deps:
#   protobuf-compiler                     -> protoc, for prost-build (llmleaf-client build.rs)
#   cmake clang libclang-dev build-essential -> aws-lc-sys (rustls crypto pulled by aws-sdk-s3)
#   pkg-config git                        -> misc build scripts / cargo git fetch
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential clang cmake git libclang-dev pkg-config protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
# The image's default toolchain is version-named (e.g. 1.96.1) while
# rust-toolchain.toml pins channel = "stable" — rustup treats those as
# DIFFERENT toolchains. Copy the pin in and install its toolchain (with its
# components/targets: clippy, rustfmt, wasm32) here, once, layer-cached.
# Without this, every compile stage re-downloads "stable" on the fly and —
# fatally — build-base's per-arch `rustup target add` lands in the image
# default toolchain instead of the one cargo actually uses (E0463 can't find
# crate for `core`/`std`). All rustup/cargo calls below run under this WORKDIR
# so they resolve the same pinned toolchain.
WORKDIR /src/catalerum
COPY rust-toolchain.toml ./
RUN rustup toolchain install \
 && rustup component add clippy rustfmt \
 && rustup target add wasm32-unknown-unknown

# ---- dependency manifests -----------------------------------------------------
# A pruned copy of the build context holding ONLY what `cargo fetch` needs to
# resolve the workspace: Cargo.toml/Cargo.lock, per-crate manifests, stub
# lib/main targets, plus benches/examples sources truncated to zero bytes —
# cargo errors on explicitly declared targets whose file is missing (e.g. the
# catalerum-markdown [[bench]]), but their CONTENT is irrelevant for fetching,
# so emptying them keeps the fetch cache key stable across bench edits. If you
# ever declare a [[test]]/[[bin]]/[[example]] at a path outside src/, benches/
# or examples/, extend the keep-list below. This stage re-runs on every source
# change (cheap: find over the context); what matters is that its OUTPUT only
# changes when dependency metadata does — the `fetch` COPY below is keyed on
# that output's checksum, not on this stage's cache status.
FROM base AS manifests
WORKDIR /manifests
COPY . .
RUN find . -type f ! -name Cargo.toml ! -name Cargo.lock \
         ! -path './crates/*/benches/*' ! -path './crates/*/examples/*' -delete \
 && find . -type f ! -name Cargo.toml ! -name Cargo.lock -exec truncate -s0 {} + \
 && find . -mindepth 1 -type d -empty -delete \
 && for m in crates/*/Cargo.toml; do \
      d="${m%/Cargo.toml}/src"; mkdir -p "$d"; : > "$d/lib.rs"; : > "$d/main.rs"; \
    done

# ---- cargo fetch (dependency download) ------------------------------------------
# Downloads the full dependency graph (all targets: native, wasm32, both cross
# triples) into CARGO_HOME *in the image layer* — deliberately NOT a cache
# mount. Cache mounts never leave the builder, so on ephemeral CI runners they
# start empty and every build re-downloaded crates; this layer rides the bake
# registry cache (mode=max) instead, and only invalidates when the manifests
# stage output changes. Compile stages inherit it via FROM and need no
# crates.io network. The stub tree is removed afterwards so it can never leak
# into compile stages (a surviving stub src/main.rs would become a phantom
# [[bin]] under --all-targets); rust-toolchain.toml from `base` stays put.
FROM base AS fetch
COPY --from=manifests /manifests/ ./
RUN cargo fetch --locked \
 && rm -rf Cargo.toml Cargo.lock crates

# ---- cross toolchain for the release binary -----------------------------------
# Maps the image's target arch ($TARGETARCH, from --platform / bake `platforms`)
# to a Rust triple, and — only when actually crossing — installs Debian's GNU
# cross toolchain and records the linker / cc-crate / bindgen overrides in
# /etc/cross-env.sh for the build stage to source. TARGET_CC/CXX/AR cover the
# cc + cmake crates (aws-lc-sys); the bindgen sysroot is defensive (aws-lc-sys
# ships pregenerated bindings for these triples, so bindgen normally never runs).
FROM fetch AS build-base
ARG BUILDARCH TARGETARCH
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) RUST_TARGET=x86_64-unknown-linux-gnu;  GNU_TRIPLE=x86_64-linux-gnu; \
             CROSS_PKGS="gcc-x86-64-linux-gnu g++-x86-64-linux-gnu libc6-dev-amd64-cross" ;; \
      arm64) RUST_TARGET=aarch64-unknown-linux-gnu; GNU_TRIPLE=aarch64-linux-gnu; \
             CROSS_PKGS="gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libc6-dev-arm64-cross" ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    rustup target add "$RUST_TARGET"; \
    printf 'export CARGO_BUILD_TARGET=%s\n' "$RUST_TARGET" >/etc/cross-env.sh; \
    if [ "$BUILDARCH" != "$TARGETARCH" ]; then \
      apt-get update; \
      apt-get install -y --no-install-recommends $CROSS_PKGS; \
      rm -rf /var/lib/apt/lists/*; \
      LINKER_VAR="CARGO_TARGET_$(echo "$RUST_TARGET" | tr 'a-z-' 'A-Z_')_LINKER"; \
      { printf 'export %s=%s-gcc\n' "$LINKER_VAR" "$GNU_TRIPLE"; \
        printf 'export TARGET_CC=%s-gcc TARGET_CXX=%s-g++ TARGET_AR=%s-ar\n' \
               "$GNU_TRIPLE" "$GNU_TRIPLE" "$GNU_TRIPLE"; \
        printf 'export BINDGEN_EXTRA_CLANG_ARGS=--sysroot=/usr/%s\n' "$GNU_TRIPLE"; \
      } >>/etc/cross-env.sh; \
    fi

# ---- source tree -------------------------------------------------------------
FROM fetch AS src
WORKDIR /src/catalerum
COPY . .

# Crates come pre-downloaded from the `fetch` layer above, so the compile
# stages run without registry/git cache mounts (a mount would SHADOW the
# fetched registry at /usr/local/cargo and force a re-download on ephemeral
# runners). The target-dir mount persists incremental compile artifacts across
# builds ON A PERSISTENT BUILDER only — mounts never reach the registry cache.
# `sharing=locked` serialises concurrent stages that share the target dir.

# ---- lint -------------------------------------------------------------------
FROM src AS lint
RUN --mount=type=cache,target=/src/catalerum/target,sharing=locked \
    cargo clippy --workspace --exclude catalerum-web --all-targets -- -D warnings \
 && cargo clippy -p catalerum-web --target wasm32-unknown-unknown -- -D warnings

# ---- test -------------------------------------------------------------------
# DB-backed tests self-skip unless CATALERUM_TEST_DATABASE_URL is set. An
# isolated build has no Postgres, so those skip (honest partial coverage — this
# mirrors `just test`). For full-DB coverage run the tests against a live
# Postgres in a services: job (see the commented `test:db` in .gitlab-ci.yml).
FROM src AS test
RUN --mount=type=cache,target=/src/catalerum/target,sharing=locked \
    cargo test --workspace --exclude catalerum-web

# ---- build (release binary) -------------------------------------------------
# Runs on the build platform, emits a $TARGETARCH binary (env from build-base;
# CARGO_BUILD_TARGET is always set, so the output path is target/<triple>/…).
# The target dir is a cache mount (not in the layer), so copy the binary OUT to
# a real path in the same RUN before the mount is unmounted. Per-arch builds of
# a multi-platform bake serialise on the shared locked mounts — intentional:
# each cargo build saturates the CPU anyway, and the triple keys the target dir.
FROM build-base AS build
WORKDIR /src/catalerum
COPY . .
RUN --mount=type=cache,target=/src/catalerum/target,sharing=locked \
    . /etc/cross-env.sh \
 && cargo build --release -p catalerum \
 && mkdir -p /out && cp "target/${CARGO_BUILD_TARGET}/release/catalerum" /out/catalerum

# Native SQLite API plus the PID-1 supervisor used only by the single-container
# distribution. The normal `build` stage above remains PostgreSQL-only.
FROM build-base AS build-all-in-one
WORKDIR /src/catalerum
COPY . .
RUN --mount=type=cache,target=/src/catalerum/target,sharing=locked \
    . /etc/cross-env.sh \
 && cargo build --release -p catalerum --features sqlite \
 && cargo build --release -p catalerum-all-in-one \
 && mkdir -p /out \
 && cp "target/${CARGO_BUILD_TARGET}/release/catalerum" /out/catalerum \
 && cp "target/${CARGO_BUILD_TARGET}/release/catalerum-all-in-one" /out/catalerum-all-in-one

# ---- build (operator binary) --------------------------------------------------
# The WorkspaceSandbox operator (SOUL §20) — same cross setup and shared locked
# target-dir cache as the app build; the two binaries overlap on most of the
# dependency graph, so this is mostly a link step when `build` ran first.
FROM build-base AS build-operator
WORKDIR /src/catalerum
COPY . .
RUN --mount=type=cache,target=/src/catalerum/target,sharing=locked \
    . /etc/cross-env.sh \
 && cargo build --release -p catalerum-operator \
 && mkdir -p /out && cp "target/${CARGO_BUILD_TARGET}/release/catalerum-operator" /out/catalerum-operator

# ---- build (preview service binary) -------------------------------------------
# The standalone preview render service (SOUL §9/§10) — same cross setup and
# shared locked target-dir cache as the app/operator builds; it shares most of
# the dependency graph, so this is mostly a link step when `build` ran first.
FROM build-base AS build-preview
WORKDIR /src/catalerum
COPY . .
RUN --mount=type=cache,target=/src/catalerum/target,sharing=locked \
    . /etc/cross-env.sh \
 && cargo build --release -p catalerum-preview --bin catalerum-preview-service \
 && mkdir -p /out && cp "target/${CARGO_BUILD_TARGET}/release/catalerum-preview-service" /out/catalerum-preview-service

# ---- web build (Trunk/wasm SPA) ----------------------------------------------
# Output (wasm/js/html) is arch-independent, and nothing here reads TARGETARCH,
# so in a multi-platform bake both platforms share ONE instance of this stage.
# trunk comes as a prebuilt release binary for the build host (cargo install
# would compile it for ~10 min on every cold CI builder); trunk itself then
# fetches wasm-bindgen/wasm-opt at build time (needs network, like cargo).
FROM fetch AS web-build
ARG BUILDARCH
ARG TRUNK_VERSION=v0.21.14
ARG CATALERUM_WEB_API_BASE=""
ENV CATALERUM_WEB_API_BASE=${CATALERUM_WEB_API_BASE}
RUN set -eux; \
    case "$BUILDARCH" in \
      amd64) TRUNK_TRIPLE=x86_64-unknown-linux-gnu ;; \
      arm64) TRUNK_TRIPLE=aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported BUILDARCH: $BUILDARCH" >&2; exit 1 ;; \
    esac; \
    apt-get update; \
    apt-get install -y --no-install-recommends curl ca-certificates; \
    rm -rf /var/lib/apt/lists/*; \
    curl -fsSL "https://github.com/trunk-rs/trunk/releases/download/${TRUNK_VERSION}/trunk-${TRUNK_TRIPLE}.tar.gz" \
      | tar -xz -C /usr/local/bin trunk; \
    trunk --version
COPY . .
RUN --mount=type=cache,target=/src/catalerum/target,sharing=locked \
    --mount=type=cache,target=/root/.cache/trunk,sharing=locked \
    cd crates/catalerum-web \
 && trunk build --release \
 && mkdir -p /out && cp -r dist /out/dist

# ---- web runtime image ---------------------------------------------------------
# Static SPA behind unprivileged nginx (uid 101, listens on 8080) to satisfy the
# cluster's runAsNonRoot policy; SPA fallback + caching live in nginx.conf.
FROM docker.io/nginxinc/nginx-unprivileged:stable-alpine AS runtime-web
COPY crates/catalerum-web/nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=web-build /out/dist /usr/share/nginx/html
EXPOSE 8080

# ---- runtime image ----------------------------------------------------------
# Distroless + :nonroot (uid 65532) to satisfy deploy/catalerum.yaml
# (runAsNonRoot, readOnlyRootFilesystem, drop ALL caps). cc-debian12 ships glibc
# + CA certs; the binary needs nothing else.
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
COPY --from=build /out/catalerum /usr/local/bin/catalerum
# Bake the default config so `docker run` works out of the box. In k8s the
# ConfigMap is mounted over /etc/catalerum (read-only) and secrets arrive via
# CATALERUM_-prefixed env — both override this baked copy.
COPY --from=build /src/catalerum/config/catalerum.toml /etc/catalerum/catalerum.toml
ENV CATALERUM_CONFIG=/etc/catalerum/catalerum.toml
EXPOSE 8787
# No subcommand -> run the Axum API + workers (SOUL §16).
ENTRYPOINT ["/usr/local/bin/catalerum"]

# ---- operator runtime image ---------------------------------------------------
# Same distroless base as the API image: the WorkspaceSandbox operator (SOUL §20)
# runs non-root with a RO rootfs and needs only glibc + CA certs (kube-rs TLS to
# the apiserver). `run` watches CRs via the in-cluster service account.
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime-operator
COPY --from=build-operator /out/catalerum-operator /usr/local/bin/catalerum-operator
ENTRYPOINT ["/usr/local/bin/catalerum-operator", "run"]

# ---- preview render service image ---------------------------------------------
# The standalone preview service (SOUL §9/§10): its own SLIM image carrying ONLY
# the render toolchain — headless LibreOffice (any office format → pdf), poppler
# (pdfinfo/pdftoppm) + fonts — plus the cross-built Rust binary. UNLIKE the app /
# operator images this is NOT distroless (it shells those binaries), and — like
# runtime-sandbox — its apt layer RUNs target-arch, so a foreign-platform bake
# needs QEMU binfmt on the builder (CI registers it before baking `preview`).
#
# Runs non-root (uid 1000) with a RO rootfs in k8s; LibreOffice keeps ALL state
# in a writable /tmp (HOME=/tmp + a per-render UserInstallation profile there),
# so mount an emptyDir at /tmp. Listens on 8790.
FROM docker.io/library/debian:bookworm-slim AS runtime-preview
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        libreoffice-nogui \
        poppler-utils \
        fonts-dejavu fonts-liberation \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build-preview /out/catalerum-preview-service /usr/local/bin/catalerum-preview-service
RUN groupadd -g 1000 preview \
 && useradd -u 1000 -g 1000 -s /usr/sbin/nologin preview
# HOME on the writable tmpfs so LibreOffice never needs the RO rootfs.
ENV HOME=/tmp \
    TMPDIR=/tmp \
    PREVIEW_BIND=0.0.0.0:8790
USER 1000:1000
EXPOSE 8790
ENTRYPOINT ["/usr/local/bin/catalerum-preview-service"]

# ---- workspace sandbox image ----------------------------------------------------
# The batteries-included image workspace sandboxes run (SOUL §20) instead of the
# bare debian:stable-slim fallback: point [exec.k8s].image / [exec.podman].image
# (or spec.image on a WorkspaceSandbox CR) at catalerum/catalerum-sandbox.
#
# UNLIKE every other stage this one is NOT cross-compiled: apt/pip below RUN
# target-arch binaries, so baking a foreign platform needs QEMU binfmt on the
# builder (CI registers it via tonistiigi/binfmt before baking `sandbox`).
#
# Runs as uid:gid 1000 to match the operator's `fsGroup: 1000` on the /work PVC
# (resources.rs); sandboxes drop ALL caps anyway, so root would buy nothing but
# blast radius. Runtime `pip install --user` still works (~/.local on PATH).
FROM docker.io/library/python:3.12-slim-bookworm AS runtime-sandbox
# Everyday CLI tools an agent shells out to; deliberately no compiler toolchain —
# the pip layer below is wheels-only, and sandboxes install extras via
# `pip install --user`, not apt (they run non-root).
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git jq less openssh-client openssl procps \
        ripgrep sqlite3 unzip wget xz-utils zip \
    && rm -rf /var/lib/apt/lists/*
# Document toolchain (own layer — it's the heavy one, keep the tools layer
# cache-stable): headless LibreOffice (any office format → pdf/txt/…), pandoc,
# enough TeX Live for `pandoc -o out.pdf` (xelatex), poppler (pdftotext/
# pdftoppm, pdf2image backend), qpdf + ghostscript (PDF decrypt/repair/
# transform), libmagic (python-magic). Fonts so LibreOffice/TeX render PDFs
# with something better than metric fallbacks.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libreoffice-nogui \
        pandoc \
        texlive-latex-base texlive-latex-recommended texlive-latex-extra \
        texlive-fonts-recommended texlive-xetex lmodern \
        poppler-utils qpdf ghostscript libmagic1 \
        fonts-dejavu fonts-liberation \
    && rm -rf /var/lib/apt/lists/*
# The Python stack: data analysis, plotting, documents/office formats (incl.
# encrypted PDFs — pypdf[crypto]/pikepdf/pymupdf — and password-protected
# office files — msoffcrypto-tool), HTTP + HTML parsing, databases, misc
# utils. `--only-binary=:all:` keeps the build compiler-free and fails LOUDLY
# if a package stops shipping wheels for an arch (instead of silently
# attempting a source build under QEMU). Unpinned on purpose: each rebuild
# picks up current wheels; the immutable SHA tag pins a deployment to a
# known-good set.
RUN pip install --no-cache-dir --only-binary=:all: \
        numpy pandas polars scipy sympy statsmodels scikit-learn duckdb \
        matplotlib seaborn plotly \
        openpyxl xlsxwriter python-docx python-pptx reportlab \
        "pypdf[crypto]" pdfplumber pymupdf pikepdf pdf2image \
        cryptography msoffcrypto-tool \
        pillow tabulate markdown html2text striprtf chardet python-magic \
        requests httpx beautifulsoup4 lxml html5lib \
        sqlalchemy "psycopg[binary]" \
        pyyaml orjson python-dateutil tqdm regex jsonschema rich
# Pure-python packages that only publish sdists (no wheel → they'd fail the
# wheels-only gate above; building them needs no compiler, just setuptools).
RUN pip install --no-cache-dir odfpy ebooklib
RUN groupadd -g 1000 sandbox \
 && useradd -m -u 1000 -g 1000 -s /bin/bash sandbox \
 && mkdir -p /work && chown 1000:1000 /work
ENV HOME=/home/sandbox \
    PATH=/home/sandbox/.local/bin:$PATH
USER 1000:1000
WORKDIR /work
# The operator/podman backends always pass the keep-alive command explicitly;
# this default just makes a bare `docker run` behave the same way.
CMD ["tail", "-f", "/dev/null"]

# ---- all-in-one image ---------------------------------------------------------
FROM ghcr.io/codefionn/llmleaf:0.2.5 AS llmleaf-upstream
FROM docker.io/qdrant/qdrant:latest AS qdrant-upstream

FROM runtime-sandbox AS runtime-all-in-one
USER root
RUN apt-get update && apt-get install -y --no-install-recommends libunwind8 nginx tesseract-ocr \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /data/qdrant /files /work /tmp/catalerum /var/lib/nginx \
 && chown -R 1000:1000 /data /files /work /tmp/catalerum /var/lib/nginx
COPY --from=build-all-in-one /out/catalerum /usr/local/bin/catalerum
COPY --from=build-all-in-one /out/catalerum-all-in-one /usr/local/bin/catalerum-all-in-one
COPY --from=build-preview /out/catalerum-preview-service /usr/local/bin/catalerum-preview-service
COPY --from=llmleaf-upstream /usr/local/bin/llmleaf /usr/local/bin/llmleaf
COPY --from=qdrant-upstream /qdrant /qdrant
COPY --from=web-build /out/dist /usr/share/catalerum-web
COPY config/all-in-one.toml /etc/catalerum/all-in-one.toml
COPY config/llmleaf.all-in-one.toml /etc/catalerum/llmleaf.toml
COPY config/qdrant.all-in-one.yaml /etc/catalerum/qdrant.yaml
COPY config/nginx.all-in-one.conf /etc/nginx/nginx.conf
ENV HOME=/home/sandbox \
    TMPDIR=/tmp \
    PREVIEW_BIND=127.0.0.1:8790 \
    CATALERUM_CONFIG=/etc/catalerum/all-in-one.toml
USER 1000:1000
VOLUME ["/data", "/files", "/work"]
EXPOSE 8080
HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=5 \
  CMD curl -fsS http://127.0.0.1:8080/api/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/catalerum-all-in-one"]

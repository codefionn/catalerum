# catalerum — single developer entrypoint (SOUL §17).
# Humans and agents run the identical recipes; no hidden scripts.

set shell := ["bash", "-uc"]

# Container engine + compose command. We prefer podman and fall back to docker;
# both speak the same `compose`/`inspect` surface this justfile relies on. Override
# CONTAINER_ENGINE (e.g. "docker") or COMPOSE (e.g. "podman-compose") to force one.
export CONTAINER_ENGINE := env_var_or_default("CONTAINER_ENGINE", `command -v podman >/dev/null 2>&1 && echo podman || echo docker`)
export COMPOSE := env_var_or_default("COMPOSE", CONTAINER_ENGINE + " compose")

# Connection strings for local dev (match docker-compose.yml and config/catalerum.toml).
export DATABASE_URL := env_var_or_default("DATABASE_URL", "postgres://catalerum:catalerum@localhost:5432/catalerum")
export VALKEY_URL := env_var_or_default("VALKEY_URL", "redis://localhost:6379")
export NEO4J_URL := env_var_or_default("NEO4J_URL", "http://localhost:7474")
export QDRANT_URL := env_var_or_default("QDRANT_URL", "http://localhost:6333")
export CATALERUM_CONFIG := env_var_or_default("CATALERUM_CONFIG", "config/catalerum.toml")

# The llmleaf proxy now runs as a published container (quay.io/codefionn/llmleaf)
# defined as the `llmleaf` service in docker-compose.yml — `just up` brings it up
# alongside the backing stores, so no sibling checkout is needed. Override
# LLMLEAF_IMAGE to pin a different image/tag. LLMLEAF_CONFIG (the host file
# bind-mounted into the container at /etc/llmleaf/llmleaf.toml) defaults to
# config/llmleaf.prod.toml when that file exists (your real-provider config,
# gitignored), otherwise the committed config/llmleaf.dev.toml — so a fresh
# checkout and CI fall back to the key-free deterministic "echo" route with no
# setup, while `prod.toml` (when present) takes over automatically. An explicit
# LLMLEAF_CONFIG env var always wins. The config listens on 0.0.0.0:8088
# (catalerum's [llm].base_url); the dev config exposes the `catalerum-dev` consumer
# key whose base64("id:password") catalerum sends as its bearer.
export LLMLEAF_IMAGE := env_var_or_default("LLMLEAF_IMAGE", "quay.io/codefionn/llmleaf:0.2.5")
export LLMLEAF_CONFIG := env_var_or_default("LLMLEAF_CONFIG", `test -f config/llmleaf.prod.toml && echo config/llmleaf.prod.toml || echo config/llmleaf.dev.toml`)

# Default: list recipes.
default:
    @just --list

# Start backing services (Postgres + Neo4j + Valkey + Qdrant + RustFS/S3 + the
# llmleaf proxy container) and wait for the stores' health. llmleaf is a scratch
# image with no shell, so it carries no healthcheck and isn't part of the wait —
# it binds :8088 near-instantly.
up:
    {{COMPOSE}} up -d
    @echo "waiting for postgres + valkey + neo4j + qdrant + rustfs to report healthy…"
    @# The Go-template braces are produced via just's literal interpolations
    @# (quoted brace pairs), because the previous quadruple-brace spelling
    @# rendered a malformed template - the until-loop could never succeed and
    @# every run burned the full 120 s before WARNing despite healthy services.
    @timeout 120 bash -c 'hs() { {{CONTAINER_ENGINE}} inspect -f "{{"{{"}}.State.Health.Status{{"}}"}}" "$1" 2>/dev/null; }; until [ "$(hs catalerum-postgres)" = healthy ] && [ "$(hs catalerum-valkey)" = healthy ] && [ "$(hs catalerum-neo4j)" = healthy ] && [ "$(hs catalerum-qdrant)" = healthy ] && [ "$(hs catalerum-rustfs)" = healthy ]; do sleep 2; done' \
      && echo "backing services are healthy" \
      || echo "WARN: services did not all report healthy in time (check '{{COMPOSE}} ps')"

# Stop and remove the backing services.
down:
    {{COMPOSE}} down

# The `catalerum` binary applies the store + IAM migrations automatically on
# boot (see crates/catalerum/src/main.rs); there is no `migrate` subcommand.
# Booting the API is the migration path; this recipe documents it.

# Apply DB migrations (the binary migrates store + IAM on boot; no subcommand).
migrate:
    @echo "migrations are applied automatically when the catalerum binary boots."
    @echo "run 'just dev' (or 'cargo run -p catalerum') and the store + IAM schemas are migrated on startup."

# Seeding (admin user + default workspace + a one-time magic-link login token)
# happens automatically on boot when [auth].dev_login = true (the default).
# There is no separate `seed` subcommand in M1.

# Seed admin + default workspace + magic-link (done automatically on boot).
seed:
    @echo "the admin + default workspace + dev magic-link are seeded automatically on boot"
    @echo "(config [auth].dev_login = true). The magic-link URL is printed by 'just dev'."

# Run the llmleaf proxy container (quay.io image) with the bundled key-free echo
# config so chat works offline (SOUL §7, §17). Serves the OpenAI-compatible
# consumer endpoint at http://localhost:8088/v1 (POST /v1/chat/completions), model
# "echo", bearer base64("catalerum-dev:dev-echo-key") — matching
# config/catalerum.toml [llm]. `just up` already starts this service; this recipe
# runs it standalone in the foreground (attached, so you see its logs) — Ctrl-C
# stops it.

# Run the llmleaf echo proxy container on :8088 (standalone, attached; `just up` starts it).
llmleaf:
    @echo "starting llmleaf ({{LLMLEAF_IMAGE}}) on http://localhost:8088 from {{LLMLEAF_CONFIG}}…"
    {{COMPOSE}} up llmleaf

# Boots the API, which migrates the DB, seeds the dev admin, and prints a
# magic-link login URL. The echo llmleaf is already up as a compose service (via
# `dev: up`) so chat works out of the box; it stays up until `just down`. Run
# `just web` in another terminal for the workbench. Opening the printed magic-link
# signs the web UI in with one click (the API 302-redirects to the SPA with the
# session token).
#
# The API runs under cargo-watch: editing any native crate rebuilds and restarts
# it on save, so a newly added route/handler is live without a manual restart.
# The wasm web crate is ignored (it has its own trunk reload via `just web`), as
# are markdown docs. Without cargo-watch installed, `dev` boots the API once and
# points you at `just install-dev-tools`.
# `dev` also exports one random bearer for the whole recipe invocation, so
# cargo-watch restarts keep accepting the same dev Authorization token.

# Boot the API under watch (rebuild + restart on save); llmleaf comes up via `up`.
dev: up
    #!/usr/bin/env bash
    set -uo pipefail
    # `up` already started the Qdrant + Neo4j containers; light up the derived
    # stores for dev so the vector index + graph projection workers run (and the
    # boot "Backing services" summary shows them as up). Scoped to `just dev` via
    # env overrides — the committed config stays default-off (see config/catalerum.toml).
    export CATALERUM_QDRANT__ENABLED="${CATALERUM_QDRANT__ENABLED:-true}"
    export CATALERUM_NEO4J__ENABLED="${CATALERUM_NEO4J__ENABLED:-true}"
    # Use the RustFS container `up` started as the dev S3 object-storage backend
    # (S3 creds set → S3 wins over local-FS in [storage]). Bucket is auto-created
    # on boot. Scoped to `just dev`; the committed config carries no S3 creds.
    export CATALERUM_STORAGE__S3__ENDPOINT="${CATALERUM_STORAGE__S3__ENDPOINT:-http://localhost:9000}"
    export CATALERUM_STORAGE__S3__ACCESS_KEY="${CATALERUM_STORAGE__S3__ACCESS_KEY:-rustfsadmin}"
    export CATALERUM_STORAGE__S3__SECRET_KEY="${CATALERUM_STORAGE__S3__SECRET_KEY:-rustfsadmin}"
    export CATALERUM_STORAGE__S3__PATH_STYLE="${CATALERUM_STORAGE__S3__PATH_STYLE:-true}"
    export CATALERUM_STORAGE__BUCKET="${CATALERUM_STORAGE__BUCKET:-catalerum}"
    # Encrypted secret store (SOUL §13): a FIXED dev master key (base64 of 32 bytes)
    # so external-DB credentials stay decryptable across cargo-watch restarts. Dev
    # only — never use this key outside local dev. Scoped to `just dev`; the
    # committed config leaves [secrets].master_key empty (feature off by default).
    export CATALERUM_SECRETS__MASTER_KEY="${CATALERUM_SECRETS__MASTER_KEY:-Y2F0YWxlcnVtLWRldi1tYXN0ZXIta2V5LTAxMjM0NTY=}"
    # Default terminal/exec backend (SOUL §20): the scrubbed-env host sandbox, so
    # a new terminal doesn't inherit the operator's environment (dev tokens, S3
    # creds, master key exported above). A workdir can still pin another backend.
    # Scoped to `just dev`; the committed config leaves [exec].backend empty (local).
    export CATALERUM_EXEC__BACKEND="${CATALERUM_EXEC__BACKEND:-sandbox}"
    if [ -z "${CATALERUM_DEV_AUTHORIZATION_TOKEN:-}" ]; then
      if ! command -v openssl >/dev/null 2>&1; then
        echo "ERROR: openssl is required to generate CATALERUM_DEV_AUTHORIZATION_TOKEN" >&2
        exit 1
      fi
      export CATALERUM_DEV_AUTHORIZATION_TOKEN="$(openssl rand -hex 32)"
    fi
    echo "llmleaf (echo) is up as a compose service on http://localhost:8088 (via 'just up')…"
    echo "booting catalerum API (migrates + seeds + prints the magic-link URL)…"
    echo "using stable dev authorization token from CATALERUM_DEV_AUTHORIZATION_TOKEN"
    echo "in another terminal run 'just web' (trunk serve) to start the workbench on http://localhost:8080"
    echo "open the magic-link URL the API prints below — it redirects you straight into the signed-in UI"
    # Register a demo EXTERNAL Postgres connection (SOUL §11) so the external-DB +
    # sql_query + schema-migration features work out of the box. It points at a
    # dedicated `catalerum_external` database on the same dev Postgres server, kept
    # separate from catalerum's own truth DB. Backgrounded: waits for the API (which
    # boots + migrates + seeds first), ensures the database exists, then creates the
    # `dev-postgres` connection via the API if it isn't already present (idempotent).
    (
      api="http://localhost:8787"
      auth="Authorization: Bearer ${CATALERUM_DEV_AUTHORIZATION_TOKEN}"
      # A cold `cargo run` build takes many minutes — wait generously (up to
      # 15 min) and bail with an honest message instead of falling through into
      # a doomed registration attempt (the old 60 s loop WARNed on every fresh
      # build even though nothing was wrong).
      api_up=""
      for _ in $(seq 1 450); do
        if curl -sf "$api/healthz" >/dev/null 2>&1; then api_up=1; break; fi
        sleep 2
      done
      if [ -z "$api_up" ]; then
        echo "NOTE: API not up after 15 min — skipped registering the dev external Postgres connection (it registers on the next 'just dev')"
        exit 0
      fi
      {{CONTAINER_ENGINE}} exec catalerum-postgres psql -U catalerum -tc \
        "SELECT 1 FROM pg_database WHERE datname='catalerum_external'" 2>/dev/null | grep -q 1 \
        || {{CONTAINER_ENGINE}} exec catalerum-postgres psql -U catalerum -c \
             "CREATE DATABASE catalerum_external" >/dev/null 2>&1 || true
      if curl -sf -H "$auth" "$api/db/connections" 2>/dev/null | grep -q '"name":"dev-postgres"'; then
        echo "dev external Postgres connection 'dev-postgres' already present"
      elif curl -sf -X POST "$api/db/connections" -H "$auth" -H 'Content-Type: application/json' \
             -d '{"name":"dev-postgres","host":"localhost","port":5432,"database":"catalerum_external","username":"catalerum","password":"catalerum"}' \
             >/dev/null 2>&1; then
        echo "registered dev external Postgres connection 'dev-postgres' -> catalerum_external (use it from sql_query / the SqlQuery automation action)"
      else
        echo "WARN: could not register the dev external Postgres connection (API is up — check the API logs / [secrets].master_key)"
      fi
    ) &
    if command -v cargo-watch >/dev/null 2>&1; then
      echo "watching native crates — the API rebuilds + restarts on save (cargo-watch)…"
      cargo watch --why -i 'crates/catalerum-web/**' -i '**/*.md' -x 'run -p catalerum'
    else
      echo "WARN: cargo-watch not installed — booting the API once, without rebuild-on-save."
      echo "      run 'just install-dev-tools' (cargo install cargo-watch) to enable auto-reload."
      cargo run -p catalerum
    fi

# Install optional dev tooling: cargo-watch, for `just dev`'s rebuild-on-save.
install-dev-tools:
    cargo install cargo-watch --locked

# Serve the Leptos web workbench (Trunk, http://localhost:8080); run beside `just dev`.
web:
    cd crates/catalerum-web && trunk serve

# cargo test across the native workspace (web is wasm; tested separately).
test:
    cargo test --workspace --exclude catalerum-web

# Boot the stack (backing services + the echo llmleaf container) and run the
# Playwright specs against the deterministic echo-LLM as a real, seeded, logged-in
# session (SOUL §17). SELF-CONTAINED: no `just dev`/`just web` and no exported
# token needed — the recipe mints its own dev session, boots an ephemeral API on
# :8787, reuses (or boots) the web workbench, then tears down what it started.
#
# Session token (mirrors `just dev`). `CATALERUM_DEV_AUTHORIZATION_TOKEN` alone is
# a valid, reusable 365-day session: the booted API turns it into one via
# `IamService::ensure_dev_authorization_token` (crates/catalerum/src/main.rs), so
# Playwright uses the SAME value as the `Authorization: Bearer` and the SPA's
# `?token=`. We generate it with `openssl rand -hex 32` (exactly as `dev` does).
# We ALSO scrape the one-time magic-link token the API prints on boot
# (`…/auth/magic?token=<TOKEN>`) into `CATALERUM_MAGIC_TOKEN`, best-effort, so the
# login-and-chat spec runs live too; if the scrape fails, that single spec
# self-skips (skip-not-green is honest, §17).
#
# Ports & teardown. The compiled SPA calls the API at an ABSOLUTE, hardcoded
# http://localhost:8787 (crates/catalerum-web/src/api.rs `API_BASE`; the API's CORS
# is permissive), so the API MUST live on :8787 — an alternate port would need a
# wasm rebuild. Therefore: if :8787 is ALREADY serving (a `just dev` session) we do
# not disturb it — we reuse it when its token is exported, else we stop with a clear
# message. When :8787 is free we boot our own API there and trap-kill it on exit.
# The web workbench (:8080) is reused if already serving, else `trunk serve` is
# booted here and torn down too. Backing containers (`up`) are shared, left running.
#
# Runner. Playwright executes from the local e2e/node_modules by default;
# `just e2e-docker` runs the IDENTICAL orchestration but executes Playwright
# inside the pinned mcr.microsoft.com/playwright container on the host network
# (docker-compose.playwright.yml) — no local node/npx/browsers needed.

# Boot stack (echo llmleaf) + self-minted session, boot API+web, run Playwright, tear down.
e2e runner="local": up
    #!/usr/bin/env bash
    set -euo pipefail

    api="http://localhost:8787"
    web="http://localhost:8080"
    log_dir="$(mktemp -d)"
    api_log="$log_dir/api.log"
    web_log="$log_dir/web.log"
    api_pid=""
    web_pid=""

    cleanup() {
      local code=$?
      [ -n "$api_pid" ] && kill "$api_pid" 2>/dev/null || true
      [ -n "$web_pid" ] && kill "$web_pid" 2>/dev/null || true
      if [ "$code" -ne 0 ]; then
        echo "e2e failed (exit $code). Logs: API=$api_log web=$web_log" >&2
      fi
    }
    trap cleanup EXIT

    # --- Web workbench (:8080) ------------------------------------------------
    # Reuse a running `just web`; otherwise boot our own `trunk serve` (torn down
    # on exit). The SPA is static (built from source), independent of the API.
    if curl -sf -o /dev/null "$web" 2>/dev/null; then
      echo "reusing the web workbench already serving on $web"
    else
      if ! command -v trunk >/dev/null 2>&1; then
        echo "ERROR: no web workbench on $web and trunk isn't installed to boot one — run 'just web' (or 'cargo install trunk')" >&2
        exit 1
      fi
      echo "booting the web workbench (trunk serve) on $web…"
      ( cd crates/catalerum-web && trunk serve ) >"$web_log" 2>&1 &
      web_pid=$!
      for _ in $(seq 1 150); do
        curl -sf -o /dev/null "$web" 2>/dev/null && break
        kill -0 "$web_pid" 2>/dev/null || { echo "ERROR: trunk serve exited during boot — see $web_log" >&2; exit 1; }
        sleep 2
      done
      curl -sf -o /dev/null "$web" 2>/dev/null || { echo "ERROR: web workbench not up on $web after 5 min — see $web_log" >&2; exit 1; }
      echo "web workbench is up on $web"
    fi

    # --- API (:8787) with a self-minted dev session --------------------------
    if curl -sf -o /dev/null "$api/healthz" 2>/dev/null; then
      # An API is already listening — likely a `just dev` session. We won't
      # disturb it, and we can't inject a token into a process we didn't start.
      if [ -n "${CATALERUM_DEV_AUTHORIZATION_TOKEN:-}" ]; then
        echo "reusing the API already serving on $api with the exported CATALERUM_DEV_AUTHORIZATION_TOKEN"
      else
        echo "ERROR: an API is already serving on $api (a 'just dev' session?)." >&2
        echo "       'just e2e' won't disturb it. Either:" >&2
        echo "         - export that session's CATALERUM_DEV_AUTHORIZATION_TOKEN, then re-run 'just e2e' (runs against it); or" >&2
        echo "         - stop it (Ctrl-C the 'just dev'), then re-run 'just e2e' (boots its own ephemeral API)." >&2
        exit 1
      fi
    else
      # :8787 is free — boot our own ephemeral API with a freshly minted token.
      if [ -z "${CATALERUM_DEV_AUTHORIZATION_TOKEN:-}" ]; then
        command -v openssl >/dev/null 2>&1 || { echo "ERROR: openssl is required to mint CATALERUM_DEV_AUTHORIZATION_TOKEN" >&2; exit 1; }
        export CATALERUM_DEV_AUTHORIZATION_TOKEN="$(openssl rand -hex 32)"
      fi
      echo "building the catalerum API (so boot is fast)…"
      cargo build -p catalerum
      [ -x target/debug/catalerum ] || { echo "ERROR: target/debug/catalerum missing after build" >&2; exit 1; }
      echo "booting an ephemeral catalerum API on $api (echo llmleaf on :8088; migrates + seeds the dev session)…"
      target/debug/catalerum >"$api_log" 2>&1 &
      api_pid=$!
      for _ in $(seq 1 180); do
        curl -sf -o /dev/null "$api/healthz" 2>/dev/null && break
        kill -0 "$api_pid" 2>/dev/null || { echo "ERROR: the API exited during boot — see $api_log" >&2; exit 1; }
        sleep 1
      done
      curl -sf -o /dev/null "$api/healthz" 2>/dev/null || { echo "ERROR: the API did not become healthy on $api within 3 min — see $api_log" >&2; exit 1; }
      echo "API healthy on $api (self-minted dev session token exported)"
      # Best-effort: scrape the one-time magic-link token the API printed on boot so
      # the login-and-chat spec runs live; on failure it self-skips (honest, §17).
      if [ -z "${CATALERUM_MAGIC_TOKEN:-}" ]; then
        magic="$(grep -oE 'auth/magic\?token=[A-Za-z0-9_.:-]+' "$api_log" 2>/dev/null | head -1 | sed 's/.*token=//' || true)"
        if [ -n "$magic" ]; then
          export CATALERUM_MAGIC_TOKEN="$magic"
          echo "captured the dev magic-link token → the login-and-chat spec runs live"
        else
          echo "NOTE: could not scrape the magic-link token from the boot log — the login-and-chat spec self-skips"
        fi
      fi
    fi

    # --- Playwright ----------------------------------------------------------
    # A spec failure or "no tests" exits non-zero (SOUL §17); with the session
    # exported above, the session-gated specs run live instead of self-skipping.
    if [ "{{runner}}" = "container" ]; then
      # Containerized runner (docker-compose.playwright.yml): the pinned
      # Playwright image joins the HOST network — `localhost` inside the
      # container is the host, so the browser reaches the SPA's hardcoded
      # http://localhost:8787 API and the :8080 workbench unchanged. The
      # exported CATALERUM_* session vars pass through into the container.
      echo "running Playwright in the pinned container (docker-compose.playwright.yml, host network)…"
      {{COMPOSE}} -f docker-compose.playwright.yml run --rm playwright
    else
      # Local runner: run from e2e/ so npx resolves the LOCAL @playwright/test
      # (a root-level npx fetches a mismatched playwright that finds zero tests).
      cd e2e
      [ -d node_modules ] || npm install --no-package-lock
      npx playwright test
    fi

# `just e2e`, but Playwright runs inside the pinned mcr.microsoft.com/playwright
# container from docker-compose.playwright.yml — no local node/npx/browsers
# needed. Uses CONTAINER_ENGINE like everything else (podman preferred, docker
# fallback). The image tag and the exact e2e/package.json @playwright/test pin
# must move together (browsers are baked into the image per-version).
e2e-docker: (e2e "container")

# Build the all-in-one image, boot it with fresh persistent volumes, exercise
# first-boot and administration through its sole public port, then remove every
# test resource. Override the image/port or skip the build when iterating:
#   CATALERUM_ALL_IN_ONE_IMAGE=catalerum-all-in-one:test \
#   CATALERUM_ALL_IN_ONE_SKIP_BUILD=1 just e2e-all-in-one
e2e-all-in-one runner="local":
    #!/usr/bin/env bash
    set -euo pipefail

    image="${CATALERUM_ALL_IN_ONE_IMAGE:-catalerum-all-in-one:e2e}"
    port="${CATALERUM_ALL_IN_ONE_PORT:-18080}"
    run_id="$(date +%s)-$$"
    container="catalerum-all-in-one-e2e-$run_id"
    data_volume="catalerum-aio-e2e-data-$run_id"
    files_volume="catalerum-aio-e2e-files-$run_id"
    work_volume="catalerum-aio-e2e-work-$run_id"
    log_file="$(mktemp -t catalerum-all-in-one-e2e.XXXXXX.log)"
    export CATALERUM_ALL_IN_ONE_URL="${CATALERUM_ALL_IN_ONE_URL:-http://127.0.0.1:$port}"

    cleanup() {
      local code=$?
      if [ "$code" -ne 0 ]; then
        echo "all-in-one e2e failed (exit $code); container log: $log_file" >&2
        {{CONTAINER_ENGINE}} logs "$container" >"$log_file" 2>&1 || true
        tail -n 120 "$log_file" >&2 || true
      else
        rm -f "$log_file"
      fi
      {{CONTAINER_ENGINE}} rm -f "$container" >/dev/null 2>&1 || true
      {{CONTAINER_ENGINE}} volume rm "$data_volume" "$files_volume" "$work_volume" >/dev/null 2>&1 || true
    }
    trap cleanup EXIT

    if [ "${CATALERUM_ALL_IN_ONE_SKIP_BUILD:-0}" != "1" ]; then
      echo "building $image from the runtime-all-in-one target…"
      {{CONTAINER_ENGINE}} build \
        --target runtime-all-in-one \
        --build-arg CATALERUM_WEB_API_BASE=/api \
        -t "$image" .
    else
      {{CONTAINER_ENGINE}} image inspect "$image" >/dev/null
      echo "reusing $image (CATALERUM_ALL_IN_ONE_SKIP_BUILD=1)"
    fi

    {{CONTAINER_ENGINE}} volume create "$data_volume" >/dev/null
    {{CONTAINER_ENGINE}} volume create "$files_volume" >/dev/null
    {{CONTAINER_ENGINE}} volume create "$work_volume" >/dev/null
    {{CONTAINER_ENGINE}} run -d \
      --name "$container" \
      -p "127.0.0.1:$port:8080" \
      -v "$data_volume:/data" \
      -v "$files_volume:/files" \
      -v "$work_volume:/work" \
      "$image" >/dev/null

    echo "waiting for the fresh all-in-one container on $CATALERUM_ALL_IN_ONE_URL…"
    for _ in $(seq 1 180); do
      curl -fsS "$CATALERUM_ALL_IN_ONE_URL/api/healthz" >/dev/null 2>&1 && break
      {{CONTAINER_ENGINE}} inspect "$container" >/dev/null 2>&1 || {
        echo "ERROR: all-in-one container exited during boot" >&2
        exit 1
      }
      sleep 1
    done
    curl -fsS "$CATALERUM_ALL_IN_ONE_URL/api/healthz" >/dev/null || {
      echo "ERROR: all-in-one container did not become healthy within 3 minutes" >&2
      exit 1
    }

    if [ "{{runner}}" = "container" ]; then
      export PLAYWRIGHT_CONFIG=playwright.all-in-one.config.ts
      {{COMPOSE}} -f docker-compose.playwright.yml run --rm playwright
    else
      cd e2e
      [ -d node_modules ] || npm install --no-package-lock
      npx playwright test --config playwright.all-in-one.config.ts
    fi

# Run the same all-in-one suite with Chromium from the pinned Playwright image.
e2e-all-in-one-docker: (e2e-all-in-one "container")

# Run the external MCP server over stdio (SOUL §26): exposes catalerum's scoped
# tool registry to MCP clients (Claude Code / Codex / opencode). JSON-RPC on
# stdout, logs on stderr; dev-login scopes it to the default workspace's owner.
# Point an MCP client's stdio transport at: `just mcp` (or `catalerum mcp`).
mcp:
    cargo run -p catalerum -- mcp

# Take a one-off disaster-recovery backup now (SOUL §30): dump Postgres + copy the
# object blobs to [backup.destination] (S3 / WebDAV / local), then prune to
# [backup].keep. Needs a destination configured (see config/catalerum.toml).
backup:
    cargo run -p catalerum -- backup

# Restore from a backup (SOUL §30). `just restore` lists available backups;
# `just restore id=<BACKUP_ID>` performs the DESTRUCTIVE restore (replaces Postgres
# + blobs). Append `force=--force` to override the schema-version guard.
restore id="" force="":
    cargo run -p catalerum -- restore {{id}} {{ if id != "" { "--yes" } else { "" } }} {{force}}

# Wipe and recreate the dev stack from scratch (drops all volumes).
reset:
    {{COMPOSE}} down -v
    just up

# Format the whole workspace.
fmt:
    cargo fmt --all

# Lint with clippy (native crates + the wasm web crate).
lint:
    cargo clippy --workspace --exclude catalerum-web --all-targets -- -D warnings
    cargo clippy -p catalerum-web --target wasm32-unknown-unknown -- -D warnings

# Compile-check the native workspace (web is wasm; checked separately).
check:
    cargo check --workspace --exclude catalerum-web

# Compile-check the wasm web workbench.
check-web:
    cargo check -p catalerum-web --target wasm32-unknown-unknown

# Regenerate the committed WorkspaceSandbox CRD manifest from the Rust types
# (SOUL §20). Run after changing crates/catalerum-k8s/src/crd.rs.
crd:
    cargo run -p catalerum-operator -- crd > deploy/crd/catalerum.dev_workspacesandboxes.yaml

# Build the operator container image (workspace root is the build context).
operator-image tag="catalerum-operator:dev":
    {{CONTAINER_ENGINE}} build -f crates/catalerum-operator/Dockerfile -t {{tag}} .

# Build the batteries-included workspace sandbox image (Dockerfile
# `runtime-sandbox` stage: python3 + analysis/document/PDF pip stack,
# LibreOffice/pandoc/TeX Live/poppler/qpdf, openssl + CLI tools).
# Host-arch only; CI bakes the multi-arch manifest (docker-bake.hcl `sandbox`).
sandbox-image tag="catalerum-sandbox:dev":
    {{CONTAINER_ENGINE}} build --target runtime-sandbox -t {{tag}} .

# Build the standalone preview render service image (Dockerfile `runtime-preview`
# stage: slim LibreOffice + poppler + the Rust binary). Host-arch only; CI bakes
# the multi-arch manifest (docker-bake.hcl `preview`).
preview-image tag="catalerum-preview:dev":
    {{CONTAINER_ENGINE}} build --target runtime-preview -t {{tag}} .

# Run the preview render service locally on :8790 (needs libreoffice + poppler on
# PATH). Point the API at it with [preview].service_url = "http://localhost:8790".
preview:
    PREVIEW_BIND=0.0.0.0:8790 cargo run -p catalerum-preview --bin catalerum-preview-service

# Run the operator out-of-cluster against your current kubeconfig (dev loop).
operator-dev:
    cargo run -p catalerum-operator -- run

# Install the CRD + operator + API RBAC into the current cluster (kind/minikube).
operator-install:
    kubectl apply -k deploy/

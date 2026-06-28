# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

catalerum: self-hostable LLM assistant — a catalogue (calendars, email, files, notes, tasks, memories) that LLM automations query and act over.

**SOUL.md is the constitution.** Design tiebreaker; code contradicting it is wrong. Update its §31 progress log after each verified chunk.

**Always work on `main`** — never create or switch branches.

## Commands

`justfile` is the single entrypoint. Container engine is **podman** (docker fallback).

- `just up` — backing services (Postgres/Neo4j/Valkey/Qdrant/RustFS + echo llmleaf :8088)
- `just dev` — API on :8787 (auto-migrates + seeds, prints magic-link login)
- `just web` — Trunk-serve the Leptos workbench on :8080
- `just test` — `cargo test --workspace --exclude catalerum-web`
- `just lint` — **the verification gate**: clippy `-D warnings`, native + wasm web crate. Don't bulk-run `cargo fmt`.
- `just e2e` — self-contained Playwright run against the deterministic echo-LLM

Single test: `cargo test -p catalerum-store --test skills_repo <name>`.

**DB-gated tests self-skip unless `CATALERUM_TEST_DATABASE_URL` is set** — a green run without it proves nothing. Use `just up` + `postgres://catalerum:catalerum@localhost:5432/catalerum`.

## Architecture (SOUL §3–§6 — apply literally)

- Workspace of small crates under `crates/`; `catalerum-core` depends on nothing, `catalerum-api` + the `catalerum` binary wire everything. `store`/`graph`/`vector` never depend on each other.
- **Postgres is truth** (`catalerum-store`, sqlx; migrations in `crates/catalerum-store/migrations/`, run on boot). Neo4j/Qdrant are rebuildable projections; Valkey is coordination only.
- **Core knows no concrete provider** — calendars/storage/email/LLM/exec/channels only via traits in their provider crates.
- **LLM acts only through typed, scoped tools** (`catalerum-api/src/tools/`), capability-checked (`catalerum-iam`, deny-by-default, attenuating).
- **Workspace = tenancy boundary**: every tenant row carries `workspace_id`; all queries workspace-filtered.
- Graph queries via in-process Datalog (`catalerum-logic`), never raw Cypher. LLM traffic via the llmleaf gateway; dev/CI use the key-free `echo` model.

## Web crate

`catalerum-web` is Leptos/wasm, excluded from the native workspace — check/lint with `--target wasm32-unknown-unknown`. SPA hardcodes API at `http://localhost:8787` (`src/api.rs`). Reuse, don't hand-roll: `components/widgets.rs` (row actions, copy, safe-href), `Dialogs` service (never `window.confirm`), `:root` theme tokens (never raw hex).

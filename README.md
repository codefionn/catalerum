# catalerum

catalerum is a self-hostable, automated, fully-integrated LLM assistant, a catalogue of the things in your life.

This is my idea of a highly capable agentic workflow environment.

![Screenshot of catalerum chat](/screenshot.jpeg)

## Goals

- fully integrated automated personal management system
- easy way to do ai native stuff and expose it via mcp servers
- ability to integrate the whole software in coding agents

## Recipes

Run `just` to list every recipe:

| Recipe | What it does |
| ------ | ------------ |
| `just up` | Start backing services + the echo llmleaf container and wait for store health. |
| `just dev` | Boot the API (migrates + seeds + prints the magic-link). |
| `just llmleaf` | Run the llmleaf proxy container with the echo config on :8088 (standalone). |
| `just web` | Serve the web workbench with `trunk serve` on :8080. |
| `just migrate` | Migrations run automatically on boot; this documents that path. |
| `just seed` | Admin + workspace + magic-link are seeded automatically on boot. |
| `just down` | Stop and remove the backing services. |
| `just reset` | Wipe volumes and recreate the dev stack. |
| `just test` | `cargo test` across the native workspace. |
| `just check` | `cargo check --workspace --exclude catalerum-web`. |
| `just check-web` | `cargo check -p catalerum-web --target wasm32-unknown-unknown`. |
| `just fmt` / `just lint` | Format / clippy (native + wasm web). |
| `just e2e` / `just mcp` | Placeholders wired up in later milestones. |

## All-in-one container

For a single-node installation, the `all-in-one` bake target packages the
frontend, API/workers, terminal runner, preview service, llmleaf, Qdrant, and
same-origin routing into one image. It exposes only port 8080, uses SQLite and
an in-process coordination store, and provides one-time owner setup plus user
management. Neo4j is replaced by the relational graph fallback.

```sh
docker buildx bake all-in-one --load
docker run --rm -p 8080:8080 \
  -v catalerum-data:/data -v "$PWD/files:/files" -v catalerum-work:/work \
  registry.k3s.s.fionn-router.internal/catalerum/catalerum-all-in-one:dev
```

See [docs/all-in-one.md](docs/all-in-one.md) for volumes, first boot, dynamic
llmleaf providers, and the distinction from the existing distributed mode.

## OpenTelemetry and Langfuse

Tracing is opt-in under `[telemetry]` in `config/catalerum.toml`. Catalerum can
export application, HTTP, and GenAI spans to a standard OTLP/HTTP collector,
directly to Langfuse, or to both at once. Incoming W3C `traceparent` headers are
honored and exporters flush on graceful process exit.

The OTLP and Langfuse destinations each have an independent LLM content policy:

- `metadata-only` (default): model, latency, finish state, token/cache usage,
  cost, and errors; no prompts or generated content.
- `all-except-system-prompts`: full inputs/outputs with every system-role
  message removed.
- `everything`: full request, response, reasoning text, and tool payloads.

For Langfuse, enable `[telemetry.langfuse]` and inject
`CATALERUM_TELEMETRY__LANGFUSE__PUBLIC_KEY` and
`CATALERUM_TELEMETRY__LANGFUSE__SECRET_KEY`. The default endpoint is Langfuse
Cloud EU; set `CATALERUM_TELEMETRY__LANGFUSE__ENDPOINT` for another region or a
self-hosted instance. Generic OTLP is configured under `[telemetry.otlp]`, with
optional static headers for authenticated collectors.

## AI Disclosure
This project is being developed with AI assistance.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)), or
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.

# All-in-one container

The all-in-one image is the single-node distribution of catalerum. It contains
the web application, API and workers, terminal runner, preview renderer,
Qdrant, llmleaf, and the internal HTTP router. SQLite is the source of truth,
the graph tools use their relational fallback, and the coordination bus is
in-process. Neo4j and Valkey are not started.

The existing distributed images remain available as the `app`, `web`,
`preview`, `operator`, and `sandbox` bake targets. They continue to use native
PostgreSQL repositories and can use Valkey and Neo4j.

## Build and run

```sh
docker buildx bake all-in-one --load

docker run --rm --name catalerum \
  -p 8080:8080 \
  -v catalerum-data:/data \
  -v "$PWD/files:/files" \
  -v catalerum-work:/work \
  registry.k3s.s.fionn-router.internal/catalerum/catalerum-all-in-one:dev
```

Open `http://localhost:8080`. On an empty data volume, the login page asks for
the initial owner's name, email address, and password. Setup is an atomic,
one-time operation. Owners and admins can subsequently create accounts and
reset local passwords under **Settings → Users**.

Only port 8080 is exposed. The frontend is served at `/`, while `/api/*` is
routed to the loopback API with the `/api` prefix removed. Qdrant, llmleaf, the
preview service, and the API listener are not reachable directly from outside
the container.

The volumes have distinct responsibilities:

- `/data` holds SQLite and Qdrant data. Back it up with a stopped-container
  volume snapshot.
- `/files` is the user-visible file catalogue and should normally be a bind
  mount.
- `/work` is persistent terminal-runner workspace data.

Set `CATALERUM_SERVER__BASE_URL` and `CATALERUM_SERVER__WEB_URL` when the public
address is not `http://localhost:8080`. Keep the API URL's `/api` suffix.

## Dynamic LLM providers

The image embeds the official `ghcr.io/codefionn/llmleaf:0.2.5` release.
catalerum is llmleaf's topology control plane: llmleaf polls a token-protected,
loopback-only endpoint and reconciles provider and route changes without a
restart. Manage them under **Settings → LLM providers**.

Provider credentials are environment references, never literal secrets in the
database. For example, start the container with:

```sh
docker run ... -e OPENAI_API_KEY catalerum-all-in-one
```

Then create provider `openai-main`:

```json
{
  "name": "openai-main",
  "kind": "openai",
  "credential": "env:OPENAI_API_KEY"
}
```

and route `gpt-4.1`:

```json
{
  "model": "gpt-4.1",
  "targets": [{ "provider": "openai-main", "model": "gpt-4.1" }]
}
```

For OpenRouter, forward the canonical key variable and optionally make a
managed route the instance-wide chat default:

```sh
docker run ... \
  -e OPENROUTER_API_KEY \
  -e CATALERUM_LLM__DEFAULT_MODEL=deepseek/deepseek-v4-pro \
  catalerum-all-in-one
```

Create provider `openrouter-main`:

```json
{
  "name": "openrouter-main",
  "kind": "openrouter",
  "credential": "env:OPENROUTER_API_KEY"
}
```

and route `deepseek/deepseek-v4-pro`:

```json
{
  "model": "deepseek/deepseek-v4-pro",
  "targets": [
    {
      "provider": "openrouter-main",
      "model": "deepseek/deepseek-v4-pro"
    }
  ]
}
```

Create the provider and route under **Settings → LLM providers** before sending
chat turns. If the boot-time default override is omitted, each user can select
the managed model under **Settings → LLM settings** while `echo` remains the
instance fallback.

An echo provider and route are included so first boot does not require an
external provider. The generated llmleaf control token exists only in the
container process environment unless `LLMLEAF_CONTROL_TOKEN` is supplied.

## Operational limits

The all-in-one mode intentionally runs one API process. Its in-memory bus,
locks, registry, and transient buffers are not shared and are reset on restart;
durable jobs remain in SQLite and resume afterward. Do not scale this image to
multiple replicas against one SQLite volume. Use the distributed PostgreSQL
configuration for horizontal scaling and high availability.

## Browser test suite

Run the image-level Playwright suite with:

```sh
just e2e-all-in-one
```

The recipe builds the current `runtime-all-in-one` target, starts it on
`127.0.0.1:18080` with three empty temporary volumes, runs Chromium against the
only public origin, and removes the container and volumes afterward. It covers
the first-owner setup, same-origin `/api` routing, SQLite/Qdrant and fallback
service status, user creation and password reset, member login/permissions, and
dynamic llmleaf provider/route reconciliation.

Use the pinned Playwright container instead of a local browser installation with
`just e2e-all-in-one-docker`. For a previously built image:

```sh
CATALERUM_ALL_IN_ONE_IMAGE=catalerum-all-in-one:test \
CATALERUM_ALL_IN_ONE_SKIP_BUILD=1 \
just e2e-all-in-one
```

The suite itself is independently runnable against an already-started image by
setting `CATALERUM_ALL_IN_ONE_URL` and running `npm run test:all-in-one` from
`e2e/`. That target must have a fresh `/data` volume because first boot is part
of the contract.

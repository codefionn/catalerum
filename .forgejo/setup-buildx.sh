#!/usr/bin/env bash
# Shared Forgejo Actions job setup (see .forgejo/workflows/ci.yml): wait for the
# dind service, write registry credentials, and create a docker-container buildx
# builder that trusts the registry's internal CA.
#
# Inputs (env): REGISTRY, REGISTRY_USER, REGISTRY_PASSWORD, and either
# REGISTRY_CA_CERT (PEM contents) or REGISTRY_CA_FILE (a runner-mounted path).
# PLATFORMS selects whether ARM64 emulation is needed.
set -euo pipefail

# dind needs a moment to start listening; retry instead of failing on the first probe.
timeout 60 sh -c 'until docker info >/dev/null 2>&1; do sleep 1; done'

: "${REGISTRY:?REGISTRY must be set}"
: "${REGISTRY_USER:?the REGISTRY_USER secret is not set}"
: "${REGISTRY_PASSWORD:?the REGISTRY_PASSWORD secret is not set}"

work="${RUNNER_TEMP:-/tmp}/catalerum-buildx"
mkdir -p "$work"

# Prefer the infrastructure-managed bundle so a stale repository secret cannot
# override the runner's current internal CA. Other runners can use the secret.
ca_file="$work/registry-ca.crt"
if [ -n "${REGISTRY_CA_FILE:-}" ] && [ -f "$REGISTRY_CA_FILE" ]; then
  cp "$REGISTRY_CA_FILE" "$ca_file"
elif [ -n "${REGISTRY_CA_CERT:-}" ]; then
  printf '%s\n' "$REGISTRY_CA_CERT" > "$ca_file"
else
  echo "no registry CA: set the REGISTRY_CA_CERT secret or mount it at \$REGISTRY_CA_FILE" >&2
  exit 1
fi

# Harbor token requests run in the buildx client, outside the BuildKit container.
export SSL_CERT_FILE="$ca_file"
if [ -n "${GITHUB_ENV:-}" ]; then
  printf 'SSL_CERT_FILE=%s\n' "$ca_file" >> "$GITHUB_ENV"
fi

# The buildx client hands these credentials to BuildKit for pushes + the layer
# cache. Written directly (not `docker login`) so the dind daemon never has to
# trust the internal CA itself.
mkdir -p "$HOME/.docker"
auth="$(printf '%s:%s' "$REGISTRY_USER" "$REGISTRY_PASSWORD" | base64 | tr -d '\n')"
printf '{"auths":{"%s":{"auth":"%s"}}}\n' "$REGISTRY" "$auth" > "$HOME/.docker/config.json"
chmod 600 "$HOME/.docker/config.json"

# buildkitd config (mirrors .gitlab/buildkitd.toml): buildx copies the referenced
# CA into the builder container so BuildKit pushes over verified TLS.
cat > "$work/buildkitd.toml" <<TOML
[worker.oci]
  max-parallelism = 2

[registry."$REGISTRY"]
  http = false
  ca = ["$ca_file"]
TOML

# BuildKit caches executable platforms when its worker starts. In rootless
# Docker, registering binfmt afterward can invalidate that detection and leave
# ARM64 RUN steps failing with exec format error. Register before bootstrapping.
case ",${PLATFORMS:-}," in
  *,linux/arm64,*) docker run --rm --privileged tonistiigi/binfmt --install arm64 ;;
esac

# A persistent Docker daemon may retain yesterday's builder and its copied CA.
# Recreate the container with today's config. Its named cache volume survives.
if docker container inspect buildx_buildkit_catalerum0 >/dev/null 2>&1; then
  docker rm -f buildx_buildkit_catalerum0
fi

docker buildx create --use --name catalerum --driver docker-container \
  --config "$work/buildkitd.toml"
docker buildx inspect --bootstrap

# Check execution through this builder, rather than trusting its platform list.
case ",${PLATFORMS:-}," in
  *,linux/arm64,*)
    docker buildx build --platform linux/arm64 --progress plain --no-cache - <<'DOCKERFILE'
FROM debian:bookworm-slim
RUN test "$(uname -m)" = aarch64
DOCKERFILE
    ;;
esac

#!/usr/bin/env bash
# Shared Forgejo Actions job setup (see .forgejo/workflows/ci.yml): wait for the
# dind service, write registry credentials, and create a docker-container buildx
# builder that trusts the registry's internal CA.
#
# Inputs (env): REGISTRY, REGISTRY_USER, REGISTRY_PASSWORD, and either
# REGISTRY_CA_CERT (PEM contents) or REGISTRY_CA_FILE (a runner-mounted path).
set -euo pipefail

# dind needs a moment to start listening; retry instead of failing on the first probe.
timeout 60 sh -c 'until docker info >/dev/null 2>&1; do sleep 1; done'

: "${REGISTRY:?REGISTRY must be set}"
: "${REGISTRY_USER:?the REGISTRY_USER secret is not set}"
: "${REGISTRY_PASSWORD:?the REGISTRY_PASSWORD secret is not set}"

work="${RUNNER_TEMP:-/tmp}/catalerum-buildx"
mkdir -p "$work"

# Resolve the internal CA: the secret wins, else a runner-mounted bundle.
ca_file="$work/registry-ca.crt"
if [ -n "${REGISTRY_CA_CERT:-}" ]; then
  printf '%s\n' "$REGISTRY_CA_CERT" > "$ca_file"
elif [ -n "${REGISTRY_CA_FILE:-}" ] && [ -f "$REGISTRY_CA_FILE" ]; then
  cp "$REGISTRY_CA_FILE" "$ca_file"
else
  echo "no registry CA: set the REGISTRY_CA_CERT secret or mount it at \$REGISTRY_CA_FILE" >&2
  exit 1
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
[registry."$REGISTRY"]
  http = false
  ca = ["$ca_file"]
TOML

docker buildx create --use --name catalerum --driver docker-container \
  --config "$work/buildkitd.toml"
docker buildx inspect --bootstrap

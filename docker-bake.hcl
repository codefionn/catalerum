# docker-bake.hcl — build definitions for `docker buildx bake`.
#
#   docker buildx bake validate      # lint + test (no image)
#   docker buildx bake app           # build the API/workers release image (no push)
#   docker buildx bake web           # build the web SPA image (trunk + nginx)
#   docker buildx bake release --push  # build + publish app + web to the registry
#
# Every variable can be overridden by an env var of the same name or `--set`.

variable "REGISTRY"        { default = "registry.k3s.s.fionn-router.internal" }
# Harbor references are <host>/<project>/<repo>; images live in the "catalerum"
# project.
variable "IMAGE_NAME"      { default = "catalerum/catalerum" } # -> ${REGISTRY}/catalerum/catalerum
variable "WEB_IMAGE_NAME"  { default = "catalerum/catalerum-web" }
variable "OPERATOR_IMAGE_NAME" { default = "catalerum/catalerum-operator" }
variable "PREVIEW_IMAGE_NAME"  { default = "catalerum/catalerum-preview" }
variable "SANDBOX_IMAGE_NAME"  { default = "catalerum/catalerum-sandbox" }
variable "ALL_IN_ONE_IMAGE_NAME" { default = "catalerum/catalerum-all-in-one" }

# Comma-separated tag lists (CI sets them, e.g. ".../catalerum:<sha>,.../catalerum:latest").
variable "TAGS"            { default = "" } # app image
variable "WEB_TAGS"        { default = "" } # web image
variable "OPERATOR_TAGS"   { default = "" } # operator image
variable "PREVIEW_TAGS"    { default = "" } # preview render service image
variable "SANDBOX_TAGS"    { default = "" } # workspace sandbox image
variable "ALL_IN_ONE_TAGS" { default = "" } # single-container distribution

# Registry ref for the buildx layer cache (empty = no registry cache). Each CI
# stage points this at its own tag (e.g. .../catalerum:cache-lint) to avoid
# clobbering, since only one target runs per bake invocation there.
variable "CACHE_REF"       { default = "" }

# Comma-separated platform list for the release image, e.g.
# "linux/amd64,linux/arm64" (empty = build host platform only). The Dockerfile
# cross-compiles each platform ON the build host — no QEMU on the builder.
variable "PLATFORMS"       { default = "" }

target "_common" {
  context    = "."
  dockerfile = "Dockerfile"
  cache-from = CACHE_REF != "" ? ["type=registry,ref=${CACHE_REF}"] : []
  cache-to   = CACHE_REF != "" ? ["type=registry,ref=${CACHE_REF},mode=max"] : []
}

# clippy (native workspace + wasm web crate); produces no image.
target "lint" {
  inherits = ["_common"]
  target   = "lint"
  output   = ["type=cacheonly"]
}

# cargo test (DB tests self-skip without a database); produces no image.
target "test" {
  inherits = ["_common"]
  target   = "test"
  output   = ["type=cacheonly"]
}

# The release runtime image. Push with `--push`; tag via the TAGS variable.
target "app" {
  inherits  = ["_common"]
  target    = "runtime"
  platforms = PLATFORMS != "" ? split(",", PLATFORMS) : []
  tags      = TAGS != "" ? split(",", TAGS) : ["${REGISTRY}/${IMAGE_NAME}:dev"]
}

# The web SPA image (Trunk/wasm build behind unprivileged nginx). Own cache-ref
# suffix: `bake app web` runs both targets in ONE invocation, and two targets
# exporting to the same registry cache tag would clobber each other.
target "web" {
  inherits   = ["_common"]
  target     = "runtime-web"
  platforms  = PLATFORMS != "" ? split(",", PLATFORMS) : []
  tags       = WEB_TAGS != "" ? split(",", WEB_TAGS) : ["${REGISTRY}/${WEB_IMAGE_NAME}:dev"]
  cache-from = CACHE_REF != "" ? ["type=registry,ref=${CACHE_REF}-web"] : []
  cache-to   = CACHE_REF != "" ? ["type=registry,ref=${CACHE_REF}-web,mode=max"] : []
}

# The WorkspaceSandbox operator image (SOUL §20) — a second binary from the
# same workspace. Own cache-ref suffix for the same reason as web.
target "operator" {
  inherits   = ["_common"]
  target     = "runtime-operator"
  platforms  = PLATFORMS != "" ? split(",", PLATFORMS) : []
  tags       = OPERATOR_TAGS != "" ? split(",", OPERATOR_TAGS) : ["${REGISTRY}/${OPERATOR_IMAGE_NAME}:dev"]
  cache-from = CACHE_REF != "" ? ["type=registry,ref=${CACHE_REF}-operator"] : []
  cache-to   = CACHE_REF != "" ? ["type=registry,ref=${CACHE_REF}-operator,mode=max"] : []
}

# The standalone preview render service (Dockerfile `runtime-preview`, SOUL
# §9/§10): a slim LibreOffice+poppler image serving one Rust binary. Like
# `sandbox`, its apt layer RUNs target-arch, so a foreign-platform build needs
# QEMU binfmt on the builder. Own cache-ref suffix (see `web`).
target "preview" {
  inherits   = ["_common"]
  target     = "runtime-preview"
  platforms  = PLATFORMS != "" ? split(",", PLATFORMS) : []
  tags       = PREVIEW_TAGS != "" ? split(",", PREVIEW_TAGS) : ["${REGISTRY}/${PREVIEW_IMAGE_NAME}:dev"]
  cache-from = CACHE_REF != "" ? ["type=registry,ref=${CACHE_REF}-preview"] : []
  cache-to   = CACHE_REF != "" ? ["type=registry,ref=${CACHE_REF}-preview,mode=max"] : []
}

# The batteries-included workspace sandbox image (Dockerfile `runtime-sandbox`).
# The ONLY target whose foreign-platform build needs QEMU binfmt on the builder:
# its apt/pip layers execute target-arch binaries instead of cross-compiling
# (CI registers binfmt via tonistiigi/binfmt before baking this).
target "sandbox" {
  inherits   = ["_common"]
  target     = "runtime-sandbox"
  platforms  = PLATFORMS != "" ? split(",", PLATFORMS) : []
  tags       = SANDBOX_TAGS != "" ? split(",", SANDBOX_TAGS) : ["${REGISTRY}/${SANDBOX_IMAGE_NAME}:dev"]
  cache-from = CACHE_REF != "" ? ["type=registry,ref=${CACHE_REF}-sandbox"] : []
  cache-to   = CACHE_REF != "" ? ["type=registry,ref=${CACHE_REF}-sandbox,mode=max"] : []
}

target "all-in-one" {
  inherits   = ["_common"]
  target     = "runtime-all-in-one"
  args       = { CATALERUM_WEB_API_BASE = "/api" }
  platforms  = PLATFORMS != "" ? split(",", PLATFORMS) : []
  tags       = ALL_IN_ONE_TAGS != "" ? split(",", ALL_IN_ONE_TAGS) : ["${REGISTRY}/${ALL_IN_ONE_IMAGE_NAME}:dev"]
  cache-from = CACHE_REF != "" ? ["type=registry,ref=${CACHE_REF}-all-in-one"] : []
  cache-to   = CACHE_REF != "" ? ["type=registry,ref=${CACHE_REF}-all-in-one,mode=max"] : []
}

group "default"  { targets = ["app", "web", "operator", "preview", "sandbox", "all-in-one"] }
group "validate" { targets = ["lint", "test"] }
group "release"  { targets = ["app", "web", "operator", "preview", "sandbox", "all-in-one"] }

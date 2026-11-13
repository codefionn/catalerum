#!/bin/sh
# Trunk post_build hook for the Markdown Web Worker (src/md_worker.rs).
#
# Trunk builds the worker (`data-type="worker"` in index.html) but tells the
# app nothing about its output, and depending on the Trunk version the file
# names may not be hashed. nginx serves *.js/*.wasm as immutable, so this hook
# writes a loader whose name and URLs carry a hash of the worker's js+wasm,
# then points index.html's `catalerum-md-worker` meta tag at it.
set -eu
# Resolve this script before leaving the project dir, so the hash below can
# still read it after the cd.
self=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
cd "$TRUNK_STAGING_DIR"
js=$(ls catalerum-markdown-worker*.js 2>/dev/null | grep -v -- '-loader-' | head -n 1 || true)
wasm=$(ls catalerum-markdown-worker*_bg.wasm 2>/dev/null | head -n 1 || true)
if [ -z "$js" ] || [ -z "$wasm" ]; then
    echo "inject-md-worker: catalerum-markdown-worker js/wasm missing in $TRUNK_STAGING_DIR" >&2
    exit 1
fi
# Hash this script too, so a change to the loader template renames it.
hash=$(cat "$js" "$wasm" "$self" | sha256sum | cut -c1-16)
loader="catalerum-markdown-worker-loader-$hash.js"
printf 'importScripts("./%s?v=%s");wasm_bindgen({module_or_path:"./%s?v=%s"});\n' \
    "$js" "$hash" "$wasm" "$hash" > "$loader"
sed -i "s|__CATALERUM_MD_WORKER__|${TRUNK_PUBLIC_URL}${loader}|" index.html

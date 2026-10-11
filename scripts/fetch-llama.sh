#!/bin/bash
# Fetches llama.cpp (MIT) at the pinned tag into Vendor/llama.cpp and applies scripts/llama.cpp-no-logits.patch, for
# the Rust build (crates/llama-sys builds it with CMake). Uses the same TAG as scripts/build-llama.sh: both builds must
# make the same vectors, which the reference vector test checks. Does nothing when the source is already there.
set -euo pipefail
cd "$(dirname "$0")/.."
TAG=$(sed -n 's/^TAG=\([^ ]*\).*/\1/p' scripts/build-llama.sh)
PATCH=scripts/llama.cpp-no-logits.patch
SOURCE=Vendor/llama.cpp

if [[ ! -d "$SOURCE" ]]; then
  git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$TAG" https://github.com/ggml-org/llama.cpp "$SOURCE"
elif [[ "$(git -C "$SOURCE" describe --tags --exact-match 2>/dev/null)" != "$TAG" ]]; then
  git -C "$SOURCE" fetch --quiet --depth 1 origin tag "$TAG"
  git -C "$SOURCE" checkout --quiet --force "$TAG"   # --force also drops the old patch
fi
if git -C "$SOURCE" apply --check "../../$PATCH" 2>/dev/null; then
  git -C "$SOURCE" apply "../../$PATCH"
fi
echo "llama.cpp $TAG (patched) is in $SOURCE"

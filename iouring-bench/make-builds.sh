#!/bin/bash
# Generates the two build trees for the benchmark app. src/main.rs is
# copied verbatim into both; the manifests differ ONLY in the topcoat
# path (upstream vs fork). The io_uring build is the fork tree built
# with `--features io-uring`.
set -euo pipefail
cd "$(dirname "$0")"
for variant in baseline fork; do
  rm -rf "build-$variant"
  mkdir -p "build-$variant/src"
  cp bench-app/src/main.rs "build-$variant/src/main.rs"
  if [ "$variant" = baseline ]; then p="$PWD/upstream/crates/topcoat"; else p="$PWD/fork/crates/topcoat"; fi
  sed "s|__TOPCOAT_PATH__|$p|" bench-app/Cargo.toml.template > "build-$variant/Cargo.toml"
  if [ "$variant" = baseline ]; then
    # Upstream topcoat has no io-uring feature to forward to, so the app
    # feature is an empty placeholder there (it is never enabled for the
    # baseline build; src/main.rs is still byte-identical).
    sed -i 's|io-uring = \["topcoat/io-uring"\]|io-uring = []|' "build-$variant/Cargo.toml"
  fi
done
echo "build trees generated"

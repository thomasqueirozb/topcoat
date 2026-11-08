#!/bin/bash
# Generates the two build trees for the benchmark app. src/main.rs is
# copied verbatim into both; the manifests differ ONLY in the topcoat
# path (upstream vs fork). The io_uring build is the fork tree built
# with `--features io-uring`.
set -euo pipefail
cd "$(dirname "$0")"

# Resolve the fork and upstream trees. Two layouts are supported:
#  1. Sibling layout: this script sits next to fork/ and upstream/ clones.
#  2. In-repo layout: this script is in <fork-clone>/iouring-bench/ — the
#     fork is the parent directory, and upstream is cloned (pinned to the
#     baseline commit) into ./.upstream on first use.
UPSTREAM_BASELINE=4602ca466d1883106f20d5b735eba6b2a86cb467
if [ -f "$PWD/fork/crates/topcoat/Cargo.toml" ]; then
  FORK_DIR="$PWD/fork"
else
  FORK_DIR="$(cd "$PWD/.." && pwd)"
fi
if [ -f "$PWD/upstream/crates/topcoat/Cargo.toml" ]; then
  UPSTREAM_DIR="$PWD/upstream"
elif [ -f "$PWD/../upstream/crates/topcoat/Cargo.toml" ]; then
  UPSTREAM_DIR="$(cd "$PWD/../upstream" && pwd)"
else
  UPSTREAM_DIR="$PWD/.upstream"
  if [ ! -f "$UPSTREAM_DIR/crates/topcoat/Cargo.toml" ]; then
    echo "cloning upstream tokio-rs/topcoat (pinned to $UPSTREAM_BASELINE) into $UPSTREAM_DIR ..."
    git clone https://github.com/tokio-rs/topcoat.git "$UPSTREAM_DIR"
    git -C "$UPSTREAM_DIR" checkout "$UPSTREAM_BASELINE"
  fi
fi
echo "fork tree:     $FORK_DIR"
echo "upstream tree: $UPSTREAM_DIR"

for variant in baseline fork; do
  rm -rf "build-$variant"
  mkdir -p "build-$variant/src"
  cp bench-app/src/main.rs "build-$variant/src/main.rs"
  if [ "$variant" = baseline ]; then p="$UPSTREAM_DIR/crates/topcoat"; else p="$FORK_DIR/crates/topcoat"; fi
  sed "s|__TOPCOAT_PATH__|$p|" bench-app/Cargo.toml.template > "build-$variant/Cargo.toml"
  if [ "$variant" = baseline ]; then
    # Upstream topcoat has no io-uring feature to forward to, so the app
    # feature is an empty placeholder there (it is never enabled for the
    # baseline build; src/main.rs is still byte-identical).
    sed -i 's|io-uring = \["topcoat/io-uring"\]|io-uring = []|' "build-$variant/Cargo.toml"
  fi
done
echo "build trees generated"

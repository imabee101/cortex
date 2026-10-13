#!/bin/bash
# Builds the client at a commit into $CORTEX_TBENCH_HOME/bin/cortex-<version>-portable. Task images are as old as
# Debian 11, so the build runs in the toolchain's bullseye image (glibc 2.31); a host build needs glibc 2.39.
# Capped at 12 GB like every build on this host. Usage: build.sh <commit> <version> [suffix]
set -euo pipefail
commit=${1:?commit} version=${2:?version} suffix=${3:-}
work=${CORTEX_TBENCH_HOME:-$HOME/data/cortex-tbench}
repo=$(git rev-parse --show-toplevel)
common=$(git -C "$repo" rev-parse --path-format=absolute --git-common-dir)
name=cortex-$version${suffix:+-$suffix}-portable
src=$(mktemp -d "$work/src-XXXXXX")
trap 'git -C "$repo" worktree remove --force "$src"' EXIT
mkdir -p "$work/bin"
git -C "$repo" worktree add -q --detach "$src" "$commit"
toolchain=$(sed -nE 's/^channel *= *"([^"]+)".*/\1/p' "$src/rust-toolchain.toml")
protoc=$(cd "$src" && dotslash -- fetch bin/protoc)
docker run --rm -m 12g --memory-swap 12g \
  -v "$src":/src -v "$common":"$common":ro \
  -v cortex-tb-target:/target -v cortex-tb-cargo:/usr/local/cargo/registry \
  -v "$protoc":/usr/local/bin/protoc:ro -v "$work/bin":/out \
  -e CARGO_TARGET_DIR=/target -e PROTOC=/usr/local/bin/protoc -e CARGO_BUILD_JOBS=8 -e CORTEX_VERSION="$version" \
  -w /src "rust:$toolchain-bullseye" bash -c "git config --global --add safe.directory '*' &&
    cargo build --release --locked -p cortex-pager-bin &&
    install -m 0755 /target/release/cortex-pager /out/$name && chown $(id -u):$(id -g) /out/$name"
echo "$work/bin/$name: $("$work/bin/$name" --version)"

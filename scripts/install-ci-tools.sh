#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
mkdir -p .ci-tools/archives .ci-tools/platform-tools

fetch_verified() {
  local filename="$1" url="$2" sha="$3"
  local archive=".ci-tools/archives/$filename"
  if [[ ! -f "$archive" ]]; then
    curl --fail --location --retry 3 --output "$archive" "$url"
  fi
  printf '%s  %s\n' "$sha" "$archive" | sha256sum --check --status
}

fetch_verified solana-v2.3.0.tar.bz2 \
  https://github.com/anza-xyz/agave/releases/download/v2.3.0/solana-release-x86_64-unknown-linux-gnu.tar.bz2 \
  56241fbe862495ff01b2b875195e44f94c22e9f2a504591a3ade1b9d82862730
fetch_verified platform-tools-v1.57.tar.bz2 \
  https://github.com/anza-xyz/platform-tools/releases/download/v1.57/platform-tools-linux-x86_64.tar.bz2 \
  b0f7af104adf726fff2a6a09ea2eb2f2d2965c92295f4d7388c08d140e0c2b00

tar -xjf .ci-tools/archives/solana-v2.3.0.tar.bz2 -C .ci-tools
tar -xjf .ci-tools/archives/platform-tools-v1.57.tar.bz2 -C .ci-tools/platform-tools
sdk="$PWD/.ci-tools/solana-release/bin/platform-tools-sdk/sbf"
mkdir -p "$sdk/dependencies"
ln -sfn "$PWD/.ci-tools/platform-tools" "$sdk/dependencies/platform-tools"

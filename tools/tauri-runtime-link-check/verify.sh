#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_dir=$(CDPATH= cd -- "$script_dir/../.." && pwd)
target_dir="$repo_dir/../blitz-rust/target/tauri-runtime-link-check"

CARGO_TARGET_DIR="$target_dir" cargo build --release --manifest-path "$script_dir/Cargo.toml"
binary="$target_dir/release/tauri-runtime-link-check"
linked=$(otool -L "$binary")

if printf '%s\n' "$linked" | grep -E 'WebKit|libc\+\+|Python' >/dev/null; then
  printf '%s\n' "$linked" >&2
  echo "forbidden runtime dependency linked" >&2
  exit 1
fi

printf '%s\n' "$linked"

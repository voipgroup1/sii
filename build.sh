#!/usr/bin/env bash
# 构建 sii 的 wasm 产物。
#   依赖：rustup target wasm32-unknown-unknown、wasm-bindgen-cli（版本必须与 Cargo.lock 一致）
set -euo pipefail
cd "$(dirname "$0")"

export PATH="$HOME/.cargo/bin:$HOME/bin:$PATH"
export LANG=C.UTF-8 LC_ALL=C.UTF-8

VER=$(grep -A1 '^name = "wasm-bindgen"$' Cargo.lock | grep '^version' | head -1 | sed 's/version = "\(.*\)"/\1/')
echo "==> wasm-bindgen $VER"

echo "==> cargo build --release --target wasm32-unknown-unknown"
cargo build --release --target wasm32-unknown-unknown --lib

echo "==> wasm-bindgen"
wasm-bindgen \
  --target web \
  --no-typescript \
  --out-dir pkg \
  --out-name sii \
  target/wasm32-unknown-unknown/release/sii.wasm

ls -lh pkg/ | sed 's/^/    /'
echo "==> 完成。起个静态服务器：python3 -m http.server 8080"

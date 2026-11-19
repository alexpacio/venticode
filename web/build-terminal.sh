#!/bin/sh
set -eu
cd "$(dirname "$0")/../tui/browser"
cargo build --workspace --target wasm32-unknown-unknown --release --locked
wasm-bindgen --target web --out-dir ../../web/src/generated/terminal target/wasm32-unknown-unknown/release/venticode_tui_browser.wasm
wasm-bindgen --target web --out-dir ../../web/src/generated/terminal target/wasm32-unknown-unknown/release/venticode_remote_screen.wasm
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz -o ../../web/src/generated/terminal/venticode_tui_browser_bg.wasm ../../web/src/generated/terminal/venticode_tui_browser_bg.wasm
  wasm-opt -Oz -o ../../web/src/generated/terminal/venticode_remote_screen_bg.wasm ../../web/src/generated/terminal/venticode_remote_screen_bg.wasm
fi

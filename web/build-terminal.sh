#!/bin/sh
set -eu
cd "$(dirname "$0")/../tui/browser"
cargo build --target wasm32-unknown-unknown --release --locked
wasm-bindgen --target web --out-dir ../../web/src/generated/terminal target/wasm32-unknown-unknown/release/venticode_tui_browser.wasm

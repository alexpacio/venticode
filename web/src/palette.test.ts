// The console TUI and the browser renderer share tui/src/view.rs, but the
// browser draws the input card in CSS. This keeps the two palettes from
// drifting: every Ansi256 constant in the renderer must have a matching token.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const LEVELS = [0, 95, 135, 175, 215, 255];
const hex = (value: number) => value.toString(16).padStart(2, "0");

// Same conversion as ftui_style::ansi256_to_rgb.
function ansi256(index: number): string {
  if (index >= 232) {
    const gray = 8 + 10 * (index - 232);
    return `#${hex(gray)}${hex(gray)}${hex(gray)}`;
  }
  const offset = index - 16;
  const rgb = [
    LEVELS[Math.floor(offset / 36)],
    LEVELS[Math.floor(offset / 6) % 6],
    LEVELS[offset % 6],
  ];
  return `#${rgb.map(hex).join("")}`;
}

test("the web terminal palette matches the console TUI", () => {
  const view = readFileSync(new URL("../../tui/src/view.rs", import.meta.url), "utf8");
  const css = readFileSync(new URL("./styles.css", import.meta.url), "utf8");
  const constants = [...view.matchAll(/const ([A-Z]+): u8 = (\d+);/g)].map((match) => ({
    name: match[1],
    index: Number(match[2]),
  }));
  assert.ok(constants.length > 0, "no palette constants found in tui/src/view.rs");
  for (const { name, index } of constants) {
    const token = css.match(new RegExp(`--tui-${name.toLowerCase()}: (#[0-9a-f]{6});`));
    assert.ok(token, `styles.css is missing --tui-${name.toLowerCase()}`);
    assert.equal(token![1], ansi256(index), `--tui-${name.toLowerCase()} should be Ansi256 ${index}`);
  }
});

test("the terminal uses the bundled cell font", () => {
  const css = readFileSync(new URL("./styles.css", import.meta.url), "utf8");
  assert.match(css, /--term-mono: "Venticode Mono", "Venticode Symbols"/);
  assert.match(css, /font: 14px\/1\.2 var\(--term-mono\);/);
  assert.match(css, /font-variant-ligatures: none/);
});
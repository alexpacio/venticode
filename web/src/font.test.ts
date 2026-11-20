// The terminal renderer (tui/src/view.rs) is shared with the console client, so
// the browser frame only matches it when every glyph it emits exists in the
// bundled fonts. This reads the WOFF2 cmaps directly, so it needs no tooling.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { brotliDecompressSync } from "node:zlib";

const TAGS = [
  "cmap", "head", "hhea", "hmtx", "maxp", "name", "OS/2", "post", "cvt ", "fpgm", "glyf", "loca",
  "prep", "CFF ", "VORG", "EBDT", "EBLC", "gasp", "hdmx", "kern", "LTSH", "PCLT", "VDMX", "vhea",
  "vmtx", "BASE", "GDEF", "GPOS", "GSUB", "EBSC", "JSTF", "MATH", "CBDT", "CBLC", "COLR", "CPAL",
  "SVG ", "sbix", "acnt", "avar", "bdat", "bloc", "bsln", "cvar", "fdsc", "feat", "fmtx", "fvar",
  "gvar", "hsty", "just", "lcar", "mort", "morx", "opbd", "prop", "trak", "Zapf", "Silf", "Glat",
  "Gloc", "Feat", "Sill",
];

function base128(bytes: Uint8Array, offset: number): [number, number] {
  let value = 0;
  for (let i = 0; i < 5; i++) {
    const byte = bytes[offset++];
    value = value * 128 + (byte & 0x7f);
    if (!(byte & 0x80)) return [value, offset];
  }
  throw new Error("invalid base128 integer");
}

function cmapTable(buffer: Buffer): Buffer {
  const bytes = new Uint8Array(buffer);
  const signature = String.fromCharCode(bytes[0], bytes[1], bytes[2], bytes[3]);
  if (signature !== "wOF2") throw new Error(`not a WOFF2 file: ${signature}`);
  const view = new DataView(buffer.buffer, buffer.byteOffset, buffer.byteLength);
  const numTables = view.getUint16(12);
  const compressedSize = view.getUint32(20);
  let offset = 48;
  const tables: Array<{ tag: string; length: number }> = [];
  for (let i = 0; i < numTables; i++) {
    const flags = bytes[offset++];
    const tagIndex = flags & 0x3f;
    const version = flags >> 6;
    let tag: string;
    if (tagIndex === 63) {
      tag = String.fromCharCode(bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]);
      offset += 4;
    } else {
      tag = TAGS[tagIndex];
    }
    let [length, next] = base128(bytes, offset);
    offset = next;
    const outline = tag === "glyf" || tag === "loca";
    if (version !== 0 && version !== 3) {
      [length, offset] = base128(bytes, offset);
    } else if (version === 0 && outline) {
      [length, offset] = base128(bytes, offset);
    }
    tables.push({ tag, length });
  }
  const data = brotliDecompressSync(bytes.subarray(offset, offset + compressedSize));
  let position = 0;
  for (const table of tables) {
    if (table.tag === "cmap") return Buffer.from(data.subarray(position, position + table.length));
    position += table.length;
  }
  throw new Error("cmap table missing");
}

function subtable(view: DataView, offset: number): Set<number> | null {
  const format = view.getUint16(offset);
  const found = new Set<number>();
  if (format === 4) {
    const segments = view.getUint16(offset + 6) / 2;
    const endBase = offset + 14;
    const startBase = endBase + segments * 2 + 2;
    const deltaBase = startBase + segments * 2;
    const rangeBase = deltaBase + segments * 2;
    for (let i = 0; i < segments; i++) {
      const end = view.getUint16(endBase + i * 2);
      const start = view.getUint16(startBase + i * 2);
      if (start === 0xffff && end === 0xffff) continue;
      const delta = view.getInt16(deltaBase + i * 2);
      const rangeOffset = view.getUint16(rangeBase + i * 2);
      for (let codepoint = start; codepoint <= end; codepoint++) {
        let glyph = 0;
        if (rangeOffset === 0) {
          glyph = (codepoint + delta) & 0xffff;
        } else {
          glyph = view.getUint16(rangeBase + i * 2 + rangeOffset + (codepoint - start) * 2);
          if (glyph !== 0) glyph = (glyph + delta) & 0xffff;
        }
        if (glyph !== 0) found.add(codepoint);
      }
    }
    return found;
  }
  if (format === 12) {
    const groups = view.getUint32(offset + 12);
    for (let i = 0; i < groups; i++) {
      const base = offset + 16 + i * 12;
      const start = view.getUint32(base);
      const end = view.getUint32(base + 4);
      if (view.getUint32(base + 8) === 0) continue;
      for (let codepoint = start; codepoint <= end; codepoint++) found.add(codepoint);
    }
    return found;
  }
  return null;
}

function codepoints(buffer: Buffer): Set<number> {
  const table = cmapTable(buffer);
  const view = new DataView(table.buffer, table.byteOffset, table.byteLength);
  const count = view.getUint16(2);
  const rank = (platform: number, encoding: number) =>
    platform === 3 && encoding === 10 ? 3 : platform === 0 ? 2 : platform === 3 && encoding === 1 ? 1 : 0;
  const records: Array<{ platform: number; encoding: number; offset: number }> = [];
  for (let i = 0; i < count; i++) {
    records.push({
      platform: view.getUint16(4 + i * 8),
      encoding: view.getUint16(6 + i * 8),
      offset: view.getUint32(8 + i * 8),
    });
  }
  records.sort((a, b) => rank(b.platform, b.encoding) - rank(a.platform, a.encoding));
  for (const record of records) {
    const found = subtable(view, record.offset);
    if (found) return found;
  }
  throw new Error("no Unicode cmap subtable");
}

function glyphsIn(path: string): string[] {
  const source = readFileSync(path, "utf8");
  const found = new Set<string>();
  for (const match of source.matchAll(/"((?:[^"\\]|\\.)*)"/g)) {
    for (const char of match[1].replace(/\\./g, "")) {
      if ((char.codePointAt(0) ?? 0) > 126) found.add(char);
    }
  }
  return [...found];
}

test("every glyph the web terminal draws is covered by the bundled fonts", () => {
  const mono = codepoints(readFileSync(new URL("./design/assets/jetbrains-mono-latin-wght-normal.woff2", import.meta.url)));
  const symbols = codepoints(readFileSync(new URL("./design/assets/venticode-symbols.woff2", import.meta.url)));
  // Every module of the shared TUI core can put glyphs on screen.
  const drawn = new Set(
    ["app", "view", "nav", "screens"].flatMap((name) =>
      glyphsIn(new URL(`../../tui/src/${name}.rs`, import.meta.url).pathname),
    ),
  );
  const missing = [...drawn].filter((char) => {
    const codepoint = char.codePointAt(0)!;
    return !mono.has(codepoint) && !symbols.has(codepoint);
  });
  assert.deepEqual(missing, [], `bundled fonts are missing: ${missing.join(" ")}`);
});

test("the symbol fallback keeps the terminal cell width", () => {
  const symbols = codepoints(readFileSync(new URL("./design/assets/venticode-symbols.woff2", import.meta.url)));
  const expected = [0x23bf, 0x2500, 0x25cf, 0x2692, 0x2717, 0x2722, 0x2733, 0x2736, 0x273b, 0x273d, 0x276f];
  for (const codepoint of expected) {
    assert.ok(symbols.has(codepoint), `symbol fallback is missing U+${codepoint.toString(16)}`);
  }
});
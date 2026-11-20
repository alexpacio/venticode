#!/usr/bin/env python3
"""Regenerate web/src/design/assets/venticode-symbols.woff2.

The terminal renderer (tui/src/view.rs) draws box drawing, geometric shapes and
dingbats that the bundled JetBrains Mono latin subset does not cover. This
script subsets them out of the Noto families (SIL OFL 1.1) and pins every
advance to 600/1000 em, the JetBrains Mono cell width, so a symbol occupies
exactly one terminal cell and the browser grid cannot drift.

The sources are not vendored; download them once:

  Noto Sans Mono      https://github.com/notofonts/noto-fonts/raw/main/hinted/ttf/NotoSansMono/NotoSansMono-Regular.ttf
  Noto Sans Symbols   https://github.com/notofonts/noto-fonts/raw/main/hinted/ttf/NotoSansSymbols/NotoSansSymbols-Regular.ttf
  Noto Sans Symbols 2 https://github.com/notofonts/noto-fonts/raw/main/hinted/ttf/NotoSansSymbols2/NotoSansSymbols2-Regular.ttf

Requirements: python3 -m pip install fonttools brotli
"""

from __future__ import annotations

import shutil
import sys
from pathlib import Path

from fontTools import subset
from fontTools.merge import Merger
from fontTools.ttLib import TTFont

CELL = 600  # JetBrains Mono advance, 600/1000 em
SOURCES = {
    "─": "NotoSansMono-Regular.ttf",  # U+2500
    "●": "NotoSansSymbols2-Regular.ttf",  # U+25CF
    "✗": "NotoSansSymbols2-Regular.ttf",  # U+2717
    "✢": "NotoSansSymbols2-Regular.ttf",  # U+2722
    "✳": "NotoSansSymbols2-Regular.ttf",  # U+2733
    "✶": "NotoSansSymbols2-Regular.ttf",  # U+2736
    "✻": "NotoSansSymbols2-Regular.ttf",  # U+273B
    "✽": "NotoSansSymbols2-Regular.ttf",  # U+273D
    "❯": "NotoSansSymbols2-Regular.ttf",  # U+276F
    "⎿": "NotoSansSymbols-Regular.ttf",  # U+23BF
    "⚒": "NotoSansSymbols-Regular.ttf",  # U+2692
    # Rounded boxes, panes and the navigator tree.
    "╭": "NotoSansMono-Regular.ttf",  # U+256D
    "╮": "NotoSansMono-Regular.ttf",  # U+256E
    "╯": "NotoSansMono-Regular.ttf",  # U+256F
    "╰": "NotoSansMono-Regular.ttf",  # U+2570
    "│": "NotoSansMono-Regular.ttf",  # U+2502
    "█": "NotoSansMono-Regular.ttf",  # U+2588
    "░": "NotoSansMono-Regular.ttf",  # U+2591
    "▏": "NotoSansMono-Regular.ttf",  # U+258F
    "▣": "NotoSansMono-Regular.ttf",  # U+25A3
    "▸": "NotoSansMono-Regular.ttf",  # U+25B8
    "▾": "NotoSansMono-Regular.ttf",  # U+25BE
    "◆": "NotoSansMono-Regular.ttf",  # U+25C6
    "○": "NotoSansMono-Regular.ttf",  # U+25CB
    "◯": "NotoSansMono-Regular.ttf",  # U+25EF
    "←": "NotoSansMono-Regular.ttf",  # U+2190
    "→": "NotoSansMono-Regular.ttf",  # U+2192
    "↔": "NotoSansMono-Regular.ttf",  # U+2194
    "‖": "NotoSansMono-Regular.ttf",  # U+2016
    "↗": "NotoSansSymbols-Regular.ttf",  # U+2197
    "⌂": "NotoSansSymbols-Regular.ttf",  # U+2302
    "⌕": "NotoSansSymbols-Regular.ttf",  # U+2315
    "☐": "NotoSansSymbols2-Regular.ttf",  # U+2610
    "☒": "NotoSansSymbols2-Regular.ttf",  # U+2612
    "✓": "NotoSansSymbols2-Regular.ttf",  # U+2713
    "✎": "NotoSansSymbols2-Regular.ttf",  # U+270E
}


def make_subset(source: Path, codepoints: set[int], work: Path) -> Path:
    options = subset.Options()
    options.flavor = None
    options.layout_features = []
    options.name_IDs = ["*"]
    options.name_legacy = True
    options.notdef_outline = False
    options.recalc_bounds = True
    options.drop_tables += ["DSIG"]
    font = subset.load_font(str(source), options)
    subsetter = subset.Subsetter(options=options)
    subsetter.populate(unicodes=codepoints)
    subsetter.subset(font)
    font.flavor = None
    font.save(str(work))
    return work


def pin_advances(path: Path, target: Path) -> None:
    font = TTFont(str(path))
    hmtx = font["hmtx"]
    for name in font.getGlyphOrder():
        advance, lsb = hmtx[name]
        hmtx[name] = (CELL, lsb)
    font["OS/2"].xAvgCharWidth = CELL
    font.flavor = "woff2"
    for name_id, value in ((1, "Venticode Symbols"), (2, "Regular"), (4, "Venticode Symbols"), (6, "VenticodeSymbols-Regular")):
        font["name"].setName(value, name_id, 3, 1, 0x409)
    font.save(str(target))


def main() -> int:
    root = Path(__file__).resolve().parents[1]
    out_dir = root / "src" / "design" / "assets"
    source_dir = Path(sys.argv[1]) if len(sys.argv) > 1 else Path.cwd()
    work = Path(".symbols-work")
    work.mkdir(exist_ok=True)

    by_source: dict[str, set[int]] = {}
    for char, source in SOURCES.items():
        by_source.setdefault(source, set()).add(ord(char))

    parts = []
    for index, (source, codepoints) in enumerate(by_source.items()):
        path = source_dir / source
        if not path.exists():
            print(f"missing source font: {path.resolve()}", file=sys.stderr)
            return 2
        parts.append(make_subset(path, codepoints, work / f"part{index}.ttf"))

    if len(parts) == 1:
        merged = parts[0]
    else:
        merged = work / "merged.ttf"
        Merger().merge([str(part) for part in parts]).save(str(merged))

    pin_advances(merged, out_dir / "venticode-symbols.woff2")
    target = out_dir / "venticode-symbols.woff2"
    print(f"wrote {target} ({target.stat().st_size} bytes)")

    font = TTFont(str(target))
    cmap = font.getBestCmap()
    missing = [char for char in SOURCES if ord(char) not in cmap]
    if missing:
        print(f"missing glyphs: {missing}", file=sys.stderr)
        return 1
    shutil.rmtree(work, ignore_errors=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
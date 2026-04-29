#!/usr/bin/env python3
"""
Generate the RantaiClaw CLI splash assets.

Outputs five files under `src/onboard/assets/`:

* `banner_full.txt` — figlet "Rantaiclaw" in pyfiglet's `slant` font, sized
  to fit ~72 columns. Used when the terminal has space for it.
* `banner_small.txt` — same word in `small` font (~46 cols) for narrower
  terminals.
* `logo_braille.txt` — brand logo rendered as Braille pixel art
  (Unicode 2800-28FF). Each character cell encodes an 8-pixel block
  (2 wide × 4 tall) so silhouettes stay crisp at small sizes.
* `logo_braille_gradient.txt` — same braille shape with a per-row color
  marker (`@N@\\t<line>`) the Rust side maps to a 4-stop palette.
* `logo_plain.txt` — glyph-only fallback for monochrome terminals.

Production builds bake the assets — Pillow + pyfiglet are only invoked
when intentionally regenerating.

Run after touching the source PNG or the dimensions:

    python3 scripts/branding/render_logo_ascii.py
"""

from __future__ import annotations

import sys
from pathlib import Path

import pyfiglet  # type: ignore
from PIL import Image  # type: ignore

REPO_ROOT = Path(__file__).resolve().parents[2]
LOGO_SRC = REPO_ROOT.parent.parent / "Logo-only Border or Stroke (1).png"
ASSETS = REPO_ROOT / "src" / "onboard" / "assets"

# Brand palette pulled from rantai-agents:
#   navy  #040b2e — logo body / dark accent
#   sky   #5eb8ff — logo squares / light accent
#   blue  #3b8cff — primary brand accent (oklch(0.55 0.2 250))
#   muted #6b7280 — secondary text

LOGO_W_CHARS = 30
LOGO_H_CHARS = 16
SOURCE_W = LOGO_W_CHARS * 2   # 60 px
SOURCE_H = LOGO_H_CHARS * 4   # 64 px

# Per-row color gradient — index into the 4-color palette in branding.rs.
# 0 = sky, 1 = blue, 2 = navy-bright, 3 = muted/dim.
# Top → sky (the cyan accent squares); middle → blue; lower → deeper navy.
LOGO_GRADIENT = [0, 0, 0, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 3, 3, 3]
assert len(LOGO_GRADIENT) == LOGO_H_CHARS


def render_banner(font: str, label: str = "Rantaiclaw", width: int = 200) -> str:
    """`width=200` so pyfiglet never wraps mid-word; the terminal layer
    handles fallback to a smaller font for narrow viewports."""
    fig = pyfiglet.Figlet(font=font, width=width)
    out = fig.renderText(label)
    lines = [line.rstrip() for line in out.splitlines()]
    while lines and not lines[-1]:
        lines.pop()
    while lines and not lines[0]:
        lines.pop(0)
    return "\n".join(lines) + "\n"


def render_braille() -> tuple[str, str, str]:
    if not LOGO_SRC.exists():
        sys.exit(f"missing source PNG: {LOGO_SRC}")

    img = Image.open(LOGO_SRC).convert("RGBA")
    img = img.resize((SOURCE_W, SOURCE_H), Image.LANCZOS)
    rgba = img.load()

    # Braille dot bits within a 2×4 cell:
    #   (0,0)→bit0   (1,0)→bit3
    #   (0,1)→bit1   (1,1)→bit4
    #   (0,2)→bit2   (1,2)→bit5
    #   (0,3)→bit6   (1,3)→bit7
    DOT_BITS = {
        (0, 0): 0, (0, 1): 1, (0, 2): 2, (1, 0): 3,
        (1, 1): 4, (1, 2): 5, (0, 3): 6, (1, 3): 7,
    }

    def cell_lit(cx: int, cy: int, dx: int, dy: int) -> bool:
        x = cx * 2 + dx
        y = cy * 4 + dy
        if x >= SOURCE_W or y >= SOURCE_H:
            return False
        r, g, b, a = rgba[x, y]
        if a < 80:
            return False
        # Treat anything darker than near-white as "lit". Both the navy body
        # and cyan squares qualify; the white background does not.
        return (r + g + b) / 3 < 235

    braille_lines: list[str] = []
    plain_lines: list[str] = []
    gradient_lines: list[str] = []

    for cy in range(LOGO_H_CHARS):
        b_row = ""
        p_row = ""
        for cx in range(LOGO_W_CHARS):
            mask = 0
            lit = 0
            for (dx, dy), bit in DOT_BITS.items():
                if cell_lit(cx, cy, dx, dy):
                    mask |= 1 << bit
                    lit += 1
            b_row += chr(0x2800 + mask)
            p_row += " ░▒▓█"[min(4, lit // 2)]
        braille_lines.append(b_row)
        plain_lines.append(p_row)
        gradient_lines.append(f"@{LOGO_GRADIENT[cy]}@\t{b_row}")

    return (
        "\n".join(braille_lines) + "\n",
        "\n".join(gradient_lines) + "\n",
        "\n".join(plain_lines) + "\n",
    )


def main() -> None:
    ASSETS.mkdir(parents=True, exist_ok=True)

    # ANSI Shadow — the chunky 3D blocky font Hermes uses; visual anchor of
    # the splash. Width: ~70 cols for "Rantaiclaw".
    full = render_banner("ansi_shadow", "RANTAICLAW")
    # `small` font as the medium fallback (~46 cols).
    small = render_banner("small", "Rantaiclaw")
    (ASSETS / "banner_full.txt").write_text(full, encoding="utf-8")
    (ASSETS / "banner_small.txt").write_text(small, encoding="utf-8")
    print(f"wrote banner_full.txt  ({len(full.splitlines())} lines, max width {max(len(l) for l in full.splitlines())})")
    print(f"wrote banner_small.txt ({len(small.splitlines())} lines, max width {max(len(l) for l in small.splitlines())})")

    braille, gradient, plain = render_braille()
    (ASSETS / "logo_braille.txt").write_text(braille, encoding="utf-8")
    (ASSETS / "logo_braille_gradient.txt").write_text(gradient, encoding="utf-8")
    (ASSETS / "logo_plain.txt").write_text(plain, encoding="utf-8")
    print(f"wrote logo_braille.txt ({len(braille.splitlines())} lines × {LOGO_W_CHARS} chars)")
    print(f"wrote logo_braille_gradient.txt (same shape with @N@ row tags)")
    print(f"wrote logo_plain.txt   (density fallback)")


if __name__ == "__main__":
    main()

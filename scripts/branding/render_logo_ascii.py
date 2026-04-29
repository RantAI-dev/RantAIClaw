#!/usr/bin/env python3
"""
Generate the RantaiClaw CLI splash assets.

Outputs three files under `src/onboard/assets/`:

* `banner.txt`   — figlet-style "RANTAICLAW" block-letter banner (raw text,
  no ANSI). The Rust side wraps it in our brand colors at render time so the
  asset stays toolchain-portable.

* `logo_ansi.txt` — colored half-block-character rendering of the brand logo
  (`Logo-only Border or Stroke (1).png`) sized for a left-side splash panel.
  ANSI 24-bit truecolor escapes are baked in; consoles without truecolor
  degrade gracefully via the closest 256-color match.

* `logo_plain.txt` — same logo as a glyph-only fallback (no ANSI) for
  monochrome terminals or when colors are disabled via `NO_COLOR`.

Run:

    python3 scripts/branding/render_logo_ascii.py

Re-run only when the source PNG or the desired dimensions change. The
generated assets are committed.
"""

from __future__ import annotations

import sys
from pathlib import Path

import pyfiglet  # type: ignore
from PIL import Image  # type: ignore

REPO_ROOT = Path(__file__).resolve().parents[2]
LOGO_SRC = REPO_ROOT.parent.parent / "Logo-only Border or Stroke (1).png"
ASSETS = REPO_ROOT / "src" / "onboard" / "assets"

# Brand palette — pulled from the rantai-agents web app.
#   navy   #040b2e  (logo body / dark accent)
#   sky    #5eb8ff  (logo squares / light accent)
#   blue   #3b8cff  (web --accent oklch(0.55 0.2 250))
#   bg     none — terminals provide it.
LOGO_W = 24   # final ASCII width in *characters*
LOGO_H = 12   # final ASCII height in *rows*; each row = 2 image pixels via half-blocks


def render_banner() -> str:
    """ANSI Shadow figlet — chunky 3D block letters that read as a logo."""
    fig = pyfiglet.figlet_format("RANTAICLAW", font="ansi_shadow")
    # pyfiglet pads with trailing newline; trim to a single trailing newline.
    return fig.rstrip("\n") + "\n"


def render_logo() -> tuple[str, str]:
    """
    Two-pass render:
      pass 1 — colored ANSI using upper-half-block (`▀`) so each character
               cell encodes two vertical pixels (foreground = top half,
               background = bottom half).
      pass 2 — glyph-only fallback using density mapping `█▓▒░ `.
    """
    if not LOGO_SRC.exists():
        sys.exit(f"missing source PNG: {LOGO_SRC}")

    img = Image.open(LOGO_SRC).convert("RGBA")
    # Preserve aspect by treating each output row as 2 image pixels.
    target_pixel_w = LOGO_W
    target_pixel_h = LOGO_H * 2
    img = img.resize((target_pixel_w, target_pixel_h), Image.LANCZOS)

    # Composite over the brand navy so transparent pixels read as
    # "background", not as the terminal's default which can be anything.
    bg = Image.new("RGB", img.size, (4, 11, 46))  # #040b2e
    bg.paste(img, mask=img.split()[3])
    pixels = bg.load()

    ansi_lines: list[str] = []
    plain_lines: list[str] = []
    for row in range(0, target_pixel_h, 2):
        ansi_row = ""
        plain_row = ""
        for col in range(target_pixel_w):
            top = pixels[col, row]
            bot = pixels[col, row + 1] if row + 1 < target_pixel_h else top

            # ANSI: 24-bit truecolor escape per cell.
            ansi_row += (
                f"\x1b[38;2;{top[0]};{top[1]};{top[2]}m"
                f"\x1b[48;2;{bot[0]};{bot[1]};{bot[2]}m"
                "▀"
            )

            # Plain glyph: density of the average luminance.
            avg = (sum(top) + sum(bot)) / 6
            plain_row += density_glyph(avg)
        ansi_row += "\x1b[0m"
        ansi_lines.append(ansi_row)
        plain_lines.append(plain_row)

    return "\n".join(ansi_lines) + "\n", "\n".join(plain_lines) + "\n"


def density_glyph(luminance: float) -> str:
    """Map 0-255 luminance to a Unicode block density character."""
    if luminance < 32:
        return " "
    if luminance < 80:
        return "░"  # ░
    if luminance < 144:
        return "▒"  # ▒
    if luminance < 208:
        return "▓"  # ▓
    return "█"      # █


def main() -> None:
    ASSETS.mkdir(parents=True, exist_ok=True)

    banner = render_banner()
    (ASSETS / "banner.txt").write_text(banner, encoding="utf-8")
    print(f"wrote {ASSETS / 'banner.txt'} ({len(banner.splitlines())} lines)")

    ansi, plain = render_logo()
    (ASSETS / "logo_ansi.txt").write_text(ansi, encoding="utf-8")
    (ASSETS / "logo_plain.txt").write_text(plain, encoding="utf-8")
    print(f"wrote {ASSETS / 'logo_ansi.txt'} ({len(ansi.splitlines())} lines, ANSI 24-bit)")
    print(f"wrote {ASSETS / 'logo_plain.txt'} ({len(plain.splitlines())} lines, glyph-only)")


if __name__ == "__main__":
    main()

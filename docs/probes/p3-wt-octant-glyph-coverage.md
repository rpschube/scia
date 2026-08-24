# P3 — Octant/sextant glyph coverage in Windows Terminal

Probe, not production. Goal: establish whether the mosaic ladder's finer
tiers (octants, sextants) actually render in the Windows Terminal
configuration scia targets, or whether the runtime glyph check must step the
ladder down.

## Setup

- Windows Terminal 1.24.11911.0, font face **FiraCode Nerd Font**.
- A printf of representative codepoints from each tier, viewed directly in
  the terminal: eighth-blocks (U+2581–2588), quadrants (U+2596–259F),
  sextants (U+1FB00–1FB3B sample), octants (U+1CD00–1CDE5 sample), braille.

## Results

| Tier | Codepoints | Renders? |
|---|---|---|
| Eighth/half blocks | U+2581–2588 | yes — correct mosaics |
| Quadrants | U+2596–259F | yes — correct mosaics |
| Sextants | U+1FB00… | **no — replacement glyphs** |
| Octants | U+1CD00… | **no — replacement glyphs** |
| Braille | U+28xx | yes |

## Reading

- Octants come from Cascadia Code ≥ 2404.23; a user font without them
  (FiraCode Nerd Font here) shows tofu, and Windows Terminal did not
  fall back to a glyph-bearing font.
- **Verdict: the runtime missing-glyph check is mandatory, not optional**
  (US-TUI-1 criterion confirmed on real hardware). On this — likely common —
  configuration the cell-mosaic ladder starts at **quadrant** (2×2), with
  octants available only after a font switch.
- The check must be per-font at startup (and after font changes), not a
  terminal-version test: the terminal supports the codepoints; the font
  decides.

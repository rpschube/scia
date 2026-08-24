# Preset format

A preset is a plain TOML file that fully describes one scene instance: the scene
type, its typed parameters, an optional layer stack, feature → parameter
mappings, and a palette source. Presets are validated with actionable errors
that name the file and line. Built-in presets are ordinary files on disk under
[`presets/`](../presets); copy one and edit it.

This is rung 0 of the scene engine: the library, the files and this reference.
See [What is not yet wired](#what-is-not-yet-wired) for the pieces that arrive
with later cards.

## Quick start

A minimal preset is just a name and a scene:

```toml
[preset]
name = "spectra"
scene = "spectra"
```

Everything else — parameters, layers, mappings, palette — is optional and falls
back to the scene's own defaults. The annotated
[`presets/spectra.toml`](../presets/spectra.toml) shows every key.

## Tables and keys

### `[preset]` (required)

| key           | type   | required | meaning                                              |
| ------------- | ------ | -------- | ---------------------------------------------------- |
| `name`        | string | yes      | The preset name. Must match `^[a-z0-9][a-z0-9-]*$`.  |
| `scene`       | string | yes      | A registered scene id (e.g. `spectra`).              |
| `description` | string | no       | A one-line human description.                        |
| `mood`        | string | no       | A one-word mood. Defaults to the scene's own mood.   |

### `[params]` (optional)

Typed parameters for the preset's scene. Every key must exist in that scene's
parameter manifest; every value is a TOML integer or float (read as `f32`) and
must lie within the parameter's `[min, max]` range. Keys you omit keep their
manifest default.

The `spectra` manifest:

| key           | default | range        | meaning                                       |
| ------------- | ------- | ------------ | --------------------------------------------- |
| `release`     | `0.15`  | `0.01..=2.0` | extra release time constant (seconds)         |
| `punch_decay` | `0.25`  | `0.01..=2.0` | onset-envelope decay time constant (seconds)  |
| `punch`       | `0.35`  | `0.0..=2.0`  | how much the envelope lifts the low bars      |
| `gap`         | `0.15`  | `0.0..=0.9`  | gap between bars, as a fraction of a column    |

```toml
[params]
release = 0.15
gap = 0.2
```

### `[[layer]]` (optional)

A layer stack. With **no** `[[layer]]` tables the preset is exactly one layer:
`[preset].scene` drawn with `[params]`. With one or more `[[layer]]` tables, each
becomes its own layer.

| key         | type   | required | default | meaning                                    |
| ----------- | ------ | -------- | ------- | ------------------------------------------ |
| `scene`     | string | yes      | —       | The scene this layer draws.                |
| `blend`     | string | no       | `over`  | Compositing: `over`, `add` or `max`.       |
| `intensity` | number | no       | `1.0`   | Layer intensity, `0.0..=1.0`.              |

`[layer.params]` is typed against **that layer's** scene manifest and merges over
its defaults. The full parameter precedence for a layer is:

```
manifest default  <  [params]  <  [layer.params]
```

so the top-level `[params]` acts as a shared base and each layer refines it.

```toml
[[layer]]
scene = "spectra"
blend = "add"
intensity = 0.8
[layer.params]
gap = 0.25
```

When layers are present, `[map]` targets the **first** layer's scene.

### `[map]` (optional)

Feature → parameter mappings. Each key is a parameter of the mapped scene (the
preset's scene, or the first layer's scene when there are layers). Each value is
a **table** describing how a feature drives that parameter:

```toml
[map]
punch   = { feature = "onset", curve = "linear", attack_ms = 0, decay_ms = 250, scale = 0.9, offset = 0.0 }
release = { feature = "bass",  curve = "pow", exponent = 2.0, attack_ms = 10, decay_ms = 120, scale = 0.3, offset = 0.05 }
```

| key         | type   | required                    | default | meaning                                  |
| ----------- | ------ | --------------------------- | ------- | ---------------------------------------- |
| `feature`   | string | yes                         | —       | The feature to read (see below).         |
| `curve`     | string | no                          | `linear`| The response curve (see below).          |
| `exponent`  | number | iff `curve = "pow"` (`> 0`) | —       | The power for `pow`.                     |
| `threshold` | number | iff `curve = "step"` (`0..=1`) | —    | The step point for `step`.               |
| `attack_ms` | number | no (`>= 0`)                 | `0`     | Envelope attack time, ms (`0` = instant).|
| `decay_ms`  | number | no (`>= 0`)                 | `0`     | Envelope decay time, ms (`0` = instant). |
| `scale`     | number | no                          | `1.0`   | Output scale.                            |
| `offset`    | number | no                          | `0.0`   | Output offset.                           |

A `[map]` value that is a **string** is reserved for rung-1 expression syntax and
is rejected today with a dedicated error (see the error table).

### `[palette]` (optional)

| key      | type            | required | meaning                                             |
| -------- | --------------- | -------- | --------------------------------------------------- |
| `source` | string          | yes      | `static` or `album-art`.                            |
| `slots`  | array of string | no       | Exactly 8 `"#rrggbb"` colours; only meaningful with `static`. |

`album-art` is accepted and validated, but resolves to the host default palette
until the album-art card lands. When `slots` is present it must hold exactly
eight `#rrggbb` entries; omit it to use the host default palette.

```toml
[palette]
source = "static"
slots = ["#0d3b3b", "#1f8f8f", "#3fd0d0", "#ffb020", "#ff6b5b", "#14161c", "#8a8f9c", "#e6e8ee"]
```

## Feature vocabulary

Each feature reads one field of the DSP `FeatureSnapshot` and is clamped to
`0.0..=1.0` before its curve.

| feature | snapshot field                                   | notes                                      |
| ------- | ------------------------------------------------ | ------------------------------------------ |
| `bass`  | `bands[0]`                                        | bass band level                            |
| `mid`   | `bands[1]`                                        | mid band level                             |
| `treb`  | `bands[2]`                                        | treble band level                          |
| `loud`  | `rms`                                              | loudness                                   |
| `peak`  | `peak`                                             | peak sample of the hop                     |
| `onset` | `onset`                                            | `1.0` on an onset hop, else `0.0`          |
| `flux`  | `flux`                                             | spectral flux                              |
| `beat`  | `beat_confidence`                                  | `0.0` until the beat-tracker card lands    |
| `width` | `((1 - stereo_correlation) / 2).clamp(0, 1)`       | stereo width                               |

## Curves

Applied to the clamped feature value `x`:

| curve    | formula                | parameter                        |
| -------- | ---------------------- | -------------------------------- |
| `linear` | `x`                    | —                                |
| `pow`    | `x^exponent`           | `exponent` required, `> 0`       |
| `log`    | `ln(1 + 9x) / ln 10`   | —                                |
| `step`   | `x >= threshold ? 1 : 0` | `threshold` required, `0..=1`  |

## Envelope semantics

After the curve, each mapping runs a first-order envelope follower toward the
curved target `x`, using the **attack** constant while the target is rising and
the **decay** constant while it is falling:

```
tau = (rising ? attack_ms : decay_ms) / 1000
y  += (x - y) * (1 - exp(-dt / tau))     # when tau > 0
y   = x                                   # when tau == 0 (instant)
```

The stored parameter value is `offset + scale * y`. A time constant `tau`
reaches `1 - e⁻¹ ≈ 63 %` of a step in one `tau`; a decay from `1.0` reaches
`e⁻¹ ≈ 37 %` after one `tau`.

## Palette rules

- `source` is `static` or `album-art`.
- `slots`, when present, must be exactly 8 entries, each a `"#rrggbb"` string
  (a leading `#` and six hex digits).
- `slots` only theme a `static` palette. An `album-art` preset with `slots` is
  still validated, but resolves to the host default until album-art palettes are
  wired.

## Error format

Every error's message begins with `<file>:<line>:<col>: ` (the line and column
are dropped only when the position is genuinely unknown, e.g. an IO error). One
example per class, as printed for a document read from `x.toml`:

| class                  | example message                                                                             |
| ---------------------- | ------------------------------------------------------------------------------------------- |
| unknown key            | `x.toml:4:1: unknown key `bogus` in [preset] (known: `name`, `scene`, `description`, `mood`)` |
| type mismatch          | `x.toml:5:11: `release`: expected number, found string`                                     |
| out of range           | `x.toml:5:7: `gap` = 5 is out of range [0, 0.9]`                                             |
| unknown scene          | `x.toml:3:9: unknown scene `nope` (known: `spectra`)`                                        |
| unknown feature        | `x.toml:5:9: unknown feature `nope``                                                         |
| expression string      | `x.toml:5:9: `punch`: expressions arrive with the expression VM; use a mapping table here`   |
| palette shape          | `x.toml:6:10: palette must have exactly 8 slots, found 7`                                    |
| invalid name           | `x.toml:2:8: invalid preset name `Bad Name`; must match ^[a-z0-9][a-z0-9-]*$`                |
| syntax                 | `x.toml:2:1: <the TOML parser's message>`                                                    |
| IO                     | `preset.toml: <the OS error>` (no line/col)                                                  |

## What is not yet wired

- **Selecting and cycling presets** — `scia --preset name` and runtime cycling
  land with the presenter / scene-browser cards. The TUI does not render scenes
  yet; this rung ships the library, the files and the docs.
- **Expressions** — string `[map]` values (a small expression language) arrive
  with the expression VM. Until then a mapping must be a table.
- **Album-art palettes** — `source = "album-art"` is accepted and validated but
  resolves to the host default palette until the album-art card lands.
- **`beat`** — reads `beat_confidence`, which is `0.0` until the beat-tracker
  card lands.

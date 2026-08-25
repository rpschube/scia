# Preset format

A preset is a plain TOML file that fully describes one scene instance: the scene
type, its typed parameters, an optional layer stack, feature → parameter
mappings, and a palette source. Presets are validated with actionable errors
that name the file and line. Built-in presets are ordinary files on disk under
[`presets/`](../presets); copy one and edit it.

This is rung 0 of the scene engine: the library, the files and this reference.
See [What is not yet wired](#what-is-not-yet-wired) for the pieces that arrive
with later cards.

New to authoring? Copy the fully commented skeleton
[`templates/preset-template.toml`](../templates/preset-template.toml) and read
the narrative guide [`docs/authoring.md`](authoring.md), which walks presets,
expressions and Luau scenes end to end. This file is the format reference they
point back to.

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

The `lattice` manifest:

| key          | default | range       | meaning                                               |
| ------------ | ------- | ----------- | ----------------------------------------------------- |
| `density`    | `24`    | `4.0..=96.0`| dots across the width (rows follow from the aspect)   |
| `ring_speed` | `0.9`   | `0.1..=4.0` | how fast a ring front travels (canvas units / second) |
| `ring_width` | `0.14`  | `0.02..=0.5`| thickness of the ring front (canvas units)            |
| `flash`      | `0.7`   | `0.0..=1.0` | ring brightness boost as its front passes a dot       |
| `glow`       | `0.35`  | `0.0..=1.0` | base dot intensity that loudness rides on             |

The `starfall` manifest:

| key      | default | range         | meaning                                                           |
| -------- | ------- | ------------- | ----------------------------------------------------------------- |
| `stars`  | `192`   | `16.0..=512.0`| number of stars in the pool, preallocated at init                 |
| `speed`  | `0.35`  | `0.05..=2.0`  | base outward speed (canvas units / second) that loudness rides on |
| `streak` | `0.6`   | `0.0..=2.0`   | streak-length gain on an onset (outer stars stretch into lines)   |
| `size`   | `1.0`   | `0.2..=3.0`   | star size multiplier                                              |
| `spread` | `0.6`   | `0.0..=1.0`   | spawn-direction spread: 1 spaces stars evenly, 0 scatters them    |

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
**either** a response **table** or a string **expression**.

#### Table form

A table describes how one feature drives the parameter through a curve and an
envelope:

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

#### Expression form

A string value is an algebraic expression, compiled once when the preset loads
and evaluated every frame:

```toml
[map]
flash = "onset * 0.9"
drift = "bass ^ 2"
band  = "0.1 + loud * 0.4"
```

The expression reads the storyboard variables below, all in `0..1` (except
`width`, which is the raw stereo field). Operators are `+ - * / % ^` with
parentheses, alongside `fasteval`'s builtin math functions (`sin`, `cos`, `abs`,
`min`, `max`, `log`, …). A syntax error or a reference to any name outside this
vocabulary fails at **load** time, at that entry's `file:line:col` (see the error
table).

| variable    | source                                | notes                                                        |
| ----------- | ------------------------------------- | ------------------------------------------------------------ |
| `bass`      | `bands[0]` clamped to `0..1`          | bass band level                                              |
| `mid`       | `bands[1]` clamped to `0..1`          | mid band level                                               |
| `treb`      | `bands[2]` clamped to `0..1`          | treble band level                                            |
| `loud`      | `rms`                                 | loudness                                                     |
| `peak`      | `peak`                                | peak sample of the hop                                       |
| `onset`     | onset **envelope**                    | `1.0` on an onset hop, then an exponential decay (τ ≈ 250 ms) so the expression sees a usable envelope, not a one-frame spike |
| `flux`      | `flux`                                | spectral flux                                                |
| `beat`      | `beat_phase`                          | position within the beat period, `0..1`                      |
| `beat_conf` | `beat_confidence`                     | beat-tracker confidence                                      |
| `width`     | `stereo_correlation` (raw)            | stereo width; `0.0` until the stereo card lands              |

A non-finite result (e.g. a division by zero) is sanitized to `0.0`. As with the
table form, the scene clamps the stored value to the parameter's `[min, max]` on
read, so an expression that overshoots is capped rather than rejected. Per-frame
evaluation is allocation-free.

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

Mappings are applied **live**, every frame: the mapped parameters are folded
into the layer's params and handed to the scene before it updates, so a mapped
value drives that same frame's render — a scene reads its tuning parameters both
at load and on every frame. Because `offset + scale * y` can leave the range the
preset validated at load, the scene clamps each value back into that parameter's
`[min, max]` when it reads it, so a mapping that overshoots is capped rather than
rejected.

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
| bad expression syntax  | `x.toml:5:9: `punch`: invalid expression: <the parser's message>`                            |
| unknown expr variable  | `x.toml:5:9: `punch`: unknown variable `nope` in expression (known: `bass`, `mid`, …)`       |
| palette shape          | `x.toml:6:10: palette must have exactly 8 slots, found 7`                                    |
| invalid name           | `x.toml:2:8: invalid preset name `Bad Name`; must match ^[a-z0-9][a-z0-9-]*$`                |
| syntax                 | `x.toml:2:1: <the TOML parser's message>`                                                    |
| IO                     | `preset.toml: <the OS error>` (no line/col)                                                  |

## Live editing

Run a preset straight from a file and edit it while it plays:

```sh
scia --demo --scene-file presets/spectra.toml
```

`--scene-file PATH` loads and validates the preset, renders it, and watches the
file. On every save the file is re-read and re-validated off the render thread,
and a good edit cross-fades in — the old scene fades out over 300 ms while the
new one fades in — so the change appears in well under half a second without
restarting or interrupting audio capture. Scene continuity carries across the
swap: a scene that snapshots state (spectra carries its onset envelope) resumes
rather than resetting.

A broken edit never blanks the screen. A syntax or validation error keeps the
last good scene running and surfaces the error's first line, dim, on the status
row; fix the file and the next save reloads cleanly. A successful reload briefly
notes its read-and-validate time on the same row.

`--scene-file` is mutually exclusive with `--scene` (which names a built-in
preset) and, like `--scene`, is not valid with `--headless`. A preset that fails
to load at startup exits with a usage error and the `file:line:col` message.

## What is not yet wired

- **Cycling presets** — a `--scene-file` preset hot-reloads on save (see
  [Live editing](#live-editing)), but runtime cycling between presets lands with
  the scene-browser card.
- **Album-art palettes** — `source = "album-art"` is accepted and validated but
  resolves to the host default palette until the album-art card lands.
- **`beat` / `beat_conf`** — the beat variables read `beat_phase` and
  `beat_confidence`; both come alive with the beat-tracker card.
- **`width`** — the expression `width` variable reads `stereo_correlation`, which
  is `0.0` until the stereo card lands.

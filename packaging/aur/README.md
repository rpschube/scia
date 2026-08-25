# AUR package for scia

A ready-to-submit [AUR](https://aur.archlinux.org/) `PKGBUILD` for `scia`. It is
a **template**: the version and source checksum are filled at release time.
Nothing here is submitted automatically — publishing to the AUR is a human step.

This is a **build-from-source** package (pkgname `scia`). Because Arch ships
ALSA and PipeWire, it builds with the `capture-pipewire` feature and captures
the system output mix — the full experience, with none of the musl static
caveats that apply to the generic Linux tarball.

## Files

| File | Purpose |
| --- | --- |
| `PKGBUILD` | the package recipe (build-from-source) |
| `.SRCINFO` | generated metadata AUR requires; must match `PKGBUILD` |

## Placeholders to fill

| Placeholder | Source |
| --- | --- |
| `__VERSION__` | the released version without the leading `v` (e.g. `1.2.0`) |
| `__SHA256__` | sha256 of the GitHub source tarball — `updpkgsums` computes it |

## Submit steps (human, after a release is published)

1. Cut the release (`git tag vX.Y.Z && git push origin vX.Y.Z`). The tag makes
   the source tarball at
   `https://github.com/rpschube/scia/archive/refs/tags/vX.Y.Z.tar.gz`
   available.
2. In a copy of this directory, set `pkgver` and run:
   ```
   updpkgsums                       # fills sha256sums from the real tarball
   makepkg --printsrcinfo > .SRCINFO
   ```
3. Build and smoke-test locally in a clean chroot:
   ```
   makepkg -si                      # or: extra-x86_64-build
   scia --demo                      # smoke test
   ```
4. Push to the AUR. The remote uses the `aur` SSH user against the AUR host
   (see the AUR submission guidelines):
   ```
   aur_host=aur.archlinux.org
   git clone "ssh://aur@${aur_host}/scia.git" aur-scia
   cp PKGBUILD .SRCINFO aur-scia/
   cd aur-scia && git commit -am "scia X.Y.Z" && git push
   ```
   (First-time submission requires an AUR account with an SSH key registered.)

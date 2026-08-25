# winget manifests for scia

Ready-to-submit [winget](https://learn.microsoft.com/windows/package-manager/)
manifests for the `rpschube.scia` package. These are **templates**: the version,
hash and release date are filled at release time. Nothing here is submitted
automatically — publishing to the community repository is a human step.

## Files

| File | Manifest type |
| --- | --- |
| `rpschube.scia.yaml` | version |
| `rpschube.scia.installer.yaml` | installer |
| `rpschube.scia.locale.en-US.yaml` | default locale |

The installer is the release asset `scia-x86_64-pc-windows-msvc.zip`, a flat zip
holding the single portable `scia.exe`. winget installs it as a portable command
aliased `scia`.

## Placeholders to fill

| Placeholder | Source |
| --- | --- |
| `__VERSION__` | the released version without the leading `v` (e.g. `1.2.0`) |
| `__SHA256__` | contents of the release asset `scia-x86_64-pc-windows-msvc.zip.sha256` (uppercase hex) |
| `__RELEASE_DATE__` | the release date, `YYYY-MM-DD` |

## Submit steps (human, after a release is published)

1. Cut the release (`git tag vX.Y.Z && git push origin vX.Y.Z`). Wait for
   `release.yml` to publish `scia-x86_64-pc-windows-msvc.zip` and its
   `.sha256`.
2. Copy the three manifests, filling every `__PLACEHOLDER__`. winget expects
   them under `manifests/r/rpschube/scia/X.Y.Z/`.
3. Validate locally (on Windows, with the App Installer / winget CLI):
   ```
   winget validate --manifest <dir>
   winget install --manifest <dir>     # optional local install smoke test
   ```
4. Submit to the community repository, either:
   - `wingetcreate submit --token <gh-token> <dir>`, or
   - fork [microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs),
     add the manifests under `manifests/r/rpschube/scia/X.Y.Z/`, and open a PR.
5. The winget-pkgs CI validates the manifests and, once merged, `scia` is
   installable via `winget install rpschube.scia`.

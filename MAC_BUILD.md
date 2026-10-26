# macOS build (File Namer AI)

The Mac version is built on a real Mac runner via GitHub Actions — the
same pattern as SmartLifeTuyaDesktop. One version, both platforms: the
Windows installer and the Mac DMG ship under the same release tag, and
the in-app updater picks the right asset per platform (`FileNamerAI-Setup-vX.Y.Z.exe`
on Windows, `FileNamerAI-vX.Y.Z.dmg` on Mac).

## One-time setup (web UI, ~2 minutes)

The workflow file can't be pushed by automation (it needs the `workflow`
token scope), so add it by hand once:

1. On GitHub, open the repo → **Add file → Create new file**.
2. Name it `.github/workflows/release-mac.yml`.
3. Paste the entire contents of `mac-build/release-mac.yml` from this repo.
4. Commit directly to `master`.

After that, every published release automatically builds and uploads the
Mac DMG alongside the Windows installer. The DMG is unsigned (ad-hoc
codesign), so first launch needs a one-time Privacy & Security approval,
same as the Tuya app: macOS refuses the first double-click — open
**System Settings → Privacy & Security**, click **Open Anyway** next to
the app, authenticate, and confirm **Open**. (Terminal alternative:
`xattr -dr com.apple.quarantine /Applications/FileNamerAI.app`.)

## Building the DMG for an existing release

If a release went out before the workflow existed (e.g. v1.3.16):

1. Repo → **Actions → macOS DMG → Run workflow**.
2. Enter the existing tag (e.g. `v1.3.16`) and run it.
3. The DMG uploads to that same release — no new version needed.

## What the Mac version does differently

- **No right-click menu**: Finder integration is a different system from
  Windows shell verbs, so the Settings section shows a note instead.
  Open the app and use it directly, or drag files onto it.
- **Config location**: `~/Library/Application Support/FileNamerAI/FileNamerAI.ini`
  (Windows keeps the `.ini` beside the exe).
- **Ollama install**: the Mac build links to ollama.com instead of
  auto-downloading an installer (Ollama for Mac ships as a `.zip` app).
- **Updates**: downloads the DMG and opens it; drag the new version to
  Applications. No auto-replace like the Windows installer.
- **Dark mode**: follows macOS Appearance via `defaults read -g AppleInterfaceStyle`.
- Everything else is identical: same naming, same date rules, same
  models, same batch mode, same version number.

## Why the Mac build can't run from this Linux VM

Cross-checking for macOS from Linux fails on the `ring` C dependency
(rustls): it passes macOS-only compiler flags (`-arch arm64`,
`-mmacosx-version-min=11.0`) that Linux `cc` rejects. The Mac runner has
the real toolchain, so the workflow is the verification step — if it
goes green, the port compiles.

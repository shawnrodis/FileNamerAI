# File Namer AI

<p align="center">
  <img src="assets/icon.png" alt="FileNamerAI app icon" width="120">
</p>

Right-click any file → **"Rename with File Namer AI"**. **Photos and videos**
are renamed deterministically from their metadata — no AI needed — using
automatic date-based names (`MM-DD-YY-HH-MM-SS`). **Documents** get a descriptive
name proposed by a local AI (Ollama). 100% local: no cloud, no API keys,
no subscriptions.

Built with [Rust](https://www.rust-lang.org/) and [egui](https://www.egui.rs/).
Distributed as an installer, no console window. Runs as a normal user
(no administrator rights needed).

## Screenshot

<p align="center">
  <img src="assets/screenshot.png" alt="The File Namer AI settings window on Windows (v1.3.19): update check, Ollama connection, in-app model downloads, and right-click menu" width="520">
  <br><em>The settings window on Windows (v1.3.19) — updates at the top, models downloadable in-app, right-click menu installed.</em>
</p>

<p align="center">
  <img src="assets/screenshot-v1.3.19.png" alt="The File Namer AI settings window on Mac (v1.3.19): update check, Ollama connection, in-app model downloads, and the advanced photo/video naming section" width="520">
  <br><em>The settings window on Mac (v1.3.19) — with the advanced photo/video naming section expanded.</em>
</p>

## Download

Grab the latest release from
[Releases](https://github.com/shawnrodis/FileNamerAI/releases) — one
version, both platforms:

- **Windows:** `FileNamerAI-Setup-vX.Y.Z.exe` installer — per-user
  install, no admin rights needed. It places the app in
  `%LOCALAPPDATA%\FileNamerAI`, registers the right-click menu, and adds
  an uninstaller.
- **Mac:** `FileNamerAI-vX.Y.Z.dmg` (universal: Apple Silicon + Intel) —
  open it, drag File Namer AI to Applications. (Building from source is
  optional; instructions below.)

  **First launch (unsigned app):** the DMG is unsigned (no Apple Developer
  certificate), so macOS refuses the first double-click and offers to move
  it to Trash — don't. Instead open **System Settings → Privacy &
  Security**, scroll to the Security section, and click **Open Anyway**
  next to the app, then authenticate (Touch ID or password) and confirm
  **Open**. After that it opens normally. (Terminal alternative, before
  first launch: `xattr -dr com.apple.quarantine /Applications/FileNamerAI.app`;
  on older macOS versions, right-click → Open does the same job.)

## How it works

1. Run the app once and click **"Add to right-click menu"** (writes a
   per-user registry entry — no admin needed).
2. For documents: install [Ollama](https://ollama.com), then
   `ollama pull qwen2.5:3b` (a ~2GB download, one time). Not needed for
   photos/videos.
3. Right-click any file → "Rename with File Namer AI" (select several files
   to rename them all in one batch window).
4. A confirm dialog shows the proposed name — edit it if you like, then Rename.
   Files that are already correctly named are detected and skipped.

Supported files:
- **Documents** (PDF, TXT/MD/CSV/LOG/JSON/XML/HTML): the local AI reads the
  content and proposes a descriptive Title Case name, with the date appended
  as `DD-Mon-YYYY` (e.g. `Saw Palmetto Price Comparison_28-May-2023`) when the
  content has a verifiable date. Dates the AI invents are stripped; if no date
  is found, the file's own date is used. A "Never add dates" option disables
  dates entirely.
- **Photos** (JPG/PNG/TIF/WebP/HEIC…): renamed by **EXIF Date Taken** with full
  seconds — e.g. `01-10-26-20-10-12.jpg`. Photos *without* real EXIF data
  (screenshots, Messenger downloads) fall back to file dates truncated to the
  minute, e.g. `01-11-26-08-48-00.jpg` — matching Bulk Rename Utility's
  "Item Date" behavior. No AI involved.
- **Videos** (MP4/MOV/M4V/3GP/AVI/MKV/WebM): renamed by the video's embedded
  media-created date (converted to local time), else file dates — always
  truncated to the minute (`:00` seconds), e.g. `01-01-26-15-07-00.mp4`.
  No AI involved.

Name collisions (burst shots sharing a timestamp) get automatic `_1`, `_2`
suffixes automatically. The earliest timestamp wins when sources disagree,
since file operations happen at or after capture.

Note: on Windows 11 the menu item lives under "Show more options".

## Based on Bulk Rename Utility

The photo/video naming logic is designed to match
[Bulk Rename Utility](https://www.bulkrenameutility.co.uk/) using these two
saved profiles (included in [`bulk-rename-utility-profiles/`](bulk-rename-utility-profiles/)):

- **File Name By Date Taken (Pictures)** — photos with real EXIF Date Taken
  keep full seconds.
- **File Name By Item Date (Videos & Screenshots)** — videos and photos
  without real EXIF data are truncated to the minute (`:00` seconds).

If you use Bulk Rename Utility, these profiles produce identical names to File Namer AI.

## Build from source

On Windows (install Rust via [rustup](https://rustup.rs/) first):

```powershell
cargo build --release
```

Cross-compile from Linux:

```bash
sudo apt install mingw-w64
rustup target add x86_64-pc-windows-gnu
# make sure a `windres` binary is on PATH, e.g.:
#   sudo ln -s /usr/bin/x86_64-w64-mingw32-windres /usr/local/bin/windres
cargo build --release --target x86_64-pc-windows-gnu
```

On a Mac (install Rust via [rustup](https://rustup.rs/) first):

```bash
cargo build --release
```

The Mac DMG is built by GitHub Actions (see `MAC_BUILD.md`) — the
workflow builds both Apple architectures, lipos them into one universal
binary, and uploads the DMG to the same release as the Windows installer.
Cross-checking macOS from Linux doesn't work: the `ring` C dependency
passes macOS-only compiler flags that Linux `cc` rejects.

The build script compiles `assets/FileNamerAI.rc` with windres and links the
resulting object directly, embedding the exe icon and the application manifest.

## Project layout

```
FileNamerAI/
├── src/main.rs            # the whole app (egui)
├── build.rs               # embeds icon + manifest via windres
├── assets/
│   ├── FileNamerAI.ico          # exe / window icon
│   ├── FileNamerAI.manifest     # asInvoker manifest
│   └── icon.png                 # runtime window icon (from the .ico)
└── Cargo.toml
```

## License

GPL-3.0 — see [LICENSE](LICENSE). Copyright (c) 2026 Shawn Rodis.

# File Namer AI

Right-click any file → **"Rename with File Namer AI"** — and a local AI
proposes a better filename based on the file's contents, with a date appended
when the content has one. 100% local: no cloud, no API keys, no subscriptions.

Built with [Rust](https://www.rust-lang.org/) and [egui](https://www.egui.rs/).
Single `.exe`, no installer, no console window. Runs as a normal user
(no administrator rights needed).

## Download

Grab the latest `FileNamerAI.exe` from
[Releases](https://github.com/shawnrodis/FileNamerAI/releases) — no install
needed, just run it. (Building from source is optional; instructions below.)

## How it works

1. Run the app once and click **"Add to right-click menu"** (writes a
   per-user registry entry — no admin needed).
2. Install [Ollama](https://ollama.com), then `ollama pull qwen2.5:3b`
   (a ~2GB download, one time).
3. Right-click any file → "Rename with File Namer AI".
4. The app extracts the file's text, asks your local model for a concise
   descriptive name, and shows a confirm dialog — edit the name if you like,
   then Rename.

Supported files: PDF (text-based), TXT/MD/CSV/LOG/JSON/XML/HTML, and images
(named from EXIF date, e.g. `IMG_19-Oct-2026`). Dates found in content are
appended as `_DD-Mon-YYYY`.

Note: on Windows 11 the menu item lives under "Show more options".

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

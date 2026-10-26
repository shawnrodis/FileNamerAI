#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! File Namer AI - right-click AI file renaming powered by a local LLM (Ollama).

use eframe::egui;
use chrono::Datelike;
use chrono::Timelike;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Hide the console window for spawned helper commands. Windows-only:
/// macOS/Linux GUI apps have no console to hide.
#[cfg(windows)]
fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x08000000);
}
#[cfg(not(windows))]
fn no_window(_cmd: &mut Command) {}
const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
const DEFAULT_MODEL: &str = "qwen2.5:3b";
const DEFAULT_VISION_MODEL: &str = "moondream";
const MAX_CONTENT_CHARS: usize = 4000;
const MENU_KEY: &str = r"HKCU\Software\Classes\*\shell\FileNamerAI";

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Config {
    ollama_url: String,
    model: String,
    vision_model: String,
    append_date: bool,
    never_dates: bool,
    force_ocr: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ollama_url: DEFAULT_OLLAMA_URL.into(),
            model: DEFAULT_MODEL.into(),
            vision_model: DEFAULT_VISION_MODEL.into(),
            append_date: true,
            never_dates: false,
            force_ocr: false,
        }
    }
}

fn config_path() -> PathBuf {
    if cfg!(windows) {
        let mut p = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
        p.set_file_name("FileNamerAI.ini");
        p
    } else {
        // macOS: ~/Library/Application Support/FileNamerAI/FileNamerAI.ini
        // (writing beside the binary would dirty the .app bundle).
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join("Library/Application Support/FileNamerAI/FileNamerAI.ini")
    }
}

fn load_config() -> Config {
    let mut cfg = Config::default();
    let text = std::fs::read_to_string(config_path()).unwrap_or_default();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "ollama_url" => cfg.ollama_url = v.trim().to_string(),
                "model" => cfg.model = v.trim().to_string(),
                "vision_model" => cfg.vision_model = v.trim().to_string(),
                "append_date" => cfg.append_date = v.trim() != "false",
                "never_dates" => cfg.never_dates = v.trim() == "true",
                "force_ocr" => cfg.force_ocr = v.trim() == "true",
                _ => {}
            }
        }
    }
    if cfg.ollama_url.is_empty() {
        cfg.ollama_url = DEFAULT_OLLAMA_URL.into();
    }
    if cfg.model.is_empty() {
        cfg.model = DEFAULT_MODEL.into();
    }
    if cfg.vision_model.is_empty() {
        cfg.vision_model = DEFAULT_VISION_MODEL.into();
    }
    cfg
}

fn save_config(cfg: &Config) {
    let s = format!(
        "ollama_url={}\nmodel={}\nvision_model={}\nappend_date={}\nnever_dates={}\nforce_ocr={}\n",
        cfg.ollama_url.trim(),
        cfg.model.trim(),
        cfg.vision_model.trim(),
        cfg.append_date,
        cfg.never_dates,
        cfg.force_ocr
    );
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, s);
}

fn is_dark_mode() -> bool {
    if cfg!(windows) {
        let mut cmd = Command::new("reg");
        cmd.args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
            "/v",
            "AppsUseLightTheme",
        ]);
        no_window(&mut cmd);
        match cmd.output() {
            Ok(o) => {
                let t = String::from_utf8_lossy(&o.stdout);
                t.lines()
                    .any(|l| l.contains("AppsUseLightTheme") && l.trim_end().ends_with("0x0"))
            }
            Err(_) => false,
        }
    } else if cfg!(target_os = "macos") {
        // AppleInterfaceStyle is "Dark" in dark mode; the key is absent
        // in light mode (defaults errors, which we treat as light... but
        // the app defaults to dark visuals, so fall back to dark).
        match Command::new("defaults")
            .args(["read", "-g", "AppleInterfaceStyle"])
            .output()
        {
            Ok(o) => String::from_utf8_lossy(&o.stdout).trim() == "Dark",
            Err(_) => true,
        }
    } else {
        true
    }
}

// ---------------------------------------------------------------------------
// content extraction
// ---------------------------------------------------------------------------

enum Extracted {
    /// Text for the AI to read (documents).
    Text(String),
    /// Deterministic date-based naming, no AI (photos, videos), plus the
    /// human-readable source of the date ("EXIF date taken", ...).
    DateTime(chrono::NaiveDateTime, &'static str),
}

fn parse_exif_dt(s: &str) -> Option<chrono::NaiveDateTime> {
    let s = s.trim().trim_matches('"').trim();
    // display_value() may render datetimes with colons (2026:01:01 12:05:40)
    // or dashes (2026-01-01 12:05:40) depending on the tag/writer.
    for fmt in ["%Y:%m:%d %H:%M:%S", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt);
        }
    }
    None
}

/// True if the file has a real DateTimeOriginal EXIF tag (0x9003).
/// Other EXIF date tags (0x9004 Digitized, 0x0132 DateTime) don't count —
/// they're often just modification stamps on downloads/screenshots.
fn has_datetime_original(path: &Path) -> bool {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut reader = BufReader::new(file);
    match exif::Reader::new().read_from_container(&mut reader) {
        Ok(exif) => exif.fields().any(|f| f.tag.number() == 0x9003),
        Err(_) => false,
    }
}

fn exif_datetime(path: &Path) -> Option<chrono::NaiveDateTime> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;
    // Match by tag NUMBER across every IFD: some writers store EXIF tags
    // (e.g. DateTimeOriginal) as plain TIFF tags instead of in the EXIF IFD.
    // 0x9003 = DateTimeOriginal, 0x9004 = DateTimeDigitized, 0x0132 = DateTime.
    let mut cands: Vec<(u8, String)> = Vec::new();
    for f in exif.fields() {
        match f.tag.number() {
            0x9003 => cands.push((0, f.display_value().to_string())),
            0x9004 => cands.push((1, f.display_value().to_string())),
            0x0132 => cands.push((2, f.display_value().to_string())),
            _ => {}
        }
    }
    cands.sort_by_key(|(prio, _)| *prio);
    cands.into_iter().find_map(|(_, s)| parse_exif_dt(&s))
}

/// Best available timestamp: EXIF (photos) / embedded media date (videos),
/// then file Created, then Modified, then now. Returns the source label too.
///
/// The EARLIEST timestamp wins: file operations (copy, rename, edit) happen
/// at or after capture, so the earliest date is closest to the original.
fn best_datetime(path: &Path, is_video: bool) -> (chrono::NaiveDateTime, &'static str) {
    let mut cands: Vec<(chrono::NaiveDateTime, &'static str)> = Vec::new();
    // Truncation rule (per Shawn 2026-10-22, BRU parity):
    // - Videos: always truncate to :00 (BRU Item Date).
    // - Photos with real EXIF: keep full seconds (BRU Date Taken).
    // - Photos without EXIF (screenshots, Messenger downloads): truncate to :00.
    // Applied before comparison so priority order is preserved.
    if is_video {
        if let Some(dt) = mp4_creation_datetime(path) {
            cands.push((dt, "video media date"));
        }
    } else if let Some(dt) = exif_datetime(path) {
        cands.push((dt, "EXIF date taken"));
    }
    // Only a real DateTimeOriginal (0x9003) exempts a photo from truncation.
    // Generic EXIF date stamps (0x9004/0x0132) on downloads don't count.
    let has_real_exif = !is_video && has_datetime_original(path);
    let truncate = is_video || !has_real_exif;
    let trunc = |dt: chrono::NaiveDateTime| -> chrono::NaiveDateTime {
        if truncate {
            dt.with_second(0)
                .and_then(|d| d.with_nanosecond(0))
                .unwrap_or(dt)
        } else {
            dt
        }
    };
    for (dt, _) in cands.iter_mut() {
        *dt = trunc(*dt);
    }
    if let Ok(md) = std::fs::metadata(path) {
        if let Ok(st) = md.created() {
            let dt: chrono::DateTime<chrono::Local> = st.into();
            cands.push((trunc(dt.naive_local()), "file created date"));
        }
        if let Ok(st) = md.modified() {
            let dt: chrono::DateTime<chrono::Local> = st.into();
            cands.push((trunc(dt.naive_local()), "file modified date"));
        }
    }
    cands
        .into_iter()
        .min_by_key(|(dt, _)| *dt)
        .unwrap_or_else(|| {
            (
                chrono::Local::now().naive_local(),
                "current time (no date found)",
            )
        })
}

/// Iterate MP4 boxes: returns (type, content_start, next_pos).
/// Handles 32-bit sizes, 64-bit largesize (size == 1), and size == 0 (to EOF).
fn next_box(data: &[u8], pos: usize) -> Option<([u8; 4], usize, usize)> {
    if pos + 8 > data.len() {
        return None;
    }
    let mut size = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as u64;
    let typ: [u8; 4] = data[pos + 4..pos + 8].try_into().ok()?;
    let mut header = 8usize;
    if size == 1 {
        if pos + 16 > data.len() {
            return None;
        }
        size = u64::from_be_bytes(data[pos + 8..pos + 16].try_into().ok()?);
        header = 16;
    } else if size == 0 {
        size = (data.len() - pos) as u64;
    }
    if size < header as u64 {
        return None;
    }
    let next = pos.checked_add(size as usize)?;
    if next > data.len() {
        return None;
    }
    Some((typ, pos + header, next))
}

fn find_box_content<'a>(data: &'a [u8], want: &[u8; 4]) -> Option<&'a [u8]> {
    let mut pos = 0;
    while let Some((typ, content_start, next_pos)) = next_box(data, pos) {
        if &typ == want {
            return Some(&data[content_start..next_pos]);
        }
        pos = next_pos;
    }
    None
}

/// Creation time from an mvhd or mdhd box (same FullBox layout).
/// QuickTime epoch is 1904-01-01; a zero value means "not set".
fn parse_box_time(body: &[u8]) -> Option<chrono::NaiveDateTime> {
    const QT_TO_UNIX: i64 = 2_082_844_800; // seconds between 1904-01-01 and 1970-01-01
    if body.len() < 4 {
        return None;
    }
    let creation: u64 = if body[0] == 1 {
        if body.len() < 12 {
            return None;
        }
        u64::from_be_bytes(body[4..12].try_into().ok()?)
    } else {
        if body.len() < 8 {
            return None;
        }
        u32::from_be_bytes(body[4..8].try_into().ok()?) as u64
    };
    if creation == 0 {
        return None;
    }
    // from_timestamp gives DateTime<Utc>; naive_local() on that would just
    // return the UTC wall time, so convert to Local first.
    chrono::DateTime::from_timestamp(creation as i64 - QT_TO_UNIX, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).naive_local())
}

/// Embedded creation time from an MP4/MOV/M4V/3GP: moov/mvhd first,
/// then moov/trak/mdia/mdhd as fallback.
fn mp4_creation_datetime(path: &Path) -> Option<chrono::NaiveDateTime> {
    let data = std::fs::read(path).ok()?;
    let moov = find_box_content(&data, b"moov")?;
    if let Some(mvhd) = find_box_content(moov, b"mvhd") {
        if let Some(dt) = parse_box_time(mvhd) {
            return Some(dt);
        }
    }
    let mut pos = 0;
    while let Some((typ, cs, next)) = next_box(moov, pos) {
        if &typ == b"trak" {
            let trak = &moov[cs..next];
            if let Some(mdia) = find_box_content(trak, b"mdia") {
                if let Some(mdhd) = find_box_content(mdia, b"mdhd") {
                    if let Some(dt) = parse_box_time(mdhd) {
                        return Some(dt);
                    }
                }
            }
            break; // first track is enough
        }
        pos = next;
    }
    None
}

/// Unescape the XML entities found in Office document XML, including
/// numeric character references (&#65; / &#x41;). `&amp;` is decoded last
/// so a doubly-escaped sequence does not get decoded twice.
fn unescape_xml_entities(s: &str) -> String {
    // Numeric references first.
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        match after.find(';') {
            Some(semi) if semi <= 10 => {
                let body = &after[..semi];
                let decoded = if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")) {
                    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                } else if let Some(dec) = body.strip_prefix('#') {
                    dec.parse::<u32>().ok().and_then(char::from_u32)
                } else {
                    None
                };
                match decoded {
                    Some(c) => out.push(c),
                    None => {
                        out.push('&');
                        out.push_str(body);
                        out.push(';');
                    }
                }
                rest = &after[semi + 1..];
            }
            _ => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Strip XML tags from Office document XML, keeping paragraph/row breaks.
/// Text nodes that were separated only by a tag get a space, so
/// `<t>foo</t><t>bar</t>` becomes "foo bar", not "foobar".
fn xml_to_plain_text(xml: &str) -> String {
    let mut out = String::new();
    let mut rest = xml;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let after = &rest[lt + 1..];
        let Some(gt) = after.find('>') else { break };
        let tag = after[..gt].trim();
        let (is_close, name) = match tag.strip_prefix('/') {
            Some(n) => (true, n.split_whitespace().next().unwrap_or("")),
            None => (
                false,
                tag.split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_end_matches('/'),
            ),
        };
        if name == "br" || name == "w:br" || name.ends_with(":br") {
            out.push('\n');
        } else if is_close
            && (name == "p"
                || name.ends_with(":p")
                || name == "tr"
                || name.ends_with(":tr")
                || name == "si"
                || name == "row")
        {
            out.push('\n');
        } else {
            out.push(' ');
        }
        rest = &after[gt + 1..];
    }
    out.push_str(rest);
    let unescaped = unescape_xml_entities(&out);
    unescaped
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn read_zip_entry_text(path: &Path, entry: &str) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let mut entry_file = archive.by_name(entry).ok()?;
    let mut xml = String::new();
    use std::io::Read as _;
    entry_file.read_to_string(&mut xml).ok()?;
    Some(xml)
}

fn zip_entry_names(path: &Path) -> Vec<String> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let Ok(archive) = zip::ZipArchive::new(file) else {
        return Vec::new();
    };
    archive.file_names().map(|s| s.to_string()).collect()
}

/// Plain text from one main ZIP/XML entry, plus any extra entries whose
/// names start with one of `extra_prefixes` (e.g. DOCX headers/footers).
fn office_zip_text(path: &Path, main: &str, extra_prefixes: &[&str]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(xml) = read_zip_entry_text(path, main) {
        let plain = xml_to_plain_text(&xml);
        if !plain.trim().is_empty() {
            parts.push(plain);
        }
    }
    if !extra_prefixes.is_empty() {
        for name in zip_entry_names(path) {
            if name != main
                && name.ends_with(".xml")
                && extra_prefixes.iter().any(|p| name.starts_with(p))
            {
                if let Some(xml) = read_zip_entry_text(path, &name) {
                    let plain = xml_to_plain_text(&xml);
                    if !plain.trim().is_empty() {
                        parts.push(plain);
                    }
                }
            }
        }
    }
    parts.join("\n")
}

/// XLSX text: shared strings first (the actual cell text); fall back to
/// sheet XML, which at least yields the stored values.
fn xlsx_text(path: &Path) -> String {
    let shared = office_zip_text(path, "xl/sharedStrings.xml", &[]);
    if !shared.trim().is_empty() {
        return shared;
    }
    let mut parts: Vec<String> = Vec::new();
    let mut names = zip_entry_names(path);
    names.sort();
    for name in names {
        if name.starts_with("xl/worksheets/sheet") && name.ends_with(".xml") {
            if let Some(xml) = read_zip_entry_text(path, &name) {
                let plain = xml_to_plain_text(&xml);
                if !plain.trim().is_empty() {
                    parts.push(plain);
                }
            }
        }
    }
    parts.join("\n")
}

/// PPTX text: every slide, in slide order.
fn pptx_text(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut names = zip_entry_names(path);
    names.sort();
    for name in names {
        if name.starts_with("ppt/slides/slide") && name.ends_with(".xml") {
            if let Some(xml) = read_zip_entry_text(path, &name) {
                let plain = xml_to_plain_text(&xml);
                if !plain.trim().is_empty() {
                    parts.push(plain);
                }
            }
        }
    }
    parts.join("\n")
}

/// Best available timestamp: EXIF (photos) / embedded media date (videos),
/// then file Created, then Modified, then now.
fn extract_content(path: &Path) -> Result<Extracted, String> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "pdf" => {
            let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read file: {e}"))?;
            let text =
                pdf_extract::extract_text_from_mem(&bytes).map_err(|e| format!("Couldn't read PDF: {e}"))?;
            let text: String = text.chars().take(MAX_CONTENT_CHARS).collect();
            if text.trim().is_empty() {
                return Err("This PDF has no readable text (it may be scanned images).".into());
            }
            Ok(Extracted::Text(text))
        }
        // Office documents are ZIP/XML: DOCX word/document.xml (+
        // headers/footers), ODT content.xml, XLSX shared strings,
        // PPTX slide XML. The extracted text then goes through the
        // same Ollama naming, date guard, and title guard as PDFs.
        "docx" => {
            let text = office_zip_text(path, "word/document.xml", &["word/header", "word/footer"]);
            let text: String = text.chars().take(MAX_CONTENT_CHARS).collect();
            if text.trim().is_empty() {
                return Err("This document has no readable text.".into());
            }
            Ok(Extracted::Text(text))
        }
        "odt" => {
            let text = office_zip_text(path, "content.xml", &[]);
            let text: String = text.chars().take(MAX_CONTENT_CHARS).collect();
            if text.trim().is_empty() {
                return Err("This document has no readable text.".into());
            }
            Ok(Extracted::Text(text))
        }
        "xlsx" => {
            let text = xlsx_text(path);
            let text: String = text.chars().take(MAX_CONTENT_CHARS).collect();
            if text.trim().is_empty() {
                return Err("This document has no readable text.".into());
            }
            Ok(Extracted::Text(text))
        }
        "pptx" => {
            let text = pptx_text(path);
            let text: String = text.chars().take(MAX_CONTENT_CHARS).collect();
            if text.trim().is_empty() {
                return Err("This document has no readable text.".into());
            }
            Ok(Extracted::Text(text))
        }
        "txt" | "md" | "markdown" | "csv" | "log" | "json" | "xml" | "html" | "htm" => {
            let text =
                std::fs::read_to_string(path).map_err(|e| format!("Couldn't read file: {e}"))?;
            let text: String = text.chars().take(MAX_CONTENT_CHARS).collect();
            if text.trim().is_empty() {
                return Err("The file is empty.".into());
            }
            Ok(Extracted::Text(text))
        }
        "jpg" | "jpeg" | "png" | "tif" | "tiff" | "webp" | "heic" | "heif" => {
            // Photos: EXIF "Date Taken", else file dates. No AI.
            let (dt, src) = best_datetime(path, false);
            Ok(Extracted::DateTime(dt, src))
        }
        "mp4" | "mov" | "m4v" | "3gp" | "avi" | "mkv" | "webm" => {
            // Videos: embedded media-created date, else file dates. No AI.
            let (dt, src) = best_datetime(path, true);
            Ok(Extracted::DateTime(dt, src))
        }
        _ => {
            // Any other file type: if it reads as plain text, name it
            // from its contents like a text document. Otherwise, don't
            // error out for the extension alone — fall back to
            // date-based naming from the file's own dates, like
            // photos/videos (earliest of created/modified, via
            // best_datetime).
            if let Ok(text) = std::fs::read_to_string(path) {
                let text: String = text.chars().take(MAX_CONTENT_CHARS).collect();
                if !text.trim().is_empty() {
                    return Ok(Extracted::Text(text));
                }
            }
            let (dt, src) = best_datetime(path, false);
            Ok(Extracted::DateTime(dt, src))
        }
    }
}

// ---------------------------------------------------------------------------
// ollama
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// model management — download models in-app via Ollama's API, no terminal
// ---------------------------------------------------------------------------

enum ModelMsg {
    Installed(Vec<String>), // model names from /api/tags
    CheckFailed,
    PullProgress {
        model: String,
        completed: u64,
        total: u64,
        status: String,
    },
    PullDone(String),
    PullFailed(String, String),
}

/// Does an installed tag satisfy a wanted model? Exact match, or any
/// variant tag (e.g. "moondream:latest" satisfies "moondream").
fn tag_matches(tag: &str, wanted: &str) -> bool {
    tag == wanted || tag.starts_with(&format!("{wanted}:"))
}

fn check_models(url: &str, tx: mpsc::Sender<ModelMsg>) {
    let send = |m| {
        let _ = tx.send(m);
    };
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            send(ModelMsg::CheckFailed);
            return;
        }
    };
    let resp = client
        .get(format!("{}/api/tags", url.trim_end_matches('/')))
        .send();
    let v: serde_json::Value = match resp {
        Ok(r) if r.status().is_success() => match r.json() {
            Ok(v) => v,
            Err(_) => {
                send(ModelMsg::CheckFailed);
                return;
            }
        },
        _ => {
            send(ModelMsg::CheckFailed);
            return;
        }
    };
    let names: Vec<String> = v["models"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
        .collect();
    send(ModelMsg::Installed(names));
}

/// Pull one model, streaming progress lines from Ollama.
fn pull_model(url: &str, model: &str, tx: mpsc::Sender<ModelMsg>) {
    let send = |m| {
        let _ = tx.send(m);
    };
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3600))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            send(ModelMsg::PullFailed(model.into(), "Couldn't start HTTP client.".into()));
            return;
        }
    };
    let resp = client
        .post(format!("{}/api/pull", url.trim_end_matches('/')))
        .json(&serde_json::json!({"name": model, "stream": true}))
        .send();
    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            send(ModelMsg::PullFailed(
                model.into(),
                format!("Ollama returned HTTP {}.", r.status()),
            ));
            return;
        }
        Err(e) => {
            send(ModelMsg::PullFailed(model.into(), format!("Pull failed: {e}")));
            return;
        }
    };
    use std::io::BufRead;
    let reader = std::io::BufReader::new(resp);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(err) = v["error"].as_str() {
            send(ModelMsg::PullFailed(model.into(), err.to_string()));
            return;
        }
        let status = v["status"].as_str().unwrap_or("").to_string();
        let completed = v["completed"].as_u64().unwrap_or(0);
        let total = v["total"].as_u64().unwrap_or(0);
        let _ = tx.send(ModelMsg::PullProgress {
            model: model.into(),
            completed,
            total,
            status: status.clone(),
        });
        if status == "success" {
            send(ModelMsg::PullDone(model.into()));
            return;
        }
    }
    // Stream ended without an explicit success — treat as done if we
    // got this far without an error (Ollama sometimes closes quietly).
    send(ModelMsg::PullDone(model.into()));
}

// ---------------------------------------------------------------------------
// Ollama itself — download + launch the official installer, no website trip
// ---------------------------------------------------------------------------

const OLLAMA_SETUP_URL: &str = "https://ollama.com/download/OllamaSetup.exe";

enum OllamaDlMsg {
    Progress(u64, u64),
    Done(std::path::PathBuf),
    Failed(String),
}

fn download_ollama_installer(tx: mpsc::Sender<OllamaDlMsg>) {
    let send = |m| {
        let _ = tx.send(m);
    };
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3600))
        .user_agent("FileNamerAI-ollama-download")
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            send(OllamaDlMsg::Failed("Couldn't start HTTP client.".into()));
            return;
        }
    };
    let mut resp = match client.get(OLLAMA_SETUP_URL).send() {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            send(OllamaDlMsg::Failed(format!(
                "Download failed (HTTP {}).",
                r.status()
            )));
            return;
        }
        Err(e) => {
            send(OllamaDlMsg::Failed(format!("Download failed: {e}")));
            return;
        }
    };
    let total = resp.content_length().unwrap_or(0);
    let dest = std::env::temp_dir().join("OllamaSetup.exe");
    let mut file = match std::fs::File::create(&dest) {
        Ok(f) => f,
        Err(_) => {
            send(OllamaDlMsg::Failed("Couldn't write the installer file.".into()));
            return;
        }
    };
    let mut got: u64 = 0;
    let mut buf = [0u8; 8192];
    use std::io::Read;
    use std::io::Write;
    loop {
        match resp.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if file.write_all(&buf[..n]).is_err() {
                    send(OllamaDlMsg::Failed("Couldn't write the installer file.".into()));
                    return;
                }
                got += n as u64;
                let _ = tx.send(OllamaDlMsg::Progress(got, total));
            }
            Err(_) => {
                send(OllamaDlMsg::Failed("Download interrupted.".into()));
                return;
            }
        }
    }
    match std::fs::metadata(&dest) {
        Ok(m) if m.len() > 0 => send(OllamaDlMsg::Done(dest)),
        _ => send(OllamaDlMsg::Failed("Downloaded file is empty.".into())),
    }
}

/// Open a URL in the default browser (fallback when a download fails).
fn open_url(url: &str) {
    if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.args(["/c", "start", "", url]);
        no_window(&mut cmd);
        let _ = cmd.spawn();
    } else if cfg!(target_os = "macos") {
        let _ = Command::new("open").arg(url).spawn();
    } else {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
}

fn ollama_up(url: &str) -> bool {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build();
    match client {
        Ok(c) => c
            .get(format!("{url}/api/tags"))
            .send()
            .map(|r| r.status().is_success())
            .unwrap_or(false),
        Err(_) => false,
    }
}

fn build_prompt(original: &str, content: &str, append_date: bool, attempt: u32) -> String {
    let date_rule = if append_date {
        "- If the content contains an obvious document date (invoice date, letter date, statement date), append it after an underscore as _DD-Mon-YYYY with the month capitalized, e.g. Example Document Title_28-May-2023.\n\
         - Never invent, guess, or approximate a date. If no date appears in the content, output the name with no date at all.\n"
    } else {
        ""
    };
    let retry_rule = if attempt > 0 {
        "- IMPORTANT: you already proposed a name for this file and it was rejected. Propose a DIFFERENT name this time — different wording, not a tweak.\n"
    } else {
        ""
    };
    format!(
        "Propose a concise descriptive filename for the file described below.\n\
         \n\
         First, figure out what the document IS: its main heading or subject, who sent it, and what kind of document it is (notification, invoice, letter, confirmation, statement, etc.). The title should name that subject — sender + document type when possible (e.g. First Advantage Screening Notification).\n\
         \n\
         Rules (follow exactly):\n\
         - Output ONLY the filename: no quotes, no explanation, no file extension.\n\
         - Use spaces between words with normal Title Case (e.g. Example Document Title). Never ALL CAPS.\n\
         - The title must use ONLY words that appear in the document content below. Never invent a title or borrow words from these instructions.\n\
         - NEVER use a field label as the title: not Today's Date, From, To, Subject, Date, an address, a phone number, or any other metadata label. Those describe the document; they are not its subject.\n\
         - Ignore email headers (From/To/Date lines), page headers/footers, and boilerplate — name the document by its actual content, not its envelope.\n\
         - Maximum 50 characters.\n\
         {date_rule}\
         {retry_rule}\
         \n\
         Original filename: {original}\n\
         Content:\n\
         ---\n\
         {content}\n\
         ---"
    )
}

/// Remove characters Windows forbids in filenames; tidy up model output.
fn sanitize_filename(raw: &str) -> String {
    let mut s: String = raw
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .chars()
        .filter(|c| !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'))
        .filter(|c| !c.is_control())
        .collect();
    // collapse whitespace runs into a single space
    let mut out = String::with_capacity(s.len());
    let mut prev_sp = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_sp && !out.is_empty() {
                out.push(' ');
                prev_sp = true;
            }
        } else {
            out.push(c);
            prev_sp = false;
        }
    }
    s = out;
    s = s.trim_matches(|c| c == '.' || c == '_' || c == '-').to_string();
    s.chars().take(80).collect()
}

// ---------------------------------------------------------------------------
// AI OCR (Ollama vision model) — for scanned PDFs and forced OCR
// ---------------------------------------------------------------------------

/// Pull embedded JPEG images out of a PDF (scanned pages are usually
/// DCTDecode/JPEG streams). Returns raw JPEG bytes, one per image found.
fn extract_pdf_images(path: &Path) -> Vec<Vec<u8>> {
    let doc = match lopdf::Document::load(path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut images = Vec::new();
    for (_id, obj) in doc.objects.iter() {
        let lopdf::Object::Stream(stream) = obj else {
            continue;
        };
        // Must be an Image XObject.
        let is_image = stream
            .dict
            .get(b"Subtype")
            .and_then(|s| s.as_name())
            .map(|n| n == b"Image")
            .unwrap_or(false);
        if !is_image {
            continue;
        }
        // Only DCTDecode (JPEG) streams are usable as-is; the raw
        // content is already a valid JPEG. Other filters (Flate, etc.)
        // would need pixel decoding + re-encoding, which we skip.
        let is_jpeg = match stream.dict.get(b"Filter") {
            Ok(f) => match f.as_name() {
                Ok(n) => n == b"DCTDecode",
                Err(_) => f
                    .as_array()
                    .map(|arr| {
                        arr.iter().any(|x| {
                            x.as_name().map(|n| n == b"DCTDecode").unwrap_or(false)
                        })
                    })
                    .unwrap_or(false),
            },
            Err(_) => false,
        };
        if is_jpeg && !stream.content.is_empty() {
            images.push(stream.content.clone());
        }
    }
    images
}

/// Ask the vision model to transcribe one image. Returns the raw text.
fn ocr_image(cfg: &Config, image_bytes: &[u8]) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("OCR request failed: {e}"))?;
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, image_bytes);
    let body = serde_json::json!({
        "model": cfg.vision_model,
        "stream": false,
        "messages": [{
            "role": "user",
            "content": "Transcribe all text visible in this image exactly, preserving line breaks. Do not add any explanation, summary, or extra words — only the transcribed text.",
            "images": [b64]
        }]
    });
    let resp = client
        .post(format!("{}/api/chat", cfg.ollama_url.trim_end_matches('/')))
        .json(&body)
        .send()
        .map_err(|e| format!("OCR request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "Vision model \"{}\" returned {}. Is it pulled? (ollama pull {})",
            cfg.vision_model,
            resp.status(),
            cfg.vision_model
        ));
    }
    let v: serde_json::Value = resp
        .json()
        .map_err(|e| format!("OCR response unreadable: {e}"))?;
    let text = v["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return Err("Vision model returned no text for this image.".into());
    }
    Ok(text)
}

/// OCR a PDF: transcribe each embedded image (first page first) and
/// join the results. Returns the combined transcribed text.
fn ocr_pdf(cfg: &Config, path: &Path) -> Result<String, String> {
    let images = extract_pdf_images(path);
    if images.is_empty() {
        return Err(
            "No usable images found in this PDF (scanned pages must be JPEG).".into(),
        );
    }
    let mut parts = Vec::new();
    let mut first_err: Option<String> = None;
    for img in &images {
        match ocr_image(cfg, img) {
            Ok(t) => parts.push(t),
            Err(e) => {
                // A model-level failure (e.g. not pulled) means the
                // rest won't work either — bail immediately.
                // But an image with no readable text (a logo, a
                // blank page) just gets skipped; try the next one.
                if e.contains("returned") && e.contains("404") {
                    return Err(e);
                }
                if first_err.is_none() {
                    first_err = Some(e);
                }
                continue;
            }
        }
    }
    if parts.is_empty() {
        return Err(first_err.unwrap_or_else(|| "OCR produced no text.".into()));
    }
    let combined = parts.join("\n").trim().to_string();
    if combined.is_empty() {
        return Err("OCR produced no text.".into());
    }
    Ok(combined)
}

fn ask_ollama(cfg: &Config, path: &Path, content: &str, attempt: u32) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|e| format!("Couldn't start HTTP client: {e}"))?;

    let original = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let temp = if attempt == 0 { 0.2 } else { 0.7 };
    let body = serde_json::json!({
        "model": cfg.model,
        "messages": [
            {"role": "system", "content": "You are a file renaming assistant. You output only the proposed filename, nothing else."},
            {"role": "user", "content": build_prompt(original, content, cfg.append_date && !cfg.never_dates, attempt)}
        ],
        "stream": false,
        "options": {"temperature": temp}
    });

    let resp = client
        .post(format!("{}/api/chat", cfg.ollama_url.trim_end_matches('/')))
        .json(&body)
        .send()
        .map_err(|e| format!("Ollama request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body: String = resp.text().unwrap_or_default().chars().take(200).collect();
        return Err(format!("Ollama returned HTTP {status}: {body}"));
    }
    let v: serde_json::Value = resp.json().map_err(|e| format!("Bad response from Ollama: {e}"))?;
    let text = v["message"]["content"].as_str().unwrap_or("").trim();
    let name = sanitize_filename(text);
    if name.is_empty() {
        return Err("The model returned an empty name.".into());
    }
    // No shouting here yet — the date suffix is split off first below, since
    // its own lowercase (e.g. "Aug") would defeat the all-caps check.
    let mut name = name;
    // A trailing bare year (_2026) or month-year (_Oct-2026) isn't a usable
    // date — drop it so the best-effort fallback can supply the file's date.
    if let Some(stem) = strip_partial_date(&name) {
        name = stem.to_string();
    }
    // Split off any full trailing date, then de-shout and normalize the stem
    // only: underscores belong only right before the date.
    let (raw_stem, raw_date) = match split_date_suffix(&name) {
        Some((stem, d, m, y)) => (stem.to_string(), Some((d, m, y))),
        None => (name.clone(), None),
    };
    let stem: String = de_shout(&raw_stem)
        .replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    // Verify the title itself, not just the date: the model must not
    // invent a title (or parrot the prompt's example). If the title's
    // words aren't in the document, reject so the user can re-roll.
    if !title_in_content(&stem, content) {
        return Err(format!(
            "The model proposed a title that doesn't match the document content (\"{stem}\") — click \"New name\" to try again."
        ));
    }
    // Verify any date the model added: it must actually appear in the content.
    // If not — or if the user disabled dates entirely — strip it.
    // Otherwise normalize it to Shawn's format (28-Mar-2012).
    // Never ship an invented date.
    let mut name = match raw_date {
        Some(_) if cfg.never_dates => stem,
        Some((d, m, y)) if date_in_content(content, d, m, y) => {
            format!("{stem}_{}", fmt_doc_date(d, m, y))
        }
        Some(_) => stem,
        None => stem,
    };
    // Best effort: if the name still has no date, fall back to the file's own
    // date (Created, else Modified) — unless dates are disabled entirely.
    if cfg.append_date && !cfg.never_dates && split_date_suffix(&name).is_none() {
        if let Some((d, m, y)) = file_ymd(path) {
            name = format!("{name}_{}", fmt_doc_date(d, m, y));
        }
    }
    Ok(name)
}

// ---------------------------------------------------------------------------
// context menu registration (HKCU, no admin needed)
// ---------------------------------------------------------------------------

fn run_reg(args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("reg");
    cmd.args(args);
    no_window(&mut cmd);
    let out = cmd
        .output()
        .map_err(|e| format!("Couldn't run reg: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn menu_installed() -> bool {
    run_reg(&["query", MENU_KEY]).is_ok()
}

fn install_menu() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_s = exe.to_string_lossy();
    run_reg(&["add", MENU_KEY, "/ve", "/d", "Rename with File Namer AI", "/f"])?;
    run_reg(&["add", MENU_KEY, "/v", "Icon", "/d", &exe_s, "/f"])?;
    // "Player" = one invocation with EVERY selected file, so multi-select
    // opens a single batch window instead of one window per file.
    // (Document would be one invocation per file — the opposite.)
    run_reg(&[
        "add", MENU_KEY, "/v", "MultiSelectModel", "/d", "Player", "/f",
    ])?;
    let cmd_key = format!("{MENU_KEY}\\command");
    let cmd = format!("\"{exe_s}\" \"%1\"");
    run_reg(&["add", &cmd_key, "/ve", "/d", &cmd, "/f"])?;
    Ok(())
}

/// Existing installs predate multi-select support (v1.3.0 even set the
/// wrong model). Enforce Player on every launch — idempotent and cheap.
fn ensure_multiselect() {
    if !menu_installed() {
        return;
    }
    let _ = run_reg(&[
        "add", MENU_KEY, "/v", "MultiSelectModel", "/d", "Player", "/f",
    ]);
}

/// Current MultiSelectModel value, for the Settings diagnostic line.
fn multiselect_model() -> Option<String> {
    run_reg(&["query", MENU_KEY, "/v", "MultiSelectModel"])
        .ok()
        .and_then(|out| {
            out.lines()
                .find(|l| l.contains("MultiSelectModel"))
                .and_then(|l| l.split_whitespace().last().map(|s| s.to_string()))
        })
}

fn uninstall_menu() -> Result<(), String> {
    run_reg(&["delete", MENU_KEY, "/f"])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// auto update (Tuya-style: download installer + run it, no browser)
// ---------------------------------------------------------------------------

const UPDATE_REPO: &str = "shawnrodis/FileNamerAI";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// A release asset this app installs from.
#[derive(Clone)]
struct ReleaseInfo {
    version: String,    // e.g. "1.3.8" (tag without the 'v')
    asset_name: String, // e.g. "FileNamerAI-Setup-v1.3.8.exe"
    asset_url: String,  // browser_download_url
    asset_size: u64,
}

enum UpdateMsg {
    Available(ReleaseInfo),
    UpToDate,
    CheckFailed,
    DownloadProgress(u64, u64), // (bytes got, total)
    Downloaded(std::path::PathBuf),
    DownloadFailed,
}

fn parse_ver(s: &str) -> (u64, u64, u64) {
    let mut it = s
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|p| p.parse::<u64>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

fn check_updates(tx: mpsc::Sender<UpdateMsg>) {
    let send = |m| {
        let _ = tx.send(m);
    };
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("FileNamerAI-update-check")
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            send(UpdateMsg::CheckFailed);
            return;
        }
    };
    let v: serde_json::Value = match client
        .get(format!(
            "https://api.github.com/repos/{UPDATE_REPO}/releases/latest"
        ))
        .send()
    {
        Ok(r) if r.status().is_success() => match r.json() {
            Ok(v) => v,
            Err(_) => {
                send(UpdateMsg::CheckFailed);
                return;
            }
        },
        _ => {
            send(UpdateMsg::CheckFailed);
            return;
        }
    };
    let tag = v["tag_name"].as_str().unwrap_or("").to_string();
    if tag.is_empty() {
        send(UpdateMsg::CheckFailed);
        return;
    }
    if parse_ver(&tag) <= parse_ver(APP_VERSION) {
        send(UpdateMsg::UpToDate);
        return;
    }
    // Newer version — find this platform's installer asset by exact name.
    let version = tag.trim_start_matches('v').to_string();
    // One version, one release, per-platform assets (see MAC_BUILD.md).
    let want = if cfg!(target_os = "macos") {
        format!("FileNamerAI-v{version}.dmg")
    } else {
        format!("FileNamerAI-Setup-v{version}.exe")
    };
    let assets = v["assets"].as_array().cloned().unwrap_or_default();
    let asset = assets.iter().find(|a| {
        a.get("name")
            .and_then(|n| n.as_str())
            .map(|n| n == want)
            .unwrap_or(false)
    });
    match asset {
        Some(a) => {
            let url = a
                .get("browser_download_url")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            let size = a.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
            if url.is_empty() {
                send(UpdateMsg::CheckFailed);
            } else {
                send(UpdateMsg::Available(ReleaseInfo {
                    version,
                    asset_name: want,
                    asset_url: url,
                    asset_size: size,
                }));
            }
        }
        None => send(UpdateMsg::CheckFailed),
    }
}

/// Download the installer to %TEMP%, reporting progress. Verifies the
/// file exists and is non-empty before reporting success.
fn download_update(info: ReleaseInfo, tx: mpsc::Sender<UpdateMsg>) {
    let send = |m| {
        let _ = tx.send(m);
    };
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent("FileNamerAI-update-download")
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            send(UpdateMsg::DownloadFailed);
            return;
        }
    };
    let mut resp = match client.get(&info.asset_url).send() {
        Ok(r) if r.status().is_success() => r,
        _ => {
            send(UpdateMsg::DownloadFailed);
            return;
        }
    };
    let total = resp.content_length().unwrap_or(info.asset_size);
    let dest = std::env::temp_dir().join(&info.asset_name);
    let mut file = match std::fs::File::create(&dest) {
        Ok(f) => f,
        Err(_) => {
            send(UpdateMsg::DownloadFailed);
            return;
        }
    };
    let mut got: u64 = 0;
    let mut buf = [0u8; 8192];
    use std::io::Read;
    use std::io::Write;
    loop {
        match resp.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if file.write_all(&buf[..n]).is_err() {
                    send(UpdateMsg::DownloadFailed);
                    return;
                }
                got += n as u64;
                let _ = tx.send(UpdateMsg::DownloadProgress(got, total));
            }
            Err(_) => {
                send(UpdateMsg::DownloadFailed);
                return;
            }
        }
    }
    match std::fs::metadata(&dest) {
        Ok(m) if m.len() > 0 => send(UpdateMsg::Downloaded(dest)),
        _ => send(UpdateMsg::DownloadFailed),
    }
}

const MONTHS_FULL: [&str; 12] = [
    "january", "february", "march", "april", "may", "june", "july", "august", "september",
    "october", "november", "december",
];
const MONTHS_ABBR: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

fn valid_ymd(day: u32, mon: u32, year: i32) -> bool {
    (1..=31).contains(&day) && (1..=12).contains(&mon) && (1900..=2100).contains(&year)
}

fn normalize_year(y: i32) -> i32 {
    // 2-digit years: 00-29 -> 2000s, 30-99 -> 1900s.
    if y < 100 {
        if y < 30 {
            2000 + y
        } else {
            1900 + y
        }
    } else {
        y
    }
}

/// Parse a trailing date suffix in common formats. Returns (stem, day, month, year).
/// Handles _DD-Mon-YYYY (e.g. _13-Oct-2025), _DD-Mon-YY (e.g. _28-Aug-20),
/// _YYYY-MM-DD (e.g. _2025-10-13), and _MM-DD-YYYY (e.g. _10-13-2025).
fn split_date_suffix(s: &str) -> Option<(&str, u32, u32, i32)> {
    let i = s.rfind('_')?;
    let (stem, tail) = (&s[..i], &s[i + 1..]);
    let parts: Vec<&str> = tail.split('-').collect();
    match parts.as_slice() {
        [d, m, y] if m.chars().any(|c| c.is_alphabetic()) => {
            let day: u32 = d.parse().ok()?;
            let year: i32 = normalize_year(y.parse().ok()?);
            let mon = MONTHS_ABBR.iter().position(|x| x.eq_ignore_ascii_case(m))? as u32 + 1;
            valid_ymd(day, mon, year).then_some((stem, day, mon, year))
        }
        [y, m, d] if y.len() == 4 => {
            let (year, mon, day): (i32, u32, u32) =
                (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
            valid_ymd(day, mon, year).then_some((stem, day, mon, year))
        }
        [a, b, c] if c.len() == 4 => {
            // All-numeric _MM-DD-YYYY (US order); if first > 12 it must be DD-MM-YYYY.
            let (n1, n2, year): (u32, u32, i32) = (a.parse().ok()?, b.parse().ok()?, c.parse().ok()?);
            let (day, mon) = if n1 > 12 { (n1, n2) } else { (n2, n1) };
            valid_ymd(day, mon, year).then_some((stem, day, mon, year))
        }
        _ => None,
    }
}

/// File's own date (Created, else Modified) as (day, month, year).
fn file_ymd(path: &Path) -> Option<(u32, u32, i32)> {
    let md = std::fs::metadata(path).ok()?;
    let st = md.created().or_else(|_| md.modified()).ok()?;
    let dt: chrono::DateTime<chrono::Local> = st.into();
    let d = dt.naive_local();
    Some((d.day(), d.month(), d.year()))
}

/// Format a date the way Shawn wants it: 28-Mar-2012.
fn fmt_doc_date(day: u32, mon: u32, year: i32) -> String {
    let abbr = MONTHS_ABBR[(mon - 1) as usize];
    let mut chars = abbr.chars();
    let cap: String = chars
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default()
        + chars.as_str();
    format!("{day:02}-{cap}-{year}")
}

/// True if the content actually mentions the given date in a common format.
/// Guards against the model inventing dates that aren't in the file.
fn date_in_content(content: &str, day: u32, mon: u32, year: i32) -> bool {
    let lower = content.to_lowercase();
    let abbr = MONTHS_ABBR[(mon - 1) as usize];
    let full = MONTHS_FULL[(mon - 1) as usize];
    let yy = (year % 100) as u32;
    let cands = [
        format!("{day:02}-{abbr}-{year}"),
        format!("{day}-{abbr}-{year}"),
        format!("{day:02}-{abbr}-{yy:02}"),
        format!("{abbr} {day}, {year}"),
        format!("{abbr} {day} {year}"),
        format!("{full} {day}, {year}"),
        format!("{full} {day} {year}"),
        format!("{day}/{mon}/{year}"),
        format!("{mon}/{day}/{year}"),
        format!("{day:02}/{mon:02}/{year}"),
        format!("{mon:02}/{day:02}/{year}"),
        format!("{mon:02}/{day:02}/{yy:02}"),
        format!("{day:02}/{mon:02}/{yy:02}"),
        format!("{year}/{mon:02}/{day:02}"),
        format!("{year}/{mon}/{day}"),
        format!("{year}-{mon:02}-{day:02}"),
        format!("{year}.{mon:02}.{day:02}"),
        format!("{day:02}.{mon:02}.{year}"),
    ];
    cands.iter().any(|c| lower.contains(c.as_str()))
}

/// True when the proposed title actually comes from the document.
/// Guards against the model hallucinating a title (or parroting the
/// prompt's example) instead of reading the content: at least half
/// of the title's significant words must appear in the content.
/// Stop words and very short tokens are skipped; matching is
/// case-insensitive substring, so "Confirmation" matches
/// "confirmations" and "Opt-Out" matches "opt-out".
fn title_in_content(stem: &str, content: &str) -> bool {
    const STOP: &[&str] = &[
        "the", "a", "an", "and", "or", "of", "for", "to", "in", "on", "at", "by", "with",
        "from", "is", "are", "was", "were", "be", "been",
    ];
    let lower = content.to_lowercase();
    let words: Vec<&str> = stem
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .filter(|w| !STOP.contains(&w.to_lowercase().as_str()))
        .collect();
    if words.is_empty() {
        return true; // nothing checkable — don't block on it
    }
    let found = words
        .iter()
        .filter(|w| lower.contains(&w.to_lowercase()))
        .count();
    // At least half the significant words must be in the document.
    found * 2 >= words.len()
}

/// If the name is SHOUTING (all caps), convert to Title Case.
fn de_shout(s: &str) -> String {
    let has_lower = s.chars().any(|c| c.is_lowercase());
    let has_upper = s.chars().any(|c| c.is_uppercase());
    if !(has_upper && !has_lower) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut cap_next = true;
    for c in s.chars() {
        if c == ' ' || c == '_' || c == '-' {
            out.push(c);
            cap_next = true;
        } else if cap_next {
            out.extend(c.to_uppercase());
            cap_next = false;
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// Trailing incomplete date: _YYYY or _Mon-YYYY. Returns the stem without it.
fn strip_partial_date(s: &str) -> Option<&str> {
    let i = s.rfind('_')?;
    let (stem, tail) = (&s[..i], &s[i + 1..]);
    let parts: Vec<&str> = tail.split('-').collect();
    match parts.as_slice() {
        [y] if y.len() == 4 && y.chars().all(|c| c.is_ascii_digit()) => Some(stem),
        [m, y]
            if y.len() == 4
                && y.chars().all(|c| c.is_ascii_digit())
                && MONTHS_ABBR.iter().any(|x| x.eq_ignore_ascii_case(m)) =>
        {
            Some(stem)
        }
        _ => None,
    }
}

/// "T_1" -> "T"; "T" -> "T". Used so a BRU-style collision suffix doesn't
/// stop us recognizing an already-correctly-named file.
fn strip_numeric_suffix(stem: &str) -> &str {
    if let Some(i) = stem.rfind('_') {
        let (base, suffix) = stem.split_at(i);
        let digits = &suffix[1..];
        if !base.is_empty() && !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            return base;
        }
    }
    stem
}

/// If "stem.ext" already exists next to the file, return "stem_1",
/// "stem_2", ... (BRU-style: collisions get _1, _2). The file's own current
/// name is never treated as a collision — including when it carries a _N
/// suffix from an earlier batch (already correctly named -> keep as-is).
fn unique_stem(path: &Path, stem: &str, ext: &str) -> String {
    if let Some(cur) = path.file_stem().and_then(|s| s.to_str()) {
        if cur == stem || strip_numeric_suffix(cur) == stem {
            return cur.to_string();
        }
    }
    let name_for = |s: &str| {
        if ext.is_empty() {
            s.to_string()
        } else {
            format!("{s}.{ext}")
        }
    };
    let ok = |s: &str| {
        let cand = path.with_file_name(name_for(s));
        cand == path || !cand.exists()
    };
    if ok(stem) {
        return stem.to_string();
    }
    for n in 1..1000 {
        let s = format!("{stem}_{n}");
        if ok(&s) {
            return s;
        }
    }
    stem.to_string()
}

// ---------------------------------------------------------------------------
// app
// ---------------------------------------------------------------------------

enum WorkerMsg {
    Suggestion(String),
    DateOnly(String, String), // (proposed stem, date source label)
    NeedOllama,
    Failed(String),
}

enum RenameState {
    Working,
    NeedOllama,
    Confirm,
    Done(String),
    Error(String),
}

struct NamerApp {
    is_rename_mode: bool,
    file_path: Option<PathBuf>,
    file_ext: String,
    cfg: Config,
    // settings fields
    url_edit: String,
    model_edit: String,
    vision_model_edit: String,
    append_date: bool,
    never_dates: bool,
    force_ocr: bool,
    settings_status: String,
    drop_status: String,
    menu_on: bool,
    // rename flow
    rx: Option<mpsc::Receiver<WorkerMsg>>,
    state: RenameState,
    suggestion: String,
    date_source: Option<String>,
    attempt: u32,
    show_about: bool,
    // Ollama installer download
    ollama_dl_rx: Option<mpsc::Receiver<OllamaDlMsg>>,
    ollama_downloading: bool,
    ollama_dl_progress: (u64, u64),
    ollama_dl_status: String,
    // model management (in-app downloads)
    model_rx: Option<mpsc::Receiver<ModelMsg>>,
    models_installed: Option<Vec<String>>,
    models_checking: bool,
    models_checked: bool,
    pulling: Option<String>,
    pull_progress: (u64, u64, String), // (completed, total, status)
    // update check
    update_rx: Option<mpsc::Receiver<UpdateMsg>>,
    update_info: Option<ReleaseInfo>,
    update_dismissed: bool,
    update_status: String,
    update_pending: bool,
    update_downloading: bool,
    update_progress: (u64, u64),
}

/// Document extraction with OCR support:
/// - force_ocr on a PDF → skip text extraction, OCR directly.
/// - PDF with no readable text → automatically try AI OCR; if OCR
///   fails, return the original "no readable text" error.
fn extract_for_naming(cfg: &Config, path: &Path) -> Result<Extracted, String> {
    let is_pdf = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false);
    if is_pdf && cfg.force_ocr {
        match ocr_pdf(cfg, path) {
            Ok(text) => {
                let text: String = text.chars().take(MAX_CONTENT_CHARS).collect();
                if !text.trim().is_empty() {
                    return Ok(Extracted::Text(text));
                }
                // OCR ran but found nothing usable — fall through to
                // the text layer rather than leaving the user with an
                // error when readable text exists.
            }
            Err(_) => {
                // Same: OCR unavailable or unusable — fall back to
                // text extraction instead of erroring out.
            }
        }
    }
    match extract_content(path) {
        Ok(e) => Ok(e),
        Err(e) if is_pdf && e.contains("no readable text") => {
            match ocr_pdf(cfg, path) {
                Ok(text) => {
                    let text: String = text.chars().take(MAX_CONTENT_CHARS).collect();
                    if text.trim().is_empty() {
                        Err(e)
                    } else {
                        Ok(Extracted::Text(text))
                    }
                }
                Err(_) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

fn rename_worker(path: PathBuf, cfg: Config, tx: mpsc::Sender<WorkerMsg>, attempt: u32) {
    let send = |m| {
        let _ = tx.send(m);
    };
    let extracted = match extract_for_naming(&cfg, &path) {
        Ok(e) => e,
        Err(e) => {
            send(WorkerMsg::Failed(e));
            return;
        }
    };
    match extracted {
        Extracted::DateTime(dt, src) => {
            // BRU-style: MM-DD-YY-HH-MM-SS, straight to the confirm dialog. No AI.
            send(WorkerMsg::DateOnly(
                dt.format("%m-%d-%y-%H-%M-%S").to_string(),
                src.to_string(),
            ));
        }
        Extracted::Text(content) => {
            if !ollama_up(&cfg.ollama_url) {
                send(WorkerMsg::NeedOllama);
                return;
            }
            match ask_ollama(&cfg, &path, &content, attempt) {
                Ok(name) => send(WorkerMsg::Suggestion(name)),
                Err(e) => send(WorkerMsg::Failed(e)),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// batch mode (multi-select): one window, BRU-style _1/_2 collision assessment
// ---------------------------------------------------------------------------

struct BatchProposal {
    stem: String,
    date_source: Option<String>, // Some for photos/videos, None for documents
}

enum BatchMsg {
    Proposal(usize, Result<BatchProposal, String>),
}

fn batch_worker(idx: usize, path: PathBuf, cfg: Config, tx: mpsc::Sender<BatchMsg>) {
    let result: Result<BatchProposal, String> = (|| {
        match extract_for_naming(&cfg, &path)? {
            Extracted::DateTime(dt, src) => Ok(BatchProposal {
                stem: sanitize_filename(&dt.format("%m-%d-%y-%H-%M-%S").to_string()),
                date_source: Some(src.to_string()),
            }),
            Extracted::Text(content) => {
                if !ollama_up(&cfg.ollama_url) {
                    return Err("Ollama isn't running. Start it, then reopen this window.".into());
                }
                let name = ask_ollama(&cfg, &path, &content, 0)?;
                Ok(BatchProposal {
                    stem: sanitize_filename(&name),
                    date_source: None,
                })
            }
        }
    })();
    let _ = tx.send(BatchMsg::Proposal(idx, result));
}

struct BatchRow {
    path: PathBuf,
    current_name: String,
    ext: String,
    edit_buf: String, // proposed stem, editable
    manual: bool,     // user edited it; worker must not overwrite
    ready: bool,      // worker delivered (or manual text entered)
    date_source: Option<String>,
    error: Option<String>,
    renamed_to: Option<String>, // set after Rename All
}

#[derive(Clone)]
enum RowStatus {
    Working,
    Error,
    NoOp,
    Ready { final_name: String, suffixed: bool },
}

/// BRU-style batch collision resolution: every file that would share a name
/// (with another selected file or an existing disk file) gets _1, _2, ...
/// Files already correctly named are left alone and hold their names.
fn resolve_batch(rows: &[BatchRow]) -> Vec<RowStatus> {
    use std::collections::{HashMap, HashSet};

    let mut out: Vec<RowStatus> = vec![RowStatus::Working; rows.len()];

    // Effective (sanitized) stems; None = not ready or error/empty.
    let eff: Vec<Option<String>> = rows
        .iter()
        .map(|r| {
            if r.error.is_some() || !r.ready {
                None
            } else {
                let s = sanitize_filename(&r.edit_buf);
                if s.is_empty() { None } else { Some(s) }
            }
        })
        .collect();
    for (i, r) in rows.iter().enumerate() {
        if r.error.is_some() {
            out[i] = RowStatus::Error;
        }
    }

    // Work per parent directory (multi-select can span folders).
    let mut by_dir: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        if eff[i].is_none() {
            continue;
        }
        let dir = r
            .path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        by_dir.entry(dir).or_default().push(i);
    }

    for (dir, indices) in by_dir {
        // Names taken on disk; files we're renaming vacate theirs.
        let mut taken: HashSet<String> = HashSet::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if let Some(n) = e.file_name().to_str() {
                    taken.insert(n.to_lowercase());
                }
            }
        }
        // No-op detection first (case-insensitive; Windows is too).
        // A trailing _N collision suffix is ignored: T_1.mp4 with a computed
        // stem of T is already correctly named.
        let mut is_noop = HashMap::new();
        for &i in &indices {
            let r = &rows[i];
            let s = eff[i].as_ref().unwrap();
            let cur_stem = r
                .path
                .file_stem()
                .and_then(|x| x.to_str())
                .unwrap_or(&r.current_name);
            let s_low = s.to_lowercase();
            let cur_low = cur_stem.to_lowercase();
            let noop = s_low == cur_low || s_low == strip_numeric_suffix(&cur_low);
            is_noop.insert(i, noop);
            if noop {
                out[i] = RowStatus::NoOp;
            } else {
                taken.remove(&r.current_name.to_lowercase());
            }
        }
        // Group the rest by (stem, ext).
        let mut groups: HashMap<(String, String), Vec<usize>> = HashMap::new();
        for &i in &indices {
            if is_noop[&i] {
                continue;
            }
            let r = &rows[i];
            groups
                .entry((eff[i].as_ref().unwrap().clone(), r.ext.clone()))
                .or_default()
                .push(i);
        }
        let mut keys: Vec<(String, String)> = groups.keys().cloned().collect();
        keys.sort();
        for (stem, ext) in keys {
            let mut members = groups[&(stem.clone(), ext.clone())].clone();
            members.sort_by(|&a, &b| rows[a].current_name.cmp(&rows[b].current_name));
            let full = |s: &str| {
                if ext.is_empty() {
                    s.to_string()
                } else {
                    format!("{s}.{ext}")
                }
            };
            if members.len() == 1 && !taken.contains(&full(&stem).to_lowercase()) {
                taken.insert(full(&stem).to_lowercase());
                out[members[0]] = RowStatus::Ready {
                    final_name: full(&stem),
                    suffixed: false,
                };
            } else {
                let mut n = 1u32;
                for &idx in &members {
                    loop {
                        let cand = format!("{stem}_{n}");
                        if n > 9999 {
                            break;
                        }
                        if !taken.contains(&full(&cand).to_lowercase()) {
                            taken.insert(full(&cand).to_lowercase());
                            out[idx] = RowStatus::Ready {
                                final_name: full(&cand),
                                suffixed: true,
                            };
                            n += 1;
                            break;
                        }
                        n += 1;
                    }
                }
            }
        }
    }
    out
}

impl NamerApp {
    fn new(cc: &eframe::CreationContext<'_>, files: Vec<PathBuf>) -> Self {
        cc.egui_ctx.set_visuals(if is_dark_mode() {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });

        let cfg = load_config();
        let file_path = files.into_iter().next();

        let mut app = Self {
            is_rename_mode: file_path.is_some(),
            file_ext: file_path
                .as_ref()
                .and_then(|p| p.extension())
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string(),
            file_path,
            url_edit: cfg.ollama_url.clone(),
            model_edit: cfg.model.clone(),
            vision_model_edit: cfg.vision_model.clone(),
            force_ocr: cfg.force_ocr,
            append_date: cfg.append_date,
            never_dates: cfg.never_dates,
            cfg,
            settings_status: String::new(),
            drop_status: String::new(),
            menu_on: menu_installed(),
            rx: None,
            state: RenameState::Working,
            suggestion: String::new(),
            date_source: None,
            attempt: 0,
            show_about: false,
            ollama_dl_rx: None,
            ollama_downloading: false,
            ollama_dl_progress: (0, 0),
            ollama_dl_status: String::new(),
            model_rx: None,
            models_installed: None,
            models_checking: false,
            models_checked: false,
            pulling: None,
            pull_progress: (0, 0, String::new()),
            update_rx: None,
            update_info: None,
            update_dismissed: false,
            update_status: String::new(),
            update_pending: false,
            update_downloading: false,
            update_progress: (0, 0),
        };

        if !app.is_rename_mode {
            // Check for updates in the background (silent unless one is found).
            let (utx, urx) = mpsc::channel();
            app.update_rx = Some(urx);
            app.update_pending = true;
            thread::spawn(move || check_updates(utx));
        }

        if app.is_rename_mode {
            app.start_worker();
        }
        app
    }

    fn poll_worker(&mut self, ctx: &egui::Context) {
        let mut got: Option<WorkerMsg> = None;
        if let Some(rx) = &self.rx {
            while let Ok(m) = rx.try_recv() {
                got = Some(m);
            }
        }
        match got {
            Some(WorkerMsg::Suggestion(name)) => {
                // Resolve collisions up front so the dialog shows the final name.
                let stem = sanitize_filename(&name);
                self.suggestion = match &self.file_path {
                    Some(p) => unique_stem(p, &stem, &self.file_ext.clone()),
                    None => stem,
                };
                self.date_source = None;
                self.state = RenameState::Confirm;
            }
            Some(WorkerMsg::DateOnly(name, src)) => {
                let stem = sanitize_filename(&name);
                self.suggestion = match &self.file_path {
                    Some(p) => unique_stem(p, &stem, &self.file_ext.clone()),
                    None => stem,
                };
                self.date_source = Some(src);
                self.state = RenameState::Confirm;
            }
            Some(WorkerMsg::NeedOllama) => self.state = RenameState::NeedOllama,
            Some(WorkerMsg::Failed(e)) => self.state = RenameState::Error(e),
            None => {
                if matches!(self.state, RenameState::Working) {
                    ctx.request_repaint();
                }
            }
        }
    }

    /// (Re)start the background suggestion worker.
    fn start_worker(&mut self) {
        let path = match self.file_path.clone() {
            Some(p) => p,
            None => return,
        };
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.state = RenameState::Working;
        let cfg = self.cfg.clone();
        let attempt = self.attempt;
        thread::spawn(move || rename_worker(path, cfg, tx, attempt));
    }

    fn do_rename(&mut self) {
        let path = match &self.file_path {
            Some(p) => p.clone(),
            None => return,
        };
        let stem = sanitize_filename(&self.suggestion);
        if stem.is_empty() {
            self.state = RenameState::Error("The name is empty.".into());
            return;
        }
        let stem = unique_stem(&path, &stem, &self.file_ext);
        let new_name = if self.file_ext.is_empty() {
            stem.clone()
        } else {
            format!("{stem}.{}", self.file_ext)
        };
        let new_path = path.with_file_name(&new_name);
        if new_path == path {
            // Name unchanged: nothing to do.
            self.state = RenameState::Done(new_name);
            return;
        }
        match std::fs::rename(&path, &new_path) {
            Ok(()) => {
                self.file_path = Some(new_path);
                self.state = RenameState::Done(new_name);
            }
            Err(e) => self.state = RenameState::Error(format!("Rename failed: {e}")),
        }
    }

    fn save_settings_from_ui(&mut self) {
        self.cfg.ollama_url = self.url_edit.trim().to_string();
        self.cfg.model = self.model_edit.trim().to_string();
        self.cfg.vision_model = self.vision_model_edit.trim().to_string();
        self.cfg.append_date = self.append_date;
        self.cfg.never_dates = self.never_dates;
        self.cfg.force_ocr = self.force_ocr;
        save_config(&self.cfg);
    }

    fn poll_ollama_dl(&mut self, ctx: &egui::Context) {
        if self.ollama_downloading {
            ctx.request_repaint();
        }
        let mut got: Vec<OllamaDlMsg> = Vec::new();
        if let Some(rx) = &self.ollama_dl_rx {
            while let Ok(m) = rx.try_recv() {
                got.push(m);
            }
        }
        for m in got {
            match m {
                OllamaDlMsg::Progress(got_b, total) => {
                    self.ollama_dl_progress = (got_b, total);
                }
                OllamaDlMsg::Done(path) => {
                    self.ollama_downloading = false;
                    // Launch the official installer WITH its UI so the
                    // user clicks through it (no silent install).
                    match Command::new(&path).spawn() {
                        Ok(_) => {
                            self.ollama_dl_status = "Ollama installer launched — complete its steps, then come back and Refresh.".into();
                        }
                        Err(e) => {
                            self.ollama_dl_status =
                                format!("Couldn't launch the installer: {e}");
                        }
                    }
                }
                OllamaDlMsg::Failed(err) => {
                    self.ollama_downloading = false;
                    self.ollama_dl_status = err;
                }
            }
        }
    }

    fn poll_models(&mut self, ctx: &egui::Context) {
        // Auto-check once when Settings opens (settings mode only).
        if !self.is_rename_mode && !self.models_checked && !self.models_checking {
            self.models_checking = true;
            let (tx, rx) = mpsc::channel();
            self.model_rx = Some(rx);
            let url = self.cfg.ollama_url.clone();
            thread::spawn(move || check_models(&url, tx));
        }
        if self.pulling.is_some() {
            ctx.request_repaint();
        }
        let mut got: Vec<ModelMsg> = Vec::new();
        if let Some(rx) = &self.model_rx {
            while let Ok(m) = rx.try_recv() {
                got.push(m);
            }
        }
        for m in got {
            match m {
                ModelMsg::Installed(names) => {
                    self.models_installed = Some(names);
                    self.models_checking = false;
                    self.models_checked = true;
                }
                ModelMsg::CheckFailed => {
                    self.models_checking = false;
                    self.models_checked = true;
                    self.models_installed = None;
                }
                ModelMsg::PullProgress {
                    model,
                    completed,
                    total,
                    status,
                } => {
                    if self.pulling.as_deref() == Some(model.as_str()) {
                        self.pull_progress = (completed, total, status);
                    }
                }
                ModelMsg::PullDone(model) => {
                    self.pulling = None;
                    self.pull_progress = (0, 0, String::new());
                    // Refresh the installed list so the button flips to Installed.
                    self.models_checking = true;
                    self.models_checked = false;
                    let (tx, rx) = mpsc::channel();
                    self.model_rx = Some(rx);
                    let url = self.cfg.ollama_url.clone();
                    thread::spawn(move || check_models(&url, tx));
                    self.settings_status = format!("{model} downloaded.");
                }
                ModelMsg::PullFailed(model, err) => {
                    self.pulling = None;
                    self.pull_progress = (0, 0, String::new());
                    self.settings_status = format!("Couldn't download {model}: {err}");
                }
            }
        }
    }

    fn poll_updates(&mut self, ctx: &egui::Context) {
        if self.update_pending {
            ctx.request_repaint();
        }
        let mut got: Option<UpdateMsg> = None;
        if let Some(rx) = &self.update_rx {
            while let Ok(m) = rx.try_recv() {
                got = Some(m);
            }
        }
        match got {
            Some(UpdateMsg::Available(info)) => {
                self.update_info = Some(info);
                self.update_dismissed = false;
                self.update_status.clear();
                self.update_pending = false;
                self.update_downloading = false;
            }
            Some(UpdateMsg::UpToDate) => {
                self.update_info = None;
                self.update_status = format!("You're up to date (v{APP_VERSION}).");
                self.update_pending = false;
            }
            Some(UpdateMsg::CheckFailed) => {
                self.update_pending = false;
                // Only report manual checks; the launch check stays silent.
                if self.update_status == "Checking…" {
                    self.update_status =
                        "Couldn't reach GitHub — check your connection and try again.".into();
                }
            }
            Some(UpdateMsg::DownloadProgress(got_bytes, total)) => {
                self.update_progress = (got_bytes, total);
                ctx.request_repaint();
            }
            Some(UpdateMsg::Downloaded(path)) => {
                self.update_downloading = false;
                self.update_status = format!(
                    "Downloaded {} — launching installer…",
                    path.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                );
                if cfg!(target_os = "macos") {
                    // macOS: open the disk image; the user drags the new
                    // version to Applications. No exit needed.
                    open_url(&path.to_string_lossy());
                    self.update_status = "Downloaded — open the disk image and drag the new version to Applications.".into();
                } else {
                    // Windows: run the installer, then exit so it can replace us.
                    let mut cmd = Command::new(&path);
                    no_window(&mut cmd);
                    let _ = cmd.spawn();
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    std::process::exit(0);
                }
            }
            Some(UpdateMsg::DownloadFailed) => {
                self.update_downloading = false;
                self.update_status = "Download failed — check your connection and try again.".into();
            }
            None => {}
        }
    }

    fn ui_settings(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        self.poll_updates(ctx);
        self.poll_models(ctx);
        self.poll_ollama_dl(ctx);

        // Drag-and-drop: files dropped onto the Settings window.
        // egui reports each drop once, on the frame it lands.
        let dropped_raw = ctx.input(|i| i.raw.dropped_files.clone());
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if !dropped_raw.is_empty() {
            // Directories and pathless drops are ignored, never a crash.
            let files: Vec<PathBuf> = dropped_raw
                .iter()
                .filter_map(|f| f.path.clone())
                .filter(|p| p.is_file())
                .collect();
            if files.is_empty() {
                self.drop_status =
                    "That drop had no files in it — drop a file to rename it.".into();
            } else if files.len() == 1 {
                // Single file: rename it in this same window, no relaunch.
                // Save the visible settings first so the worker uses them.
                self.save_settings_from_ui();
                let file = files.into_iter().next().unwrap();
                self.file_ext = file
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                self.file_path = Some(file);
                self.is_rename_mode = true;
                self.suggestion.clear();
                self.date_source = None;
                self.attempt = 0;
                self.start_worker();
                return;
            } else {
                // Multiple files: hand them to a fresh process, whose
                // coordinate_files()/BatchApp path opens one batch window.
                let count = files.len();
                let spawned = std::env::current_exe()
                    .map_err(|e| e.to_string())
                    .and_then(|exe| {
                        std::process::Command::new(exe)
                            .args(&files)
                            .spawn()
                            .map_err(|e| e.to_string())
                    });
                match spawned {
                    Ok(_) => {
                        self.drop_status =
                            format!("{count} files dropped — opening the batch window…");
                    }
                    Err(e) => {
                        self.drop_status =
                            format!("Couldn't open the batch window: {e}");
                    }
                }
            }
        }

        ui.heading("File Namer AI");
        if cfg!(windows) {
            ui.label("Right-click any file, choose \"Rename with File Namer AI\", and a local AI proposes a better name.");
        } else {
            ui.label("Drag files onto this window and a local AI proposes a better name.");
        }

        // Drop zone: a visible affordance, highlighted while files hover.
        let mut drop_frame = egui::Frame::group(ui.style());
        if hovering {
            drop_frame.fill = egui::Color32::from_rgb(38, 70, 120);
        }
        drop_frame.show(ui, |ui| {
            ui.label(egui::RichText::new("Drop files here to rename them").strong());
            if hovering {
                ui.label(egui::RichText::new("Release to drop").strong());
            } else {
                ui.label("One file opens the rename dialog right here. Several files open the batch window.");
            }
            if !self.drop_status.is_empty() {
                ui.label(&self.drop_status);
            }
        });
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            if ui.button("Check for updates").clicked() {
                let (tx2, rx2) = mpsc::channel();
                self.update_rx = Some(rx2);
                self.update_dismissed = false;
                self.update_info = None;
                self.update_status = "Checking…".into();
                self.update_pending = true;
                thread::spawn(move || check_updates(tx2));
            }
            if !self.update_status.is_empty() {
                ui.label(&self.update_status);
            }
        });

        // update banner
        let upd = self.update_info.clone();
        if let Some(info) = upd {
            if !self.update_dismissed {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "Update available: v{} (you have v{APP_VERSION})",
                            info.version
                        ))
                        .strong(),
                    );
                    if self.update_downloading {
                        let (got_b, total_b) = self.update_progress;
                        let pct = if total_b > 0 {
                            got_b as f32 / total_b as f32 * 100.0
                        } else {
                            0.0
                        };
                        ui.label(format!(
                            "Downloading… {:.0}% ({:.1} / {:.1} MB)",
                            pct,
                            got_b as f32 / 1_048_576.0,
                            total_b as f32 / 1_048_576.0
                        ));
                    } else if ui.button("Download & Install").clicked() {
                        // Tuya-style: download the installer, run
                        // it, exit so it can replace us.
                        self.update_downloading = true;
                        self.update_progress = (0, info.asset_size);
                        let (tx2, rx2) = mpsc::channel();
                        self.update_rx = Some(rx2);
                        let info2 = info.clone();
                        thread::spawn(move || download_update(info2, tx2));
                    }
                    if ui.button("Dismiss").clicked() {
                        self.update_dismissed = true;
                    }
                });
            }
        }

        ui.add_space(8.0);

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(egui::RichText::new("Local AI (Ollama)").strong());
            ui.horizontal(|ui| {
                ui.label("Ollama URL:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.url_edit).desired_width(280.0),
                );
            });
            ui.checkbox(&mut self.append_date, "Append date (_DD-Mon-YYYY) when the content has one");
            ui.checkbox(&mut self.never_dates, "Never add dates to document names");
            ui.checkbox(&mut self.force_ocr, "Force AI OCR for PDFs (ignore the text layer, read the images instead)");
            if ui.button("Save settings").clicked() {
                self.save_settings_from_ui();
                self.settings_status = "Settings saved.".into();
            }
            if !self.settings_status.is_empty() {
                ui.label(&self.settings_status);
            }
            ui.add_space(4.0);
            ui.label("First-time setup: install Ollama from ollama.com, start it, then download the models in the Models section below — no terminal needed.");
        });

        ui.add_space(8.0);
        {
            // Models: view, rename, and download right here — no terminal.
            let installed = self.models_installed.clone();
            let checking = self.models_checking;
            let pulling = self.pulling.clone();
            let (pc, pt, pstatus) = self.pull_progress.clone();
            let mut to_pull: Option<String> = None;
            let mut do_refresh = false;
            let mut to_dl_ollama = false;
            let ollama_downloading = self.ollama_downloading;
            let ollama_dl_progress = self.ollama_dl_progress;
            let ollama_dl_status = self.ollama_dl_status.clone();
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(egui::RichText::new("Models").strong());
                ui.label("Edit a name to switch models, then Save settings. Download right here — no terminal needed.");
                // Row helper inlined twice (naming + vision) since each
                // edits a different field.
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.model_edit)
                            .desired_width(160.0),
                    );
                    ui.label("(naming documents)");
                    let wanted = self.model_edit.trim().to_string();
                    if wanted.is_empty() {
                        ui.label("Enter a model name.");
                    } else {
                        let is_installed = installed
                            .as_ref()
                            .map(|list| list.iter().any(|t| tag_matches(t, &wanted)))
                            .unwrap_or(false);
                        let is_pulling = pulling.as_deref() == Some(wanted.as_str());
                        if is_pulling {
                            if pt > 0 {
                                ui.label(format!(
                                    "Downloading… {:.0}% ({:.1} / {:.1} MB) {}",
                                    pc as f32 / pt as f32 * 100.0,
                                    pc as f32 / 1_048_576.0,
                                    pt as f32 / 1_048_576.0,
                                    pstatus
                                ));
                            } else {
                                ui.label(format!("Downloading… {pstatus}"));
                            }
                        } else if checking {
                            ui.label("Checking…");
                        } else if installed.is_none() {
                            ui.label("Can't reach Ollama.");
                        } else if is_installed {
                            ui.label(
                                egui::RichText::new("Installed")
                                    .color(egui::Color32::GREEN),
                            );
                        } else if ui.button("Download").clicked() {
                            to_pull = Some(wanted);
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.vision_model_edit)
                            .desired_width(160.0),
                    );
                    ui.label("(PDF OCR)");
                    let wanted = self.vision_model_edit.trim().to_string();
                    if wanted.is_empty() {
                        ui.label("Enter a model name.");
                    } else {
                        let is_installed = installed
                            .as_ref()
                            .map(|list| list.iter().any(|t| tag_matches(t, &wanted)))
                            .unwrap_or(false);
                        let is_pulling = pulling.as_deref() == Some(wanted.as_str());
                        if is_pulling {
                            if pt > 0 {
                                ui.label(format!(
                                    "Downloading… {:.0}% ({:.1} / {:.1} MB) {}",
                                    pc as f32 / pt as f32 * 100.0,
                                    pc as f32 / 1_048_576.0,
                                    pt as f32 / 1_048_576.0,
                                    pstatus
                                ));
                            } else {
                                ui.label(format!("Downloading… {pstatus}"));
                            }
                        } else if checking {
                            ui.label("Checking…");
                        } else if installed.is_none() {
                            ui.label("Can't reach Ollama.");
                        } else if is_installed {
                            ui.label(
                                egui::RichText::new("Installed")
                                    .color(egui::Color32::GREEN),
                            );
                        } else if ui.button("Download").clicked() {
                            to_pull = Some(wanted);
                        }
                    }
                });
                if ui.button("Refresh").clicked() {
                    do_refresh = true;
                }
                // Ollama itself missing: Windows downloads + launches the
                // official installer; macOS links to the website (Ollama
                // for Mac ships as a .zip app, not an installer).
                if installed.is_none() && !checking {
                    ui.add_space(4.0);
                    ui.label("Ollama isn't installed or isn't running.");
                    if cfg!(windows) {
                        if ollama_downloading {
                            let (got_b, total_b) = ollama_dl_progress;
                            if total_b > 0 {
                                ui.label(format!(
                                    "Downloading Ollama… {:.0}% ({:.1} / {:.1} MB)",
                                    got_b as f32 / total_b as f32 * 100.0,
                                    got_b as f32 / 1_048_576.0,
                                    total_b as f32 / 1_048_576.0
                                ));
                            } else {
                                ui.label("Downloading Ollama…");
                            }
                        } else if ui.button("Download & Install Ollama").clicked() {
                            to_dl_ollama = true;
                        }
                    }
                    if ui.button("Open ollama.com").clicked() {
                        open_url("https://ollama.com");
                    }
                    if !ollama_dl_status.is_empty() {
                        ui.label(ollama_dl_status.as_str());
                    }
                }
            });
            if let Some(m) = to_pull {
                self.pulling = Some(m.clone());
                self.pull_progress = (0, 0, String::new());
                let (tx, rx) = mpsc::channel();
                self.model_rx = Some(rx);
                let url = self.cfg.ollama_url.clone();
                thread::spawn(move || pull_model(&url, &m, tx));
            }
            if do_refresh {
                self.models_checking = true;
                self.models_checked = false;
                let (tx, rx) = mpsc::channel();
                self.model_rx = Some(rx);
                let url = self.cfg.ollama_url.clone();
                thread::spawn(move || check_models(&url, tx));
            }
            if to_dl_ollama {
                self.ollama_downloading = true;
                self.ollama_dl_progress = (0, 0);
                self.ollama_dl_status.clear();
                let (tx, rx) = mpsc::channel();
                self.ollama_dl_rx = Some(rx);
                thread::spawn(move || download_ollama_installer(tx));
            }
        }

        ui.add_space(8.0);
        if cfg!(windows) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(egui::RichText::new("Right-click menu").strong());
            ui.horizontal(|ui| {
                if self.menu_on {
                    if ui.button("Remove from right-click menu").clicked() {
                        match uninstall_menu() {
                            Ok(()) => {
                                self.menu_on = false;
                                self.settings_status = "Removed from the right-click menu.".into();
                            }
                            Err(e) => self.settings_status = format!("Couldn't remove: {e}"),
                        }
                    }
                } else if ui.button("Add to right-click menu").clicked() {
                    match install_menu() {
                        Ok(()) => {
                            self.menu_on = true;
                            self.settings_status =
                                "Added! Right-click any file to rename it.".into();
                        }
                        Err(e) => self.settings_status = format!("Couldn't add: {e}"),
                    }
                }
            });
            ui.label("Note: on Windows 11 the item lives under \"Show more options\".");
            ui.label("You can also drag files onto this window to rename them.");
            if self.menu_on {
                match multiselect_model() {
                    Some(m) => {
                        ui.label(format!("Multi-select mode: {m} (want \"Player\" for one batch window)."));
                    }
                    None => {
                        ui.label("Multi-select mode: not set.");
                    }
                }
            }
        });
        } else {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(egui::RichText::new("Right-click menu").strong());
                ui.label("Right-click menu integration is Windows-only. On Mac, drag files onto this window to rename them — one file opens the rename dialog here, several open the batch window.");
            });
        }

        ui.add_space(8.0);
        ui.collapsing("How photo and video dating works", |ui| {
            ui.label("Photos and videos are named from dates — no AI involved.");
            ui.label("• Photos with a real Date Taken in EXIF keep full seconds (MM-DD-YY-HH-MM-SS).");
            ui.label("• Photos without real Date Taken (screenshots, chat downloads) are truncated to the minute (:00 seconds).");
            ui.label("• Videos use the embedded media-created date, always truncated to the minute (:00).");
            ui.label("• If the dates disagree, the earliest one wins (EXIF/media date, file Created, file Modified).");
            ui.label("• Two files sharing a timestamp get _1, _2 suffixes; already-correct names are left alone.");
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("About").clicked() {
                    self.show_about = true;
                }
            });
        });
    }

    fn ui_rename(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.poll_worker(ctx);
        self.poll_ollama_dl(ctx);
        let fname = self
            .file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        ui.heading("File Namer AI");
        ui.label(format!("File: {fname}"));
        ui.add_space(8.0);

        match &self.state {
            RenameState::Working => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Reading the file and asking the local AI…");
                });
            }
            RenameState::NeedOllama => {
                ui.label(egui::RichText::new("Can't reach Ollama.").strong());
                ui.label("Install it from ollama.com, then run:");
                ui.monospace("ollama pull qwen2.5:3b");
                ui.monospace("ollama pull moondream  (for PDF OCR)");
                ui.monospace("ollama serve");
                ui.add_space(8.0);
                if cfg!(windows) {
                    if self.ollama_downloading {
                        let (got_b, total_b) = self.ollama_dl_progress;
                        if total_b > 0 {
                            ui.label(format!(
                                "Downloading Ollama… {:.0}% ({:.1} / {:.1} MB)",
                                got_b as f32 / total_b as f32 * 100.0,
                                got_b as f32 / 1_048_576.0,
                                total_b as f32 / 1_048_576.0
                            ));
                        } else {
                            ui.label("Downloading Ollama…");
                        }
                    } else if ui.button("Download & Install Ollama").clicked() {
                        self.ollama_downloading = true;
                        self.ollama_dl_progress = (0, 0);
                        self.ollama_dl_status.clear();
                        let (tx, rx) = mpsc::channel();
                        self.ollama_dl_rx = Some(rx);
                        thread::spawn(move || download_ollama_installer(tx));
                    }
                }
                if ui.button("Open ollama.com").clicked() {
                    open_url("https://ollama.com");
                }
                if !self.ollama_dl_status.is_empty() {
                    ui.label(self.ollama_dl_status.as_str());
                }
                ui.add_space(8.0);
                if ui.button("Close").clicked() {
                    std::process::exit(0);
                }
            }
            RenameState::Confirm => {
                // Already-named detection (same logic as batch): the suggestion
                // resolves to the file's current name (modulo a _N suffix).
                let new_name = if self.file_ext.is_empty() {
                    self.suggestion.clone()
                } else {
                    format!("{}.{}", self.suggestion, self.file_ext)
                };
                let already = self
                    .file_path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .and_then(|s| s.to_str())
                    .map(|cur| cur.to_lowercase() == new_name.to_lowercase())
                    .unwrap_or(false);
                if already {
                    ui.label(
                        egui::RichText::new(format!("✓ Already correctly named as {new_name}"))
                            .color(egui::Color32::GREEN)
                            .strong(),
                    );
                    if let Some(src) = &self.date_source {
                        ui.label(
                            egui::RichText::new(format!("Date source: {src}"))
                                .weak()
                                .small(),
                        );
                    }
                    ui.add_space(8.0);
                    if ui.button("Close").clicked() {
                        std::process::exit(0);
                    }
                    return;
                }
                ui.label("Proposed name (edit if you like):");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.suggestion).desired_width(340.0),
                    );
                    if !self.file_ext.is_empty() {
                        ui.label(format!(".{}", self.file_ext));
                    }
                });
                if let Some(src) = &self.date_source {
                    ui.label(
                        egui::RichText::new(format!("Date source: {src}"))
                            .weak()
                            .small(),
                    );
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Rename").clicked() {
                        self.do_rename();
                    }
                    if ui.button("New name").clicked() {
                        self.attempt += 1;
                        self.start_worker();
                    }
                    if ui.button("Cancel").clicked() {
                        std::process::exit(0);
                    }
                });
            }
            RenameState::Done(new_name) => {
                ui.label(egui::RichText::new(format!("Renamed to {new_name}")).strong());
                ui.add_space(8.0);
                if ui.button("Close").clicked() {
                    std::process::exit(0);
                }
            }
            RenameState::Error(e) => {
                let e = e.clone();
                ui.label(egui::RichText::new("Something went wrong:").strong());
                ui.label(e);
                ui.add_space(8.0);
                if ui.button("Close").clicked() {
                    std::process::exit(0);
                }
            }
        }
    }
}

impl eframe::App for NamerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if self.is_rename_mode {
                    self.ui_rename(ui, ctx);
                } else {
                    self.ui_settings(ctx, ui);
                }
                ui.add_space(8.0);
            });
        });

        if self.show_about {
            egui::Window::new("About File Namer AI")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(
                        format!(
                            "File Namer AI v{APP_VERSION}\n\n\
                             Right-click any file and a local AI (Ollama)\n\
                             proposes a better name based on its contents,\n\
                             with a date when the content has one.\n\n\
                             100% local — no cloud, no API keys.\n\n\
                             Built for Shawn Rodis."
                        ),
                    );
                    ui.add_space(8.0);
                    if ui.button("Close").clicked() {
                        self.show_about = false;
                    }
                });
        }
    }
}

struct BatchApp {
    rows: Vec<BatchRow>,
    rx: mpsc::Receiver<BatchMsg>,
    statuses: Vec<RowStatus>,
    finished: bool,
}

impl BatchApp {
    fn new(cc: &eframe::CreationContext<'_>, files: Vec<PathBuf>) -> Self {
        cc.egui_ctx.set_visuals(if is_dark_mode() {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
        let cfg = load_config();
        let mut rows: Vec<BatchRow> = files
            .into_iter()
            .map(|path| {
                let current_name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let ext = path
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                BatchRow {
                    path,
                    current_name,
                    ext,
                    edit_buf: String::new(),
                    manual: false,
                    ready: false,
                    date_source: None,
                    error: None,
                    renamed_to: None,
                }
            })
            .collect();
        rows.sort_by(|a, b| a.current_name.cmp(&b.current_name));
        let (tx, rx) = mpsc::channel();
        for (i, row) in rows.iter().enumerate() {
            let tx = tx.clone();
            let path = row.path.clone();
            let cfg = cfg.clone();
            thread::spawn(move || batch_worker(i, path, cfg, tx));
        }
        let statuses = vec![RowStatus::Working; rows.len()];
        Self {
            rows,
            rx,
            statuses,
            finished: false,
        }
    }

    fn re_resolve(&mut self) {
        self.statuses = resolve_batch(&self.rows);
    }

    fn poll_workers(&mut self, ctx: &egui::Context) {
        let mut got = false;
        while let Ok(BatchMsg::Proposal(idx, result)) = self.rx.try_recv() {
            got = true;
            if let Some(row) = self.rows.get_mut(idx) {
                match result {
                    Ok(p) => {
                        if !row.manual {
                            row.edit_buf = p.stem;
                        }
                        row.date_source = p.date_source;
                        row.ready = true;
                    }
                    Err(e) => {
                        row.error = Some(e);
                    }
                }
            }
        }
        if got {
            self.re_resolve();
        } else if self
            .statuses
            .iter()
            .any(|s| matches!(s, RowStatus::Working))
        {
            ctx.request_repaint();
        }
    }

    fn do_rename_all(&mut self) {
        for (row, status) in self.rows.iter_mut().zip(self.statuses.iter()) {
            if let RowStatus::Ready { final_name, .. } = status {
                let target = row.path.with_file_name(final_name);
                match std::fs::rename(&row.path, &target) {
                    Ok(()) => row.renamed_to = Some(final_name.clone()),
                    Err(e) => row.error = Some(format!("Rename failed: {e}")),
                }
            }
        }
        self.finished = true;
    }
}

impl eframe::App for BatchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_workers(ctx);
        let mut needs_resolve = false;

        let to_rename = self
            .statuses
            .iter()
            .filter(|s| matches!(s, RowStatus::Ready { .. }))
            .count();
        let noop = self
            .statuses
            .iter()
            .filter(|s| matches!(s, RowStatus::NoOp))
            .count();
        let errs = self
            .statuses
            .iter()
            .filter(|s| matches!(s, RowStatus::Error))
            .count();
        let working = self
            .statuses
            .iter()
            .filter(|s| matches!(s, RowStatus::Working))
            .count();

        // Bottom bar first so the scroll area can't squeeze it out.
        egui::TopBottomPanel::bottom("batch_bottom").show(ctx, |ui| {
            ui.add_space(4.0);
            if !self.finished {
                ui.label(format!(
                    "{to_rename} to rename • {noop} already named • {errs} errors"
                ));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let can_go = to_rename > 0 && working == 0;
                    if ui
                        .add_enabled(can_go, egui::Button::new("Rename All"))
                        .clicked()
                    {
                        self.do_rename_all();
                    }
                    if ui.button("Cancel").clicked() {
                        std::process::exit(0);
                    }
                    if working > 0 {
                        ui.label(format!("Working on {working}…"));
                    }
                });
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(
                        "Tip: edit any proposed name — collisions are re-checked across all files.",
                    )
                    .small()
                    .weak(),
                );
            } else {
                let ok = self.rows.iter().filter(|r| r.renamed_to.is_some()).count();
                let failed = self.rows.iter().filter(|r| r.error.is_some()).count();
                ui.label(format!("Done: {ok} renamed • {failed} failed."));
                ui.add_space(4.0);
                if ui.button("Close").clicked() {
                    std::process::exit(0);
                }
            }
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(format!("Batch rename — {} files", self.rows.len()));
            ui.add_space(4.0);
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("batch_grid")
                    .striped(true)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("File").strong());
                        ui.label(egui::RichText::new("Proposed name").strong());
                        ui.label(egui::RichText::new("Final name").strong());
                        ui.label(egui::RichText::new("Source").strong());
                        ui.end_row();
                        for i in 0..self.rows.len() {
                            ui.label(egui::RichText::new(&self.rows[i].current_name).small());
                            ui.horizontal(|ui| {
                                let resp = ui.add(
                                    egui::TextEdit::singleline(&mut self.rows[i].edit_buf)
                                        .desired_width(200.0),
                                );
                                if resp.changed() {
                                    self.rows[i].manual = true;
                                    self.rows[i].ready = true;
                                    needs_resolve = true;
                                }
                                if !self.rows[i].ext.is_empty() {
                                    ui.label(format!(".{}", self.rows[i].ext));
                                }
                            });
                            // Final-name column: always show the real filename.
                            match &self.statuses[i] {
                                RowStatus::Working => {
                                    ui.label(egui::RichText::new("…").weak());
                                }
                                RowStatus::Error => {
                                    ui.label(
                                        egui::RichText::new("error").color(egui::Color32::RED),
                                    );
                                }
                                RowStatus::NoOp => {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "✓ {}",
                                            self.rows[i].current_name
                                        ))
                                        .small()
                                        .weak(),
                                    );
                                }
                                RowStatus::Ready { final_name, suffixed } => {
                                    if self.finished && self.rows[i].renamed_to.is_some() {
                                        ui.label(
                                            egui::RichText::new(format!("✓ {final_name}"))
                                                .color(egui::Color32::GREEN),
                                        );
                                    } else {
                                        let mut t = egui::RichText::new(final_name).small();
                                        if *suffixed {
                                            t = t.strong();
                                        }
                                        ui.label(t);
                                    }
                                }
                            }
                            // Source / error column.
                            if let Some(e) = &self.rows[i].error {
                                ui.label(
                                    egui::RichText::new(e).color(egui::Color32::RED).small(),
                                );
                            } else if let Some(s) = &self.rows[i].date_source {
                                ui.label(egui::RichText::new(s).small().weak());
                            } else {
                                ui.label("");
                            }
                            ui.end_row();
                        }
                    });
            });
        });
        if needs_resolve {
            self.re_resolve();
        }
    }
}

/// Multi-select coordination: Windows may invoke the verb once per file
/// despite MultiSelectModel=Player. The first instance becomes the
/// coordinator; later instances drop their file paths and exit silently.
/// The coordinator collects stragglers briefly, then opens one window.
fn coordinate_files(files: Vec<PathBuf>) -> Vec<PathBuf> {
    use std::time::{Duration, Instant};
    // Fast path: Player model delivered everything at once (or nothing to do).
    if files.len() != 1 {
        return files;
    }
    let drop_dir = std::env::temp_dir().join("FileNamerAI-batch");
    let _ = std::fs::create_dir_all(&drop_dir);
    let lock_path = drop_dir.join("coordinator.lock");
    let is_fresh = |p: &Path| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().unwrap_or_default() < Duration::from_secs(10))
            .unwrap_or(false)
    };

    // Try to become coordinator (atomic). Clear a stale lock first.
    let lock = (|| -> Option<std::fs::File> {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(f) => Some(f),
            Err(_) => {
                if !is_fresh(&lock_path) {
                    let _ = std::fs::remove_file(&lock_path);
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&lock_path)
                        .ok()
                } else {
                    None
                }
            }
        }
    })();

    let Some(lock) = lock else {
        // Not the coordinator: hand our file over and exit quietly.
        let tag: String = files[0]
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        let dest = drop_dir.join(format!("{}_{}.txt", std::process::id(), tag));
        let _ = std::fs::write(&dest, files[0].to_string_lossy().as_bytes());
        std::process::exit(0);
    };

    // Coordinator: sweep stale drops, then collect live stragglers.
    if let Ok(rd) = std::fs::read_dir(&drop_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p != lock_path
                && p.extension().and_then(|s| s.to_str()) == Some("txt")
                && !is_fresh(&p)
            {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    let mut all = files;
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(900) {
        if let Ok(rd) = std::fs::read_dir(&drop_dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p == lock_path
                    || p.extension().and_then(|s| s.to_str()) != Some("txt")
                    || !is_fresh(&p)
                {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&p) {
                    let fp = PathBuf::from(content.trim());
                    if fp.is_file() && !all.contains(&fp) {
                        all.push(fp);
                    }
                }
                let _ = std::fs::remove_file(&p);
            }
        }
        std::thread::sleep(Duration::from_millis(60));
    }
    drop(lock);
    let _ = std::fs::remove_file(&lock_path);
    all
}

fn main() {
    // Registry migration for installs that predate multi-select support.
    ensure_multiselect();

    let files: Vec<PathBuf> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .collect();
    // One window for a multi-select, even if the shell invoked us per-file.
    let files = coordinate_files(files);
    let batch = files.len() > 1;
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("File Namer AI")
        .with_inner_size(if batch { [760.0, 540.0] } else { [600.0, 560.0] });
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let _ = eframe::run_native(
        "File Namer AI",
        options,
        Box::new(move |cc| {
            if batch {
                Box::new(BatchApp::new(cc, files.clone())) as Box<dyn eframe::App>
            } else {
                Box::new(NamerApp::new(cc, files.clone())) as Box<dyn eframe::App>
            }
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn docx_xml_to_plain_text_keeps_paragraphs_and_unescapes() {
        let xml = r#"<?xml version="1.0"?><w:document><w:body><w:p><w:r><w:t>Screening Notification</w:t></w:r></w:p><w:p><w:r><w:t>From: First Advantage &amp; Co</w:t></w:r></w:p><w:p><w:r><w:t>26-Oct-2026</w:t></w:r></w:p></w:body></w:document>"#;
        let plain = xml_to_plain_text(xml);
        assert_eq!(
            plain,
            "Screening Notification\nFrom: First Advantage & Co\n26-Oct-2026"
        );
    }

    #[test]
    fn docx_extraction_reads_document_xml_from_a_zip() {
        let dir = std::env::temp_dir().join(format!("filenamer_docx_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.docx");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file("word/document.xml", opts).unwrap();
            zip.write_all(
                br#"<?xml version="1.0"?><w:document><w:body><w:p><w:r><w:t>Quarterly Report</w:t></w:r></w:p><w:p><w:r><w:t>Revenue grew in Q3 2026</w:t></w:r></w:p></w:body></w:document>"#,
            )
            .unwrap();
            zip.finish().unwrap();
        }
        match extract_content(&path) {
            Ok(Extracted::Text(text)) => {
                assert!(text.contains("Quarterly Report"), "got: {text}");
                assert!(text.contains("Revenue grew in Q3 2026"), "got: {text}");
            }
             _ => panic!("expected Text extraction"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_binary_type_falls_back_to_date_not_an_error() {
        let dir = std::env::temp_dir().join(format!("filenamer_bin_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("mystery.xyzbin");
        std::fs::write(&path, [0xffu8, 0xfe, 0x00, 0x01]).unwrap();
        match extract_content(&path) {
            Ok(Extracted::DateTime(_, _)) => {}
             _ => panic!("expected date fallback"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

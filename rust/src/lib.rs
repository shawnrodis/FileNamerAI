//! File Namer AI — Rust core (RustDesk-style: Rust logic, Flutter UI).
//! Extracted from the egui app (branch egui-legacy, v1.3.20), with ALL
//! egui UI code stripped. The Flutter UI calls this library through a
//! plain C ABI (see the fna_* exports at the bottom) via Dart's ffi
//! package. No flutter_rust_bridge codegen is used: codegen output is
//! version-sensitive to the Flutter/Dart toolchain and cannot be
//! verified on a Linux VM without a Flutter SDK, whereas a C ABI
//! cdylib needs only `cargo build` on the Actions Windows runner.
//! Behaviour is the egui logic, unchanged: photo/video date naming,
//! document extraction, Ollama naming guards, model pull, menu
//! registration, and batch collision resolution.

use chrono::{Datelike, Timelike};
use serde::{Deserialize, Serialize};
use std::ffi::{CStr, CString};
use std::io::{BufReader, Read, Write};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
const DEFAULT_MODEL: &str = "qwen2.5:3b";
const DEFAULT_VISION_MODEL: &str = "moondream";
const MAX_CONTENT_CHARS: usize = 4000;
const MENU_KEY: &str = r"HKCU\Software\Classes\*\shell\FileNamerAI";
const OLLAMA_SETUP_URL: &str = "https://ollama.com/download/OllamaSetup.exe";

#[cfg(windows)]
fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x08000000);
}
#[cfg(not(windows))]
fn no_window(_cmd: &mut Command) {}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PhotoSettings {
    pub date_order: String,
    pub separator: String,
    pub include_time: bool,
    pub seconds: String,
    pub source: String,
    pub collision_suffix: String,
    pub prefix: String,
    pub suffix_text: String,
}
impl Default for PhotoSettings {
    fn default() -> Self {
        Self {
            date_order: "mm-dd-yy".into(),
            separator: "-".into(),
            include_time: true,
            seconds: "current".into(),
            source: "earliest".into(),
            collision_suffix: "_1".into(),
            prefix: String::new(),
            suffix_text: String::new(),
        }
    }
}
impl PhotoSettings {
    fn validated(mut self) -> Self {
        if !["mm-dd-yy", "yyyy-mm-dd", "dd-mm-yy"].contains(&self.date_order.as_str()) {
            self.date_order = "mm-dd-yy".into();
        }
        if self.separator != "-" && self.separator != "_" {
            self.separator = "-".into();
        }
        if !["current", "always", "never"].contains(&self.seconds.as_str()) {
            self.seconds = "current".into();
        }
        if !["earliest", "latest", "exif_only"].contains(&self.source.as_str()) {
            self.source = "earliest".into();
        }
        if !["_1", "-1", "(1)"].contains(&self.collision_suffix.as_str()) {
            self.collision_suffix = "_1".into();
        }
        self
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub ollama_url: String,
    pub model: String,
    pub vision_model: String,
    pub append_date: bool,
    pub never_dates: bool,
    pub force_ocr: bool,
    pub photo: PhotoSettings,
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
            photo: PhotoSettings::default(),
        }
    }
}
fn parse_config(json: &str) -> Config {
    let mut cfg: Config = serde_json::from_str(json).unwrap_or_default();
    cfg.photo = cfg.photo.validated();
    cfg
}

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
/// Default selection: the EARLIEST timestamp wins: file operations (copy,
/// rename, edit) happen at or after capture, so the earliest date is
/// closest to the original. Advanced settings can switch this to latest,
/// or to EXIF/media date only (file dates as fallback).
fn best_datetime_with_settings(
    path: &Path,
    is_video: bool,
    settings: &PhotoSettings,
) -> (chrono::NaiveDateTime, &'static str) {
    let mut primary: Vec<(chrono::NaiveDateTime, &'static str)> = Vec::new();
    // Seconds rule:
    // - "current" (default, per Shawn 2026-10-22, BRU parity):
    //   videos always truncate to :00; photos with real EXIF keep full
    //   seconds; photos without EXIF (screenshots, downloads) truncate.
    // - "always": truncate every candidate to :00.
    // - "never": keep full seconds for every candidate.
    // Applied before comparison so priority order is preserved.
    let has_real_exif = !is_video && has_datetime_original(path);
    let truncate = match settings.seconds.as_str() {
        "always" => true,
        "never" => false,
        _ => is_video || !has_real_exif,
    };
    let trunc = |dt: chrono::NaiveDateTime| -> chrono::NaiveDateTime {
        if truncate {
            dt.with_second(0)
                .and_then(|d| d.with_nanosecond(0))
                .unwrap_or(dt)
        } else {
            dt
        }
    };
    if is_video {
        if let Some(dt) = mp4_creation_datetime(path) {
            primary.push((trunc(dt), "video media date"));
        }
    } else if let Some(dt) = exif_datetime(path) {
        primary.push((trunc(dt), "EXIF date taken"));
    }
    let mut file_cands: Vec<(chrono::NaiveDateTime, &'static str)> = Vec::new();
    if let Ok(md) = std::fs::metadata(path) {
        if let Ok(st) = md.created() {
            let dt: chrono::DateTime<chrono::Local> = st.into();
            file_cands.push((trunc(dt.naive_local()), "file created date"));
        }
        if let Ok(st) = md.modified() {
            let dt: chrono::DateTime<chrono::Local> = st.into();
            file_cands.push((trunc(dt.naive_local()), "file modified date"));
        }
    }
    // EXIF-only: use the primary date alone when it exists; otherwise
    // fall back to the file dates (earliest of them) and label that
    // fallback honestly, so the UI never claims an EXIF date it didn't use.
    let cands: Vec<(chrono::NaiveDateTime, &'static str)> =
        if settings.source == "exif_only" {
            if primary.is_empty() {
                file_cands.clone()
            } else {
                primary
            }
        } else {
            primary.into_iter().chain(file_cands).collect()
        };
    if settings.source == "exif_only" && !cands.is_empty() {
        // Fallback path above already narrowed to file dates; pick earliest
        // among them regardless of the latest setting elsewhere.
        // (When a primary date exists, cands has exactly one entry.)
        if cands.len() > 1 {
            return cands
                .into_iter()
                .min_by_key(|(dt, _)| *dt)
                .unwrap();
        }
        return cands.into_iter().next().unwrap();
    }
    if settings.source == "latest" {
        cands.into_iter().max_by_key(|(dt, _)| *dt)
    } else {
        cands.into_iter().min_by_key(|(dt, _)| *dt)
    }
    .unwrap_or_else(|| {
        (
            chrono::Local::now().naive_local(),
            "current time (no date found)",
        )
    })
}

/// Single formatting path for photo/video date names, used by BOTH the
/// single-file and batch DateTime paths so they cannot drift apart.
/// Default output is exactly MM-DD-YY-HH-MM-SS (prefix/suffix blank).
/// The separator joins every component, including date-to-time.
fn format_photo_name(dt: chrono::NaiveDateTime, settings: &PhotoSettings) -> String {
    let sep = if settings.separator == "_" { "_" } else { "-" };
    let date = match settings.date_order.as_str() {
        "yyyy-mm-dd" => format!(
            "{:04}{}{:02}{}{:02}",
            dt.year(),
            sep,
            dt.month(),
            sep,
            dt.day()
        ),
        "dd-mm-yy" => format!(
            "{:02}{}{:02}{}{:02}",
            dt.day(),
            sep,
            dt.month(),
            sep,
            dt.year() % 100
        ),
        _ => format!(
            "{:02}{}{:02}{}{:02}",
            dt.month(),
            sep,
            dt.day(),
            sep,
            dt.year() % 100
        ),
    };
    let base = if settings.include_time {
        format!(
            "{}{}{:02}{}{:02}{}{:02}",
            date,
            sep,
            dt.hour(),
            sep,
            dt.minute(),
            sep,
            dt.second()
        )
    } else {
        date
    };
    format!("{}{}{}", settings.prefix, base, settings.suffix_text)
}

/// Collision suffix for date names only (AI document names keep "_1").
fn collision_name(stem: &str, n: u32, style: &str) -> String {
    match style {
        "-1" => format!("{stem}-{n}"),
        "(1)" => format!("{stem} ({n})"),
        _ => format!("{stem}_{n}"),
    }
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
// Default-settings entry point, used by tests; production paths go
// through extract_content_with_config with the user's settings.
#[allow(dead_code)]
fn extract_content(path: &Path) -> Result<Extracted, String> {
    extract_content_with_config(path, &PhotoSettings::default())
}

fn extract_content_with_config(
    path: &Path,
    settings: &PhotoSettings,
) -> Result<Extracted, String> {
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
            let (dt, src) = best_datetime_with_settings(path, false, settings);
            Ok(Extracted::DateTime(dt, src))
        }
        "mp4" | "mov" | "m4v" | "3gp" | "avi" | "mkv" | "webm" => {
            // Videos: embedded media-created date, else file dates. No AI.
            let (dt, src) = best_datetime_with_settings(path, true, settings);
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
            let (dt, src) = best_datetime_with_settings(path, false, settings);
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

const MONTHS_FULL: [&str; 12] = [
    "january", "february", "march", "april", "may", "june", "july", "august", "september",
    "october", "november", "december",
];
const MONTHS_ABBR: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];


pub fn check_models_sync(url: &str) -> Result<Vec<String>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Couldn't start HTTP client: {e}"))?;
    let resp = client
        .get(format!("{}/api/tags", url.trim_end_matches('/')))
        .send()
        .map_err(|e| format!("Check failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Ollama returned HTTP {}.", resp.status()));
    }
    let v: serde_json::Value = resp.json().map_err(|e| format!("Bad response: {e}"))?;
    Ok(v["models"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
        .collect())
}

pub fn pull_model_sync(url: &str, model: &str) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3600))
        .build()
        .map_err(|e| format!("Couldn't start HTTP client: {e}"))?;
    let resp = client
        .post(format!("{}/api/pull", url.trim_end_matches('/')))
        .json(&serde_json::json!({"name": model, "stream": true}))
        .send()
        .map_err(|e| format!("Pull failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Ollama returned HTTP {}.", resp.status()));
    }
    use std::io::BufRead;
    let reader = std::io::BufReader::new(resp);
    for line in reader.lines() {
        let line = match line { Ok(l) => l, Err(_) => break };
        if line.trim().is_empty() { continue; }
        let v: serde_json::Value = match serde_json::from_str(&line) { Ok(v) => v, Err(_) => continue };
        if let Some(err) = v["error"].as_str() {
            return Err(err.to_string());
        }
        if v["status"].as_str() == Some("success") {
            return Ok(());
        }
    }
    Ok(())
}

pub fn download_ollama_installer_sync() -> Result<PathBuf, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3600))
        .user_agent("FileNamerAI-ollama-download")
        .build()
        .map_err(|e| format!("Couldn't start HTTP client: {e}"))?;
    let mut resp = client
        .get(OLLAMA_SETUP_URL)
        .send()
        .map_err(|e| format!("Download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Download failed (HTTP {}).", resp.status()));
    }
    let dest = std::env::temp_dir().join("OllamaSetup.exe");
    let mut file = std::fs::File::create(&dest)
        .map_err(|_| "Couldn't write the installer file.".to_string())?;
    let mut buf = [0u8; 8192];
    loop {
        match resp.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                file.write_all(&buf[..n])
                    .map_err(|_| "Couldn't write the installer file.".to_string())?;
            }
            Err(_) => return Err("Download interrupted.".into()),
        }
    }
    match std::fs::metadata(&dest) {
        Ok(m) if m.len() > 0 => Ok(dest),
        _ => Err("Downloaded file is empty.".into()),
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

/// "T_1" / "T-1" / "T (1)" -> "T" (for the given style); "T" -> "T".
/// Used so a collision suffix doesn't stop us recognizing an
/// already-correctly-named file. The legacy "_1" form is always also
/// recognised, so files named under the old default still match after
/// a style switch. Note the "-" form is ambiguous with "-" date
/// separators (a plain "10-26-26" date ends in "-26"); that is safe in
/// practice because no-op detection first checks exact equality, and
/// suffix stripping only matters for names already carrying a suffix.
fn strip_numeric_suffix<'a>(stem: &'a str, style: &str) -> &'a str {
    fn strip_sep<'a>(stem: &'a str, sep: char) -> Option<&'a str> {
        let i = stem.rfind(sep)?;
        let (base, suffix) = stem.split_at(i);
        let digits = &suffix[1..];
        if !base.is_empty() && !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            Some(base)
        } else {
            None
        }
    }
    if style == "(1)" && stem.ends_with(')') {
        if let Some(i) = stem.rfind(" (") {
            let digits = &stem[i + 2..stem.len() - 1];
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                return &stem[..i];
            }
        }
    } else if style == "-1" {
        if let Some(base) = strip_sep(stem, '-') {
            return base;
        }
    }
    // Default style, and legacy fallback for every style.
    if let Some(base) = strip_sep(stem, '_') {
        return base;
    }
    stem
}

/// If "stem.ext" already exists next to the file, return "stem_1",
/// "stem_2", ... (default style). The file's own current name is never
/// treated as a collision — including when it carries a _N suffix from
/// an earlier batch (already correctly named -> keep as-is).
fn unique_stem(path: &Path, stem: &str, ext: &str) -> String {
    unique_stem_with_style(path, stem, ext, "_1")
}

/// Same as unique_stem, but with the advanced photo/video collision
/// suffix style. Only date names use this; AI document names keep "_1".
fn unique_stem_with_style(path: &Path, stem: &str, ext: &str, style: &str) -> String {
    if let Some(cur) = path.file_stem().and_then(|s| s.to_str()) {
        if cur == stem || strip_numeric_suffix(cur, style) == stem {
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
        let s = collision_name(stem, n, style);
        if ok(&s) {
            return s;
        }
    }
    stem.to_string()
}

// ---------------------------------------------------------------------------
// app
// ---------------------------------------------------------------------------

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
    match extract_content_with_config(path, &cfg.photo) {
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


// ---------------------------------------------------------------------------
// Dart config mapping + JSON resolve + C ABI exports
// ---------------------------------------------------------------------------

fn parse_config_dart(json: &str) -> Config {
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or(serde_json::Value::Null);
    let get_s = |k: &str, d: &str| -> String {
        v.get(k).and_then(|x| x.as_str()).unwrap_or(d).to_string()
    };
    let get_b = |k: &str, d: bool| -> bool {
        v.get(k).and_then(|x| x.as_bool()).unwrap_or(d)
    };
    let date_order = match get_s("dateOrder", "MM-DD-YY").as_str() {
        "DD-MM-YY" => "dd-mm-yy",
        "YY-MM-DD" => "yyyy-mm-dd",
        _ => "mm-dd-yy",
    };
    let seconds = match get_s("secondsRule", "auto").as_str() {
        "keep" => "never",
        "truncate" => "always",
        _ => "current",
    };
    let source = match get_s("timestampSource", "earliest").as_str() {
        "latest" => "latest",
        "exif-only" => "exif_only",
        _ => "earliest",
    };
    let mut cfg = Config {
        ollama_url: get_s("ollamaUrl", DEFAULT_OLLAMA_URL),
        model: get_s("model", DEFAULT_MODEL),
        vision_model: get_s("visionModel", DEFAULT_VISION_MODEL),
        append_date: get_b("appendDate", true),
        never_dates: get_b("neverDates", false),
        force_ocr: v.get("forceOcr").and_then(|x| x.as_bool()).unwrap_or(false),
        photo: PhotoSettings {
            date_order: date_order.into(),
            separator: get_s("separator", "-"),
            include_time: get_b("timeOn", true),
            seconds: seconds.into(),
            source: source.into(),
            collision_suffix: "_1".into(),
            prefix: get_s("prefix", ""),
            suffix_text: get_s("suffix", ""),
        },
    };
    cfg.photo = cfg.photo.validated();
    cfg
}

fn propose_file(path: &str, config_json: &str, attempt: u32) -> serde_json::Value {
    let cfg = parse_config_dart(config_json);
    let p = Path::new(path);
    match extract_for_naming(&cfg, p) {
        Ok(Extracted::DateTime(dt, src)) => serde_json::json!({
            "kind": "date",
            "proposed": format_photo_name(dt, &cfg.photo),
            "date_source": src,
        }),
        Ok(Extracted::Text(content)) => {
            if !ollama_up(&cfg.ollama_url) {
                return serde_json::json!({"kind": "need_ollama", "error": "Ollama isn't running. Start it, then try again."});
            }
            match ask_ollama(&cfg, p, &content, attempt) {
                Ok(name) => serde_json::json!({"kind": "document", "proposed": name}),
                Err(e) => serde_json::json!({"kind": "error", "error": e}),
            }
        }
        Err(e) => serde_json::json!({"kind": "error", "error": e}),
    }
}

#[derive(Debug, Deserialize)]
struct ResolveRow {
    path: String,
    current_name: String,
    ext: String,
    proposed: String,
    ready: bool,
    error: bool,
    date_source: Option<String>,
}

fn resolve_batch_json(input: &str) -> serde_json::Value {
    let v: serde_json::Value = serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
    let photo_style = v
        .get("photo_collision_suffix")
        .and_then(|x| x.as_str())
        .unwrap_or("_1")
        .to_string();
    let rows: Vec<ResolveRow> = v
        .get("rows")
        .and_then(|x| serde_json::from_value(x.clone()).ok())
        .unwrap_or_default();
    use std::collections::{HashMap, HashSet};
    let mut out: Vec<serde_json::Value> = vec![];
    for _ in &rows {
        out.push(serde_json::json!({"final_name": "", "status": "working", "suffixed": false}));
    }
    let eff: Vec<Option<String>> = rows
        .iter()
        .map(|r| {
            if r.error || !r.ready {
                None
            } else {
                let s = sanitize_filename(&r.proposed);
                if s.is_empty() { None } else { Some(s) }
            }
        })
        .collect();
    for (i, r) in rows.iter().enumerate() {
        if r.error {
            out[i] = serde_json::json!({"final_name": r.current_name, "status": "error", "suffixed": false});
        }
    }
    let mut by_dir: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        if eff[i].is_none() { continue; }
        let dir = Path::new(&r.path).parent().map(|p| p.to_path_buf()).unwrap_or_default();
        by_dir.entry(dir).or_default().push(i);
    }
    for (dir, indices) in by_dir {
        let mut taken: HashSet<String> = HashSet::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if let Some(n) = e.file_name().to_str() {
                    taken.insert(n.to_lowercase());
                }
            }
        }
        let mut is_noop: HashMap<usize, bool> = HashMap::new();
        for &i in &indices {
            let r = &rows[i];
            let s = eff[i].as_ref().unwrap();
            let cur_stem = Path::new(&r.path)
                .file_stem()
                .and_then(|x| x.to_str())
                .unwrap_or(&r.current_name)
                .to_string();
            let style = if r.date_source.is_some() { photo_style.as_str() } else { "_1" };
            let noop = s.to_lowercase() == cur_stem.to_lowercase()
                || s.to_lowercase() == strip_numeric_suffix(&cur_stem.to_lowercase(), style);
            // strip_numeric_suffix expects the original case-insensitive form;
            // the egui code compares lowercased stems, and strip_numeric_suffix
            // on a lowercased stem is equivalent for _N / -N / (N) suffixes.
            is_noop.insert(i, noop);
            if noop {
                out[i] = serde_json::json!({"final_name": r.current_name, "status": "noop", "suffixed": false});
            } else {
                taken.remove(&r.current_name.to_lowercase());
            }
        }
        let mut groups: HashMap<(String, String), Vec<usize>> = HashMap::new();
        for &i in &indices {
            if is_noop[&i] { continue; }
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
            let full = |s: &str| -> String {
                if ext.is_empty() { s.to_string() } else { format!("{s}.{ext}") }
            };
            if members.len() == 1 && !taken.contains(&full(&stem).to_lowercase()) {
                taken.insert(full(&stem).to_lowercase());
                out[members[0]] = serde_json::json!({"final_name": full(&stem), "status": "ready", "suffixed": false});
            } else {
                let mut n = 1u32;
                for &idx in &members {
                    let style = if rows[idx].date_source.is_some() { photo_style.as_str() } else { "_1" };
                    if style.is_empty() {
                        // Collision suffix disabled in the UI: keep the stem.
                        taken.insert(full(&stem).to_lowercase());
                        out[idx] = serde_json::json!({"final_name": full(&stem), "status": "ready", "suffixed": false});
                        continue;
                    }
                    loop {
                        let cand = collision_name(&stem, n, style);
                        if n > 9999 { break; }
                        if !taken.contains(&full(&cand).to_lowercase()) {
                            taken.insert(full(&cand).to_lowercase());
                            out[idx] = serde_json::json!({"final_name": full(&cand), "status": "ready", "suffixed": true});
                            n += 1;
                            break;
                        }
                        n += 1;
                    }
                }
            }
        }
    }
    serde_json::json!({"results": out})
}

fn install_menu_with_exe(exe_path: &str) -> Result<(), String> {
    let exe_s = if exe_path.is_empty() {
        std::env::current_exe().map_err(|e| e.to_string())?.to_string_lossy().into_owned()
    } else {
        exe_path.to_string()
    };
    run_reg(&["add", MENU_KEY, "/ve", "/d", "Rename with File Namer AI", "/f"])?;
    run_reg(&["add", MENU_KEY, "/v", "Icon", "/d", &exe_s, "/f"])?;
    run_reg(&["add", MENU_KEY, "/v", "MultiSelectModel", "/d", "Player", "/f"])?;
    let cmd_key = format!("{MENU_KEY}\\command");
    let cmd = format!("\"{exe_s}\" \"%1\"");
    run_reg(&["add", &cmd_key, "/ve", "/d", &cmd, "/f"])?;
    Ok(())
}

fn cstr_to_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() }
}
fn string_to_ptr(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}
fn json_ptr(v: serde_json::Value) -> *mut c_char {
    string_to_ptr(v.to_string())
}

#[no_mangle]
pub extern "C" fn fna_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe { let _ = CString::from_raw(ptr); }
    }
}

#[no_mangle]
pub extern "C" fn fna_version() -> *mut c_char {
    string_to_ptr("1.4.2-rust".to_string())
}

#[no_mangle]
pub extern "C" fn fna_propose(path: *const c_char, config_json: *const c_char, attempt: u32) -> *mut c_char {
    let p = cstr_to_string(path);
    let c = cstr_to_string(config_json);
    json_ptr(propose_file(&p, &c, attempt))
}

#[no_mangle]
pub extern "C" fn fna_extract_text(path: *const c_char, config_json: *const c_char) -> *mut c_char {
    let cfg = parse_config_dart(&cstr_to_string(config_json));
    let path_s = cstr_to_string(path);
    let p = Path::new(&path_s);
    let v = match extract_for_naming(&cfg, p) {
        Ok(Extracted::Text(t)) => serde_json::json!({"kind": "text", "text": t}),
        Ok(Extracted::DateTime(dt, src)) => serde_json::json!({"kind": "date", "proposed": format_photo_name(dt, &cfg.photo), "date_source": src}),
        Err(e) => serde_json::json!({"kind": "error", "error": e}),
    };
    json_ptr(v)
}

#[no_mangle]
pub extern "C" fn fna_resolve_batch(input_json: *const c_char) -> *mut c_char {
    json_ptr(resolve_batch_json(&cstr_to_string(input_json)))
}

#[no_mangle]
pub extern "C" fn fna_check_models(ollama_url: *const c_char) -> *mut c_char {
    match check_models_sync(&cstr_to_string(ollama_url)) {
        Ok(models) => json_ptr(serde_json::json!({"models": models})),
        Err(e) => json_ptr(serde_json::json!({"error": e})),
    }
}

#[no_mangle]
pub extern "C" fn fna_pull_model(ollama_url: *const c_char, model: *const c_char) -> *mut c_char {
    match pull_model_sync(&cstr_to_string(ollama_url), &cstr_to_string(model)) {
        Ok(()) => json_ptr(serde_json::json!({"ok": true})),
        Err(e) => json_ptr(serde_json::json!({"ok": false, "error": e})),
    }
}

#[no_mangle]
pub extern "C" fn fna_download_ollama_installer() -> *mut c_char {
    match download_ollama_installer_sync() {
        Ok(p) => json_ptr(serde_json::json!({"path": p.to_string_lossy()})),
        Err(e) => json_ptr(serde_json::json!({"error": e})),
    }
}

#[no_mangle]
pub extern "C" fn fna_ensure_multiselect() {
    ensure_multiselect();
}

#[no_mangle]
pub extern "C" fn fna_menu_installed() -> i32 {
    if menu_installed() { 1 } else { 0 }
}

#[no_mangle]
pub extern "C" fn fna_install_menu(exe_path: *const c_char) -> *mut c_char {
    match install_menu_with_exe(&cstr_to_string(exe_path)) {
        Ok(()) => json_ptr(serde_json::json!({"ok": true})),
        Err(e) => json_ptr(serde_json::json!({"ok": false, "error": e})),
    }
}

#[no_mangle]
pub extern "C" fn fna_uninstall_menu() -> *mut c_char {
    match uninstall_menu() {
        Ok(()) => json_ptr(serde_json::json!({"ok": true})),
        Err(e) => json_ptr(serde_json::json!({"ok": false, "error": e})),
    }
}

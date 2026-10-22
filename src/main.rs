#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! File Namer AI - right-click AI file renaming powered by a local LLM (Ollama).

use eframe::egui;
use chrono::Datelike;
use std::io::BufReader;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const CREATE_NO_WINDOW: u32 = 0x08000000;
const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
const DEFAULT_MODEL: &str = "qwen2.5:3b";
const MAX_CONTENT_CHARS: usize = 4000;
const MENU_KEY: &str = r"HKCU\Software\Classes\*\shell\FileNamerAI";

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Config {
    ollama_url: String,
    model: String,
    append_date: bool,
    never_dates: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ollama_url: DEFAULT_OLLAMA_URL.into(),
            model: DEFAULT_MODEL.into(),
            append_date: true,
            never_dates: false,
        }
    }
}

fn config_path() -> PathBuf {
    let mut p = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    p.set_file_name("FileNamerAI.ini");
    p
}

fn load_config() -> Config {
    let mut cfg = Config::default();
    let text = std::fs::read_to_string(config_path()).unwrap_or_default();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "ollama_url" => cfg.ollama_url = v.trim().to_string(),
                "model" => cfg.model = v.trim().to_string(),
                "append_date" => cfg.append_date = v.trim() != "false",
                "never_dates" => cfg.never_dates = v.trim() == "true",
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
    cfg
}

fn save_config(cfg: &Config) {
    let s = format!(
        "ollama_url={}\nmodel={}\nappend_date={}\nnever_dates={}\n",
        cfg.ollama_url.trim(),
        cfg.model.trim(),
        cfg.append_date,
        cfg.never_dates
    );
    let _ = std::fs::write(config_path(), s);
}

fn is_dark_mode() -> bool {
    let out = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
            "/v",
            "AppsUseLightTheme",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) => {
            let t = String::from_utf8_lossy(&o.stdout);
            t.lines()
                .any(|l| l.contains("AppsUseLightTheme") && l.trim_end().ends_with("0x0"))
        }
        Err(_) => false,
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
    if is_video {
        if let Some(dt) = mp4_creation_datetime(path) {
            cands.push((dt, "video media date"));
        }
    } else if let Some(dt) = exif_datetime(path) {
        cands.push((dt, "EXIF date taken"));
    }
    if let Ok(md) = std::fs::metadata(path) {
        if let Ok(st) = md.created() {
            let dt: chrono::DateTime<chrono::Local> = st.into();
            cands.push((dt.naive_local(), "file created date"));
        }
        if let Ok(st) = md.modified() {
            let dt: chrono::DateTime<chrono::Local> = st.into();
            cands.push((dt.naive_local(), "file modified date"));
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
    chrono::DateTime::from_timestamp(creation as i64 - QT_TO_UNIX, 0).map(|dt| dt.naive_local())
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
        _ => Err(format!(
            "Unsupported file type: .{ext} (PDF, text, photos and videos work)"
        )),
    }
}

// ---------------------------------------------------------------------------
// ollama
// ---------------------------------------------------------------------------

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
        "- If the content contains an obvious document date (invoice date, letter date, statement date), append it after an underscore as _DD-Mon-YYYY with the month capitalized, e.g. Saw Palmetto Price Comparison_28-May-2023.\n\
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
         Rules (follow exactly):\n\
         - Output ONLY the filename: no quotes, no explanation, no file extension.\n\
         - Use spaces between words with normal Title Case (e.g. Saw Palmetto Price Comparison). Never ALL CAPS.\n\
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
    let out = Command::new("reg")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
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
    let cmd_key = format!("{MENU_KEY}\\command");
    let cmd = format!("\"{exe_s}\" \"%1\"");
    run_reg(&["add", &cmd_key, "/ve", "/d", &cmd, "/f"])?;
    Ok(())
}

fn uninstall_menu() -> Result<(), String> {
    run_reg(&["delete", MENU_KEY, "/f"])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// auto update check (GitHub Releases)
// ---------------------------------------------------------------------------

const UPDATE_REPO: &str = "shawnrodis/FileNamerAI";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

enum UpdateMsg {
    Available(String, String), // (version tag, release url)
    UpToDate,
    CheckFailed,
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
    let url = v["html_url"].as_str().unwrap_or("").to_string();
    if tag.is_empty() || url.is_empty() {
        send(UpdateMsg::CheckFailed);
        return;
    }
    if parse_ver(&tag) > parse_ver(APP_VERSION) {
        send(UpdateMsg::Available(tag, url));
    } else {
        send(UpdateMsg::UpToDate);
    }
}

fn open_url(url: &str) {
    let _ = Command::new("cmd")
        .args(["/c", "start", "", url])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
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

/// If "stem.ext" already exists next to the file, return "stem-02",
/// "stem-03", ... (burst shots share a timestamp). The file's own current
/// name is never treated as a collision.
fn unique_stem(path: &Path, stem: &str, ext: &str) -> String {
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
    for n in 2..100 {
        let s = format!("{stem}-{n:02}");
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
    append_date: bool,
    never_dates: bool,
    settings_status: String,
    menu_on: bool,
    // rename flow
    rx: Option<mpsc::Receiver<WorkerMsg>>,
    state: RenameState,
    suggestion: String,
    date_source: Option<String>,
    attempt: u32,
    show_about: bool,
    // update check
    update_rx: Option<mpsc::Receiver<UpdateMsg>>,
    update_info: Option<(String, String)>,
    update_dismissed: bool,
    update_status: String,
    update_pending: bool,
}

fn rename_worker(path: PathBuf, cfg: Config, tx: mpsc::Sender<WorkerMsg>, attempt: u32) {
    let send = |m| {
        let _ = tx.send(m);
    };
    let extracted = match extract_content(&path) {
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

impl NamerApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(if is_dark_mode() {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });

        let cfg = load_config();
        let args: Vec<String> = std::env::args().skip(1).collect();
        let file_path = args
            .iter()
            .find(|a| !a.starts_with('-'))
            .map(PathBuf::from)
            .filter(|p| p.is_file());

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
            append_date: cfg.append_date,
            never_dates: cfg.never_dates,
            cfg,
            settings_status: String::new(),
            menu_on: menu_installed(),
            rx: None,
            state: RenameState::Working,
            suggestion: String::new(),
            date_source: None,
            attempt: 0,
            show_about: false,
            update_rx: None,
            update_info: None,
            update_dismissed: false,
            update_status: String::new(),
            update_pending: false,
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
        self.cfg.append_date = self.append_date;
        self.cfg.never_dates = self.never_dates;
        save_config(&self.cfg);
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
            Some(UpdateMsg::Available(tag, url)) => {
                self.update_info = Some((tag, url));
                self.update_dismissed = false;
                self.update_status.clear();
                self.update_pending = false;
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
            None => {}
        }
    }

    fn ui_settings(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        self.poll_updates(ctx);
        ui.heading("File Namer AI");
        ui.label("Right-click any file → \"Rename with File Namer AI\" and a local AI proposes a better name.");

        // update banner
        let upd = self.update_info.clone();
        if let Some((ver, url)) = upd {
            if !self.update_dismissed {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("Update available: {ver} (you have v{APP_VERSION})"))
                            .strong(),
                    );
                    if ui.button("Download").clicked() {
                        open_url(&url);
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
            ui.horizontal(|ui| {
                ui.label("Model:");
                ui.add(egui::TextEdit::singleline(&mut self.model_edit).desired_width(280.0));
                if ui.button("Test").clicked() {
                    self.save_settings_from_ui();
                    self.settings_status = if ollama_up(&self.cfg.ollama_url) {
                        format!("Connected — model \"{}\" is reachable.", self.cfg.model)
                    } else {
                        "Can't reach Ollama. Is it installed and running? (ollama serve)".into()
                    };
                }
            });
            ui.checkbox(&mut self.append_date, "Append date (_DD-Mon-YYYY) when the content has one");
            ui.checkbox(&mut self.never_dates, "Never add dates to document names");
            if ui.button("Save settings").clicked() {
                self.save_settings_from_ui();
                self.settings_status = "Settings saved.".into();
            }
            if !self.settings_status.is_empty() {
                ui.label(&self.settings_status);
            }
            ui.add_space(4.0);
            ui.label("First-time setup: install Ollama from ollama.com, then run:");
            ui.monospace("ollama pull qwen2.5:3b");
        });

        ui.add_space(8.0);
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
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("About").clicked() {
                    self.show_about = true;
                }
            });
        });
    }

    fn ui_rename(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.poll_worker(ctx);
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
                ui.monospace("ollama serve");
                ui.add_space(8.0);
                if ui.button("Close").clicked() {
                    std::process::exit(0);
                }
            }
            RenameState::Confirm => {
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

fn main() {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("File Namer AI")
        .with_inner_size([600.0, 560.0]);
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
        Box::new(|cc| Box::new(NamerApp::new(cc))),
    );
}

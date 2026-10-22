#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! File Namer AI - right-click AI file renaming powered by a local LLM (Ollama).

use eframe::egui;
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

struct Config {
    ollama_url: String,
    model: String,
    append_date: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ollama_url: DEFAULT_OLLAMA_URL.into(),
            model: DEFAULT_MODEL.into(),
            append_date: true,
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
        "ollama_url={}\nmodel={}\nappend_date={}\n",
        cfg.ollama_url.trim(),
        cfg.model.trim(),
        cfg.append_date
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
    /// Text for the AI to read.
    Text(String),
    /// No readable text (e.g. photo): date-based naming only.
    DateOnly(chrono::NaiveDate),
}

fn exif_date(path: &Path) -> Option<chrono::NaiveDate> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;
    let field = exif
        .get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
        .or_else(|| exif.get_field(exif::Tag::DateTime, exif::In::PRIMARY))?;
    let s = field.display_value().to_string(); // "2026:10:22 15:26:11"
    chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y:%m:%d %H:%M:%S")
        .ok()
        .map(|dt| dt.date())
}

fn modified_date(path: &Path) -> Option<chrono::NaiveDate> {
    let md = std::fs::metadata(path).ok()?;
    let st = md.modified().ok()?;
    let dt: chrono::DateTime<chrono::Local> = st.into();
    Some(dt.date_naive())
}

fn best_date(path: &Path) -> chrono::NaiveDate {
    exif_date(path)
        .or_else(|| modified_date(path))
        .unwrap_or_else(|| chrono::Local::now().date_naive())
}

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
            Ok(Extracted::DateOnly(best_date(path)))
        }
        _ => Err(format!(
            "Unsupported file type: .{ext} (PDF, text, Word-style and image files work)"
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

fn build_prompt(original: &str, content: &str, append_date: bool) -> String {
    let date_rule = if append_date {
        "- If the content contains an obvious document date (invoice date, letter date, statement date), append it as _DD-Mon-YYYY, e.g. Report_Q3_19-Oct-2026. If no date is in the content, do NOT add one.\n"
    } else {
        ""
    };
    format!(
        "Propose a concise descriptive filename for the file described below.\n\
         \n\
         Rules (follow exactly):\n\
         - Output ONLY the filename: no quotes, no explanation, no file extension.\n\
         - Use underscores between words. No spaces.\n\
         - Maximum 50 characters.\n\
         {date_rule}\
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
    // collapse whitespace runs into a single underscore
    let mut out = String::with_capacity(s.len());
    let mut prev_us = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_us && !out.is_empty() {
                out.push('_');
                prev_us = true;
            }
        } else {
            out.push(c);
            prev_us = false;
        }
    }
    s = out;
    s = s.trim_matches(|c| c == '.' || c == '_' || c == '-').to_string();
    s.chars().take(80).collect()
}

fn has_date_suffix(s: &str) -> bool {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let b = s.as_bytes();
    if b.len() < 11 {
        return false;
    }
    for i in 0..=b.len() - 11 {
        let w = &b[i..i + 11];
        if w[0].is_ascii_digit()
            && w[1].is_ascii_digit()
            && w[2] == b'-'
            && w[6] == b'-'
            && w[7].is_ascii_digit()
            && w[8].is_ascii_digit()
            && w[9].is_ascii_digit()
            && w[10].is_ascii_digit()
        {
            if let Ok(mon) = std::str::from_utf8(&w[3..6]) {
                if MONTHS.contains(&mon.to_lowercase().as_str()) {
                    return true;
                }
            }
        }
    }
    false
}

fn ask_ollama(cfg: &Config, path: &Path, content: &str) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|e| format!("Couldn't start HTTP client: {e}"))?;

    let original = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let body = serde_json::json!({
        "model": cfg.model,
        "messages": [
            {"role": "system", "content": "You are a file renaming assistant. You output only the proposed filename, nothing else."},
            {"role": "user", "content": build_prompt(original, content, cfg.append_date)}
        ],
        "stream": false,
        "options": {"temperature": 0.2}
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
    let mut name = sanitize_filename(text);
    if name.is_empty() {
        return Err("The model returned an empty name.".into());
    }
    if cfg.append_date && !has_date_suffix(&name) {
        if let Some(d) = modified_date(path) {
            name.push('_');
            name.push_str(&d.format("%d-%b-%Y").to_string());
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
// app
// ---------------------------------------------------------------------------

enum WorkerMsg {
    Suggestion(String),
    DateOnly(String),
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
    settings_status: String,
    menu_on: bool,
    // rename flow
    rx: Option<mpsc::Receiver<WorkerMsg>>,
    state: RenameState,
    suggestion: String,
    show_about: bool,
}

fn rename_worker(path: PathBuf, cfg: Config, tx: mpsc::Sender<WorkerMsg>) {
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
        Extracted::DateOnly(d) => {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("IMG");
            let clean: String = stem
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                .take(30)
                .collect();
            let prefix = if clean.is_empty() { "IMG".into() } else { clean };
            send(WorkerMsg::DateOnly(format!(
                "{}_{}",
                prefix,
                d.format("%d-%b-%Y")
            )));
        }
        Extracted::Text(content) => {
            if !ollama_up(&cfg.ollama_url) {
                send(WorkerMsg::NeedOllama);
                return;
            }
            match ask_ollama(&cfg, &path, &content) {
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
            cfg,
            settings_status: String::new(),
            menu_on: menu_installed(),
            rx: None,
            state: RenameState::Working,
            suggestion: String::new(),
            show_about: false,
        };

        if app.is_rename_mode {
            let (tx, rx) = mpsc::channel();
            app.rx = Some(rx);
            let path = app.file_path.clone().unwrap();
            let cfg = Config {
                ollama_url: app.cfg.ollama_url.clone(),
                model: app.cfg.model.clone(),
                append_date: app.cfg.append_date,
            };
            thread::spawn(move || rename_worker(path, cfg, tx));
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
                self.suggestion = name;
                self.state = RenameState::Confirm;
            }
            Some(WorkerMsg::DateOnly(name)) => {
                self.suggestion = name;
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
        let new_name = if self.file_ext.is_empty() {
            stem.clone()
        } else {
            format!("{stem}.{}", self.file_ext)
        };
        let new_path = path.with_file_name(&new_name);
        if new_path.exists() {
            self.state = RenameState::Error(format!(
                "A file named \"{new_name}\" already exists here."
            ));
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
        save_config(&self.cfg);
    }

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("File Namer AI");
        ui.label("Right-click any file → \"Rename with File Namer AI\" and a local AI proposes a better name.");
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
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Rename").clicked() {
                        self.do_rename();
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
                    self.ui_settings(ui);
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
                        "File Namer AI v1.0\n\n\
                         Right-click any file and a local AI (Ollama)\n\
                         proposes a better name based on its contents,\n\
                         with a date when the content has one.\n\n\
                         100% local — no cloud, no API keys.\n\n\
                         Built for Shawn Rodis.",
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

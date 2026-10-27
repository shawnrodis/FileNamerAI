// File Namer AI — Rust/Flutter version (v1.4.0). Flutter UI, Rust core
// (rust/src/lib.rs, extracted from the egui app, branch egui-legacy).
// Naming, extraction, and Ollama behaviour come from Rust via FFI;
// Dart code in this file is UI only and must not reimplement naming.
//
// Core parity with the egui app, in one file on purpose: the GitHub Actions
// workflow generates the Windows runner with `flutter create` and copies
// this file over the generated lib/main.dart, so everything must live here.
//
// Rules implemented (from the egui app):
//  - Photos/videos: deterministic, NO AI, Bulk Rename Utility-style
//    MM-DD-YY-HH-MM-SS. Earliest timestamp wins across EXIF DateTimeOriginal
//    and file Modified (see note on Created below). Videos always truncate
//    seconds to :00. Photos WITH a real DateTimeOriginal keep full seconds;
//    photos WITHOUT it (screenshots, Messenger downloads) truncate to :00.
//    _1/_2 suffixes on real collisions only; a trailing _N is ignored when
//    detecting an already-correct name.
//  - Documents: Ollama /api/chat (temp 0.2, re-roll 0.7). Prompt asks for
//    subject/sender + document type, NEVER field labels as titles. Date
//    format DD-Mon-YYYY, underscore only before the date, Title Case.
//    Guards: a date must appear in the document content or it is stripped;
//    title words must appear in the content or the model re-rolls. Dates and
//    titles are never invented.
//  - Already-correctly-named files are left alone.
//
// KNOWN GAPS (honest, not faked): Mac DMG. Automatic update
// install is implemented (verified download + delayed installer
// launch, same pattern as Utility Closet). AI OCR for image-only PDFs works via the Rust core
// (Ollama vision model), with a Force AI OCR toggle in Settings.
// Windows right-click menu
// install/remove is implemented via per-user HKCU reg commands. File "Created" time is NOT exposed by Dart's FileStat
// on all platforms, so the earliest-timestamp rule uses EXIF DateTimeOriginal
// + file Modified here; on Windows that is usually the closest available
// equivalent. EXIF parsing here covers JPEG (the common case); PNG/HEIC
// files fall back to Modified and therefore truncate seconds, per the rule
// for photos without a real DateTimeOriginal.

import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:desktop_drop/desktop_drop.dart';
import 'package:file_picker/file_picker.dart';
import 'package:flutter/material.dart';
import 'package:http/http.dart' as http;
import 'package:path/path.dart' as p;
import 'package:path_provider/path_provider.dart';

import 'rust_bridge.dart';

// ---------------------------------------------------------------- settings

const String appVersion = '1.4.3';

class Settings {
  String ollamaUrl;
  String model;
  String visionModel;
  String dateOrder; // 'MM-DD-YY', 'DD-MM-YY', 'YY-MM-DD'
  String separator;
  bool timeOn;
  String secondsRule; // 'auto', 'keep', 'truncate'
  String timestampSource; // 'earliest', 'latest', 'exif-only'
  String collisionStyle; // '_1', '-1', '(1)', or '' (none)
  String prefix;
  String suffix;
  // Document-naming options from the egui app (Rust core reads all
  // three via parse_config_dart: forceOcr, appendDate, neverDates).
  // forceOcr: use Ollama vision OCR even when a PDF has a text layer.
  // appendDate: append _DD-Mon-YYYY to document names (default true).
  // neverDates: never add dates to document names (overrides appendDate
  // in Rust: append_date && !never_dates, and a never-dated name keeps
  // its stem). These were previously lost on update because Flutter
  // Settings did not expose them, even though Rust supported them.
  bool forceOcr;
  bool appendDate;
  bool neverDates;

  Settings({
    this.ollamaUrl = 'http://localhost:11434',
    this.model = 'qwen2.5:3b',
    this.visionModel = 'moondream',
    this.dateOrder = 'MM-DD-YY',
    this.separator = '-',
    this.timeOn = true,
    this.secondsRule = 'auto',
    this.timestampSource = 'earliest',
    this.collisionStyle = '_1',
    this.prefix = '',
    this.suffix = '',
    this.forceOcr = false,
    this.appendDate = true,
    this.neverDates = false,
  });

  Map<String, dynamic> toJson() => {
        'ollamaUrl': ollamaUrl,
        'model': model,
        'visionModel': visionModel,
        'dateOrder': dateOrder,
        'separator': separator,
        'timeOn': timeOn,
        'secondsRule': secondsRule,
        'timestampSource': timestampSource,
        'collisionSuffix': collisionStyle,
        'prefix': prefix,
        'suffix': suffix,
        'forceOcr': forceOcr,
        'appendDate': appendDate,
        'neverDates': neverDates,
      };

  static Settings fromJson(Map<String, dynamic> j) => Settings(
        ollamaUrl: j['ollamaUrl'] as String? ?? 'http://localhost:11434',
        model: j['model'] as String? ?? 'qwen2.5:3b',
        visionModel: j['visionModel'] as String? ?? 'moondream',
        dateOrder: j['dateOrder'] as String? ?? 'MM-DD-YY',
        separator: j['separator'] as String? ?? '-',
        timeOn: j['timeOn'] as bool? ?? true,
        secondsRule: j['secondsRule'] as String? ?? 'auto',
        timestampSource: j['timestampSource'] as String? ?? 'earliest',
        collisionStyle: j['collisionSuffix'] is String
            ? (j['collisionSuffix'] as String)
            : (j['collisionSuffix'] as bool? ?? true ? '_1' : ''),
        prefix: j['prefix'] as String? ?? '',
        suffix: j['suffix'] as String? ?? '',
        forceOcr: j['forceOcr'] as bool? ?? false,
        appendDate: j['appendDate'] as bool? ?? true,
        neverDates: j['neverDates'] as bool? ?? false,
      );
}

Future<File> _settingsFile() async {
  final dir = await getApplicationSupportDirectory();
  await dir.create(recursive: true);
  return File(p.join(dir.path, 'filenamer_ai_flutter_settings.json'));
}

// One-time migration from the old egui settings file
// (FileNamerAI.ini, plain key=value lines, egui load_config).
// Runs ONLY if the new settings file does not already exist, and the
// migrated settings are saved immediately, so migration runs exactly
// once and never overwrites a user's new settings. A missing or
// corrupt old file is silently ignored (defaults, no crash).
// Old locations (egui config_path): Windows = directory of the exe +
// "FileNamerAI.ini"; macOS = ~/Library/Application Support/
// FileNamerAI/FileNamerAI.ini. Both candidates are tried, in that
// order, so a Windows install moved to Mac (or vice versa) still
// migrates if the old file is present.
// Known limitation: egui collision suffixes "-1" and "(1)" collapse
// to "_1" in Flutter, which only supports on/off with "_1".
Future<Settings?> _migrateEguiSettings() async {
  try {
    final candidates = <File>[];
    try {
      candidates.add(File(p.join(
          p.dirname(Platform.resolvedExecutable), 'FileNamerAI.ini')));
    } catch (_) {}
    try {
      final home = Platform.environment['HOME'] ??
          Platform.environment['USERPROFILE'] ??
          '';
      if (home.isNotEmpty) {
        candidates.add(File(p.join(home, 'Library', 'Application Support',
            'FileNamerAI', 'FileNamerAI.ini')));
      }
    } catch (_) {}
    File? old;
    for (final c in candidates) {
      try {
        if (await c.exists()) {
          old = c;
          break;
        }
      } catch (_) {}
    }
    if (old == null) return null;
    final kv = <String, String>{};
    for (final line in await old.readAsLines()) {
      final eq = line.indexOf('=');
      if (eq < 0) continue;
      kv[line.substring(0, eq).trim()] = line.substring(eq + 1).trim();
    }
    if (kv.isEmpty) return null;
    String nonEmpty(String key, String fallback) {
      final v = kv[key];
      return (v == null || v.isEmpty) ? fallback : v;
    }

    final s = Settings();
    s.ollamaUrl = nonEmpty('ollama_url', 'http://localhost:11434');
    s.model = nonEmpty('model', 'qwen2.5:3b');
    s.visionModel = nonEmpty('vision_model', 'moondream');
    s.appendDate = kv['append_date'] != 'false';
    s.neverDates = kv['never_dates'] == 'true';
    s.forceOcr = kv['force_ocr'] == 'true';
    // egui internal values, validated exactly as egui did
    // (PhotoSettings::validated + load_config defaults).
    switch (kv['photo_date_order']) {
      case 'dd-mm-yy':
        s.dateOrder = 'DD-MM-YY';
        break;
      case 'yyyy-mm-dd':
        s.dateOrder = 'YY-MM-DD';
        break;
      case 'mm-dd-yy':
      default:
        s.dateOrder = 'MM-DD-YY';
    }
    final sep = kv['photo_separator'];
    s.separator = (sep == '-' || sep == '_') ? sep! : '-';
    s.timeOn = kv['photo_include_time'] != 'false';
    switch (kv['photo_seconds']) {
      case 'never':
        s.secondsRule = 'keep';
        break;
      case 'always':
        s.secondsRule = 'truncate';
        break;
      case 'current':
      default:
        s.secondsRule = 'auto';
    }
    switch (kv['photo_source']) {
      case 'latest':
        s.timestampSource = 'latest';
        break;
      case 'exif_only':
        s.timestampSource = 'exif-only';
        break;
      case 'earliest':
      default:
        s.timestampSource = 'earliest';
    }
    // Flutter only supports on/off with "_1": any non-empty egui
    // suffix ("_1", "-1", "(1)") maps to on; empty maps to off.
    final coll = kv['photo_collision_suffix'];
    s.collisionStyle = coll != null && ['_1', '-1', '(1)'].contains(coll) ? coll : '_1';
    s.prefix = kv['photo_prefix'] ?? '';
    s.suffix = kv['photo_suffix_text'] ?? '';
    return s;
  } catch (_) {
    return null;
  }
}

Future<Settings> loadSettings() async {
  try {
    final f = await _settingsFile();
    if (await f.exists()) {
      return Settings.fromJson(jsonDecode(await f.readAsString()));
    }
    // First launch only: import egui settings, then save them as the
    // new settings file so this never runs again.
    final migrated = await _migrateEguiSettings();
    if (migrated != null) {
      await saveSettings(migrated);
      return migrated;
    }
  } catch (_) {}
  return Settings();
}

Future<void> saveSettings(Settings s) async {
  try {
    final f = await _settingsFile();
    await f.writeAsString(jsonEncode(s.toJson()));
  } catch (_) {}
}

// ------------------------------------------------------------ date helpers

const _months = [
  'Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun',
  'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'
];
const _monthNames = [
  'January', 'February', 'March', 'April', 'May', 'June',
  'July', 'August', 'September', 'October', 'November', 'December'
];

String _two(int n) => n.toString().padLeft(2, '0');

String formatPhotoName(DateTime dt, Settings s, bool hasRealExif) {
  final yy = _two(dt.year % 100);
  final mm = _two(dt.month);
  final dd = _two(dt.day);
  final sep = s.separator.isEmpty ? '-' : s.separator;
  String date;
  switch (s.dateOrder) {
    case 'DD-MM-YY':
      date = '$dd$sep$mm$sep$yy';
      break;
    case 'YY-MM-DD':
      date = '$yy$sep$mm$sep$dd';
      break;
    default:
      date = '$mm$sep$dd$sep$yy';
  }
  if (!s.timeOn) return date;
  bool keepSeconds;
  if (s.secondsRule == 'keep') {
    keepSeconds = true;
  } else if (s.secondsRule == 'truncate') {
    keepSeconds = false;
  } else {
    keepSeconds = hasRealExif;
  }
  final time = keepSeconds
      ? '${_two(dt.hour)}$sep${_two(dt.minute)}$sep${_two(dt.second)}'
      : '${_two(dt.hour)}$sep${_two(dt.minute)}${sep}00';
  // For the default '-' separator and the classic BRU form, the time part
  // above already reads MM-DD-YY-HH-MM-SS. For a custom separator the same
  // separator is used throughout, which is what the Settings preview shows.
  return '$date$sep$time';
}

String formatDocDate(DateTime dt) =>
    '${_two(dt.day)}-${_months[dt.month - 1]}-${dt.year}';


// ------------------------------------------------------------- Rust core
// Naming, extraction, and Ollama logic are NOT implemented in Dart.
// They live in rust/src/lib.rs (extracted from the egui app) and are
// called through lib/rust_bridge.dart via FFI. The Dart formatter
// above (formatPhotoName / formatDocDate) is used only for the
// Settings live preview and is never used for actual naming.

// --------------------------------------------------------------- file items

enum FileKind { photo, video, document, other }

FileKind kindForExt(String ext) {
  const photos = {'jpg', 'jpeg', 'png', 'heic', 'heif', 'webp', 'tif', 'tiff', 'bmp'};
  const videos = {'mp4', 'mov', 'm4v', 'avi', 'mkv', 'wmv'};
  const docs = {'pdf', 'docx', 'odt', 'xlsx', 'pptx', 'txt', 'md', 'csv'};
  if (photos.contains(ext)) return FileKind.photo;
  if (videos.contains(ext)) return FileKind.video;
  if (docs.contains(ext)) return FileKind.document;
  return FileKind.other;
}

class FileItem {
  final File file;
  String proposed = '';
  String finalName = '';
  String status = 'Pending';
  String source = '';
  bool alreadyCorrect = false;
  bool proposeError = false;
  final controllers = <String, TextEditingController>{};

  FileItem(this.file);

  String get ext => p.extension(file.path).replaceFirst('.', '').toLowerCase();
  String get baseName => p.basename(file.path);
  String get stem => p.basenameWithoutExtension(file.path);
}

String stripTrailingSuffix(String stem) {
  final m = RegExp(r'^(.*)_\d+$').firstMatch(stem);
  return m != null ? m.group(1)! : stem;
}

// ---------------------------------------------------------------------- UI

// egui coordinate_files(), ported to Dart: when Windows invokes the
// app once per selected file (despite MultiSelectModel Player), the
// first process becomes coordinator, waits 900ms collecting drop files
// from the stragglers, and opens ONE window with all of them.
// A straggler writes its file path to %TEMP%\\FileNamerAI-batch and
// exits quietly, before any window is created.
Future<List<String>> _coordinateFiles(List<String> files) async {
  if (!Platform.isWindows || files.length != 1) return files;
  final dir = Directory(p.join(Directory.systemTemp.path, 'FileNamerAI-batch'));
  try {
    dir.createSync(recursive: true);
  } catch (_) {
    return files;
  }
  final lock = File(p.join(dir.path, 'coordinator.lock'));
  bool fresh(File f) {
    try {
      return DateTime.now().difference(f.statSync().modified) <
          const Duration(seconds: 10);
    } catch (_) {
      return false;
    }
  }
  bool isCoordinator = false;
  try {
    lock.createSync(exclusive: true);
    isCoordinator = true;
  } catch (_) {
    if (fresh(lock)) {
      try {
        final tag = p.basename(files[0]).split('').map((c) {
          return RegExp(r'[A-Za-z0-9]').hasMatch(c) ? c : '_';
        }).join();
        File(p.join(dir.path, '${pid}_$tag.txt'))
            .writeAsStringSync(files[0]);
      } catch (_) {}
      exit(0);
    }
    try {
      lock.deleteSync();
      lock.createSync(exclusive: true);
      isCoordinator = true;
    } catch (_) {
      return files;
    }
  }
  if (!isCoordinator) return files;
  try {
    for (final e in dir.listSync()) {
      if (e.path != lock.path &&
          e.path.endsWith('.txt') &&
          !fresh(File(e.path))) {
        try {
          e.deleteSync();
        } catch (_) {}
      }
    }
  } catch (_) {}
  final all = List<String>.from(files);
  final sw = Stopwatch()..start();
  while (sw.elapsed < const Duration(milliseconds: 900)) {
    try {
      for (final e in dir.listSync()) {
        if (e.path == lock.path || !e.path.endsWith('.txt')) continue;
        final f = File(e.path);
        if (!fresh(f)) continue;
        try {
          final content = f.readAsStringSync().trim();
          if (content.isNotEmpty &&
              File(content).existsSync() &&
              !all.contains(content)) {
            all.add(content);
          }
        } catch (_) {}
        try {
          f.deleteSync();
        } catch (_) {}
      }
    } catch (_) {}
    await Future.delayed(const Duration(milliseconds: 60));
  }
  try {
    lock.deleteSync();
  } catch (_) {}
  return all;
}

Future<void> main(List<String> args) async {
  // Right-click (and any shell launch) passes file paths as arguments.
  // Flutter desktop delivers them via main's args; fall back to
  // Platform.executableArguments as well, in case the engine route
  // differs. Flags are dropped, files are filtered here and in _addPaths.
  final initial = <String>{};
  for (final a in [...args, ...Platform.executableArguments]) {
    if (!a.startsWith('-')) initial.add(a);
  }
  var files =
      initial.where((f) => File(f).existsSync()).toList();
  if (Platform.isWindows) {
    // egui main() did this on every launch: repair a wrong-model verb
    // (v1.3.0 set the wrong model) so the NEXT multi-select is Player.
    RustBridge.ensureMultiselect();
    files = await _coordinateFiles(files);
  }
  runApp(FileNamerApp(initialPaths: files));
}

class FileNamerApp extends StatelessWidget {
  final List<String> initialPaths;
  const FileNamerApp({super.key, this.initialPaths = const []});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'File Namer AI',
      theme: ThemeData(
        colorScheme: ColorScheme.fromSeed(seedColor: Colors.teal),
        useMaterial3: true,
      ),
      darkTheme: ThemeData(
        colorScheme: ColorScheme.fromSeed(
            seedColor: Colors.teal, brightness: Brightness.dark),
        useMaterial3: true,
      ),
      themeMode: ThemeMode.system,
      home: HomePage(initialPaths: initialPaths),
    );
  }
}

class HomePage extends StatefulWidget {
  final List<String> initialPaths;
  const HomePage({super.key, this.initialPaths = const []});

  @override
  State<HomePage> createState() => _HomePageState();
}

class _HomePageState extends State<HomePage> {
  Settings settings = Settings();
  final List<FileItem> items = [];
  bool processing = false;
  String log = 'Add files, then Propose Names.';
  bool _settingsLoaded = false;
  int _settingsVersion = 0;
  String ollamaStatus = 'Not checked';
  List<String> ollamaModels = [];
  String ollamaSetupLog = '';
  bool ollamaBusy = false;
  String settingsStatus = '';
  String updateStatus = 'Not checked';
  bool updateChecking = false;
  String? updateTag;
  String updateAssetUrl = '';
  String updateAssetName = '';
  int updateAssetSize = 0;
  bool updateDownloading = false;
  double updateProgress = 0;
  String menuStatus = 'Not checked';
  bool menuInstalled = false;
  int lastRenamed = 0;
  int lastFailed = 0;
  bool renameDone = false;

  @override
  void initState() {
    super.initState();
    loadSettings().then((s) async {
      if (!mounted) return;
      setState(() {
        settings = s;
        _settingsLoaded = true;
      });
      if (widget.initialPaths.isNotEmpty) {
        final added = await _addPaths(widget.initialPaths);
        await _proposeItems(added);
      }
    });
    if (Platform.isWindows) _checkMenu();
    _checkUpdates();
  }

  Future<void> _save() => saveSettings(settings);

  Future<void> _resetSettings() async {
    setState(() {
      settings = Settings();
      _settingsVersion++;
    });
    await _save();
    _setLog('Settings reset to defaults.');
  }

  Future<void> _checkOllama() async {
    setState(() {
      ollamaBusy = true;
      ollamaStatus = 'Checking…';
    });
    try {
      final models = await RustBridge.checkModels(settings.ollamaUrl);
      setState(() {
        ollamaModels = models;
        ollamaStatus = 'Ollama running — ${models.length} model(s) installed';
        ollamaSetupLog = models.isEmpty
            ? 'No models installed yet.'
            : 'Installed: ${models.join(', ')}';
      });
    } catch (_) {
      setState(() {
        ollamaStatus = 'Ollama not running / not installed';
        ollamaModels = [];
      });
    }
    setState(() => ollamaBusy = false);
  }

  Future<void> _openOllamaSite() async {
    try {
      if (Platform.isWindows) {
        await Process.start('cmd', ['/c', 'start', '', 'https://ollama.com']);
      } else if (Platform.isMacOS) {
        await Process.start('open', ['https://ollama.com']);
      } else {
        await Process.start('xdg-open', ['https://ollama.com']);
      }
    } catch (_) {}
  }

  Future<void> _downloadOllama() async {
    if (!Platform.isWindows) {
      await _openOllamaSite();
      return;
    }
    setState(() {
      ollamaBusy = true;
      ollamaSetupLog = 'Downloading Ollama installer…';
    });
    try {
      final path = await RustBridge.downloadOllamaInstaller();
      await Process.start(path, []);
      setState(() => ollamaSetupLog =
          'Ollama installer launched — complete its steps, then come back and Check again.');
    } catch (e) {
      setState(() => ollamaSetupLog = 'Download failed: $e');
    }
    setState(() => ollamaBusy = false);
  }

  Future<void> _pullModel(String model) async {
    setState(() {
      ollamaBusy = true;
      ollamaSetupLog = 'Pulling $model…';
    });
    try {
      await RustBridge.pullModel(settings.ollamaUrl, model);
      setState(() => ollamaSetupLog = 'Pulled $model.');
      await _checkOllama();
    } catch (e) {
      setState(() => ollamaSetupLog = 'Pull failed: $e');
    }
    setState(() => ollamaBusy = false);
  }

  Future<void> _downloadMissingModels() async {
    await _checkOllama();
    for (final m in [settings.model, settings.visionModel]) {
      if (!ollamaModels.any((n) => n == m || n.startsWith('$m:'))) {
        await _pullModel(m);
      }
    }
    await _checkOllama();
  }

  void _setLog(String s) {
    if (mounted) setState(() => log = s);
  }

  Future<List<FileItem>> _addPaths(List<String> paths) async {
    final added = <FileItem>[];
    for (final path in paths) {
      final f = File(path);
      if (!await f.exists()) continue;
      if (items.any((it) => it.file.path == path)) continue;
      final item = FileItem(f);
      items.add(item);
      added.add(item);
    }
    setState(() {});
    _setLog(added.isNotEmpty ? 'Added ${added.length} file(s).' : 'No new files added.');
    return added;
  }

  Future<void> _pickFiles() async {
    final result = await FilePicker.platform.pickFiles(allowMultiple: true);
    if (result == null) return;
    final added = await _addPaths(result.paths.whereType<String>().toList());
    await _proposeItems(added);
  }

  Future<void> _proposeOne(FileItem item, {bool reroll = false}) async {
    // Naming, extraction, and Ollama behaviour come from the Rust core
    // (rust/src/lib.rs, extracted from the egui app) via FFI — not
    // from Dart logic. That is what makes file Created time, PDF/Office
    // extraction, and AI OCR for image-only PDFs work exactly like egui.
    try {
      item.status = 'Proposing…';
      setState(() {});
      final configJson = jsonEncode(settings.toJson());
      final result = await RustBridge.propose(
          item.file.path, configJson, reroll ? 1 : 0);
      final kind = result['kind'] as String? ?? 'error';
      if (kind == 'date') {
        item.proposed = result['proposed'] as String? ?? '';
        item.source = result['date_source'] as String? ?? '';
        item.status = 'Date-based (${item.source})';
        item.proposeError = false;
      } else if (kind == 'document') {
        item.proposed = result['proposed'] as String? ?? '';
        item.status = reroll ? 'Re-rolled' : 'AI named';
        item.source = item.status;
        item.proposeError = false;
      } else {
        item.proposed = item.stem;
        item.status = (result['error'] as String?) ?? 'Naming failed';
        item.source = '';
        item.proposeError = true;
      }
    } catch (e) {
      item.proposed = item.stem;
      item.status = 'Error: $e';
      item.source = '';
      item.proposeError = true;
    }
    item.finalName = item.proposed +
        (item.ext.isEmpty ? '' : '.${item.ext}');
  }

  Future<void> _proposeItems(List<FileItem> toPropose) async {
    if (toPropose.isEmpty) return;
    setState(() {
      processing = true;
      renameDone = false;
    });
    for (final item in toPropose) {
      await _proposeOne(item);
    }
    await _resolveBatch();
    setState(() => processing = false);
    // egui batch footer counts: to rename / already named / errors.
    // A batch where every file is already correct must say so plainly
    // instead of inviting a Rename All that would rename nothing.
    final toRename = items
        .where((i) =>
            !i.alreadyCorrect &&
            i.finalName.isNotEmpty &&
            i.finalName != i.baseName)
        .length;
    final alreadyNamed = items.where((i) => i.alreadyCorrect).length;
    final errors = items.where((i) => i.proposeError).length;
    if (toRename == 0 && alreadyNamed > 0 && errors == 0) {
      _setLog(
          'No renames needed — all $alreadyNamed file(s) already correctly named.');
    } else {
      _setLog(
          '$toRename to rename • $alreadyNamed already named • $errors errors. Edit Final Name, then Rename All.');
    }
  }

  Future<void> _proposeAll() async {
    if (items.isEmpty) {
      _setLog('No files to name.');
      return;
    }
    await _proposeItems(List<FileItem>.from(items));
  }

  Future<void> _resolveBatch() async {
    // Already-correct detection + collisions, via the Rust core
    // (egui resolve_batch): date rows use the advanced collision style,
    // document rows keep "_1", already-correct names are left alone.
    try {
      final rows = items.map((item) {
        final kind = kindForExt(item.ext);
        final isDate = kind == FileKind.photo || kind == FileKind.video;
        return {
          'path': item.file.path,
          'current_name': item.baseName,
          'ext': item.ext,
          'proposed': item.proposed,
          'ready': !item.proposeError && item.proposed.isNotEmpty,
          'error': item.proposeError,
          'date_source': isDate ? item.source : null,
        };
      }).toList();
      final resolved = RustBridge.resolveBatch(jsonEncode({
        'photo_collision_suffix': settings.collisionStyle,
        'rows': rows,
      }));
      final results = (resolved['results'] as List?) ?? [];
      for (var i = 0; i < items.length && i < results.length; i++) {
        final item = items[i];
        final r = results[i] as Map<String, dynamic>;
        final status = r['status'] as String? ?? 'ready';
        if (status == 'noop') {
          item.alreadyCorrect = true;
          item.finalName = item.baseName;
          item.status = 'Already correct — left alone';
        } else if (status == 'ready') {
          item.alreadyCorrect = false;
          item.finalName = r['final_name'] as String? ?? item.finalName;
        } else if (status == 'error') {
          item.alreadyCorrect = false;
          item.finalName = item.baseName;
        }
      }
    } catch (e) {
      _setLog('Collision resolution failed ($e) — final names are the proposed names.');
    }
  }

  Future<void> _renameSingle(FileItem item) async {
    if (item.alreadyCorrect) {
      _setLog('Already correct — left alone.');
      return;
    }
    if (item.finalName.isEmpty || item.finalName == item.baseName) {
      _setLog('Nothing to rename.');
      return;
    }
    setState(() => processing = true);
    try {
      final target = p.join(p.dirname(item.file.path), item.finalName);
      await item.file.rename(target);
      item.status = 'Renamed to ${item.finalName}';
      setState(() {
        lastRenamed = 1;
        lastFailed = 0;
        renameDone = true;
      });
      _setLog('Done: 1 renamed • 0 failed.');
    } catch (e) {
      item.status = 'Rename failed: $e';
      setState(() {
        lastRenamed = 0;
        lastFailed = 1;
        renameDone = true;
      });
      _setLog('Done: 0 renamed • 1 failed.');
    }
    setState(() => processing = false);
  }

  Future<void> _renameAll() async {
    var renamed = 0;
    var failed = 0;
    var skipped = 0;
    setState(() {
      processing = true;
      renameDone = false;
    });
    for (final item in items) {
      if (item.alreadyCorrect) {
        skipped++;
        continue;
      }
      if (item.finalName.isEmpty || item.finalName == item.baseName) {
        skipped++;
        continue;
      }
      try {
        final target = p.join(p.dirname(item.file.path), item.finalName);
        await item.file.rename(target);
        item.status = 'Renamed';
        renamed++;
      } catch (e) {
        item.status = 'Rename failed: $e';
        failed++;
      }
    }
    setState(() {
      processing = false;
      lastRenamed = renamed;
      lastFailed = failed;
      renameDone = true;
    });
    _setLog('Done: $renamed renamed • $failed failed. (Skipped: $skipped.)');
  }

  Future<void> _checkUpdates() async {
    setState(() {
      updateChecking = true;
      updateStatus = 'Checking…';
      updateTag = null;
      updateAssetUrl = '';
      updateAssetName = '';
      updateAssetSize = 0;
    });
    try {
      final resp = await http.get(
        Uri.parse('https://api.github.com/repos/shawnrodis/FileNamerAI/releases/latest'),
        headers: {'User-Agent': 'FileNamerAI', 'Accept': 'application/vnd.github+json'},
      ).timeout(const Duration(seconds: 15));
      if (resp.statusCode != 200) {
        setState(() => updateStatus = 'Could not check for updates (HTTP ${resp.statusCode}).');
      } else {
        final body = jsonDecode(resp.body) as Map<String, dynamic>;
        final tag = ((body['tag_name'] as String?) ?? '').replaceAll(RegExp(r'^v'), '');
        final current = appVersion;
        if (tag.isEmpty) {
          setState(() => updateStatus = 'Could not check for updates (no releases found).');
        } else if (tag == current) {
          setState(() => updateStatus = 'Up to date (v$current).');
        } else {
          // Installer asset only, exact name — never a zip or a stray file.
          final expected = 'FileNamerAI-Setup-v$tag.exe';
          Map<String, dynamic>? asset;
          for (final a in (body['assets'] as List?) ?? []) {
            if (a is Map<String, dynamic> && a['name'] == expected) {
              asset = a;
              break;
            }
          }
          if (asset == null) {
            setState(() => updateStatus =
                'Update available: $tag (you have v$current) — installer not found on the releases page.');
          } else {
            // Extract before setState: a captured nullable local is
            // not promoted inside the setState closure.
            final assetUrl =
                (asset['browser_download_url'] as String?) ?? '';
            final assetSize = (asset['size'] as int?) ?? 0;
            setState(() {
              updateTag = tag;
              updateAssetUrl = assetUrl;
              updateAssetName = expected;
              updateAssetSize = assetSize;
              updateStatus = 'Update available: $tag (you have v$current).';
            });
          }
        }
      }
    } catch (e) {
      setState(() => updateStatus = 'Could not check for updates: $e');
    }
    setState(() => updateChecking = false);
  }

  Future<void> _downloadAndInstallUpdate() async {
    if (updateAssetUrl.isEmpty || updateAssetName.isEmpty) return;
    if (!Platform.isWindows) {
      setState(() => updateStatus =
          'Auto-install is Windows only — download it from the releases page.');
      return;
    }
    setState(() {
      updateDownloading = true;
      updateProgress = 0;
    });
    try {
      final req = await http.Client().send(http.Request('GET', Uri.parse(updateAssetUrl)));
      if (req.statusCode != 200) throw StateError('HTTP ${req.statusCode}');
      final total = req.contentLength ?? updateAssetSize;
      final chunks = <int>[];
      var got = 0;
      await for (final chunk in req.stream) {
        chunks.addAll(chunk);
        got += chunk.length;
        if (total > 0) setState(() => updateProgress = got / total);
      }
      // Verify before launching: a redirect/error page or truncated file
      // must never be run as an installer, and must never exit the app.
      if (chunks.isEmpty ||
          chunks.length < 2 ||
          chunks[0] != 0x4D ||
          chunks[1] != 0x5A ||
          (total > 0 && got != total)) {
        throw StateError('Download verification failed');
      }
      final dest =
          File('${Directory.systemTemp.path}\\${updateAssetName}');
      await dest.writeAsBytes(chunks);
      setState(() =>
          updateStatus = 'Downloaded ${updateAssetName} — launching installer…');
      // Do NOT run the installer directly: it would try to replace the
      // running app exe while it is still locked. Write a tiny .bat
      // that waits 2s (app fully closed by then), runs the installer,
      // then deletes itself. The .bat survives the app exit because
      // cmd owns it.
      final cleanPath = dest.path.replaceAll('"', '');
      final bat =
          File('${Directory.systemTemp.path}\\FileNamerAI-update.bat');
      await bat.writeAsString(
          '@echo off\r\ntimeout /t 2 /nobreak >nul\r\nstart "" "$cleanPath"\r\ndel "%~f0"\r\n');
      await Process.start('cmd', ['/C', bat.path]);
      exit(0);
    } catch (_) {
      setState(() {
        updateDownloading = false;
        updateStatus = 'Download failed — check your connection and try again.';
      });
    }
  }

  Future<void> _checkMenu() async {
    if (!Platform.isWindows) return;
    try {
      final installed = RustBridge.menuInstalled();
      setState(() {
        menuInstalled = installed;
        menuStatus = installed
            ? 'Right-click menu: installed.'
            : 'Right-click menu: not installed.';
      });
    } catch (_) {
      setState(() => menuStatus = 'Right-click menu: could not check.');
    }
  }

  Future<void> _addMenu() async {
    if (!Platform.isWindows) return;
    try {
      await RustBridge.installMenu(Platform.resolvedExecutable);
      setState(() => menuStatus = 'Right-click menu: installed.');
      await _checkMenu();
    } catch (e) {
      setState(() => menuStatus = 'Right-click menu: failed to install ($e).');
    }
  }

  Future<void> _removeMenu() async {
    if (!Platform.isWindows) return;
    try {
      await RustBridge.uninstallMenu();
      setState(() => menuStatus = 'Right-click menu: removed.');
      await _checkMenu();
    } catch (e) {
      setState(() => menuStatus = 'Right-click menu: failed to remove ($e).');
    }
  }

  @override
  Widget build(BuildContext context) {
    return DefaultTabController(
      length: 2,
      child: Scaffold(
        appBar: AppBar(
          title: const Text('File Namer AI'),
          bottom: const TabBar(tabs: [
            Tab(text: 'Rename'),
            Tab(text: 'Settings'),
          ]),
        ),
        body: TabBarView(children: [
          _buildRenameTab(),
          _buildSettingsTab(),
        ]),
      ),
    );
  }

  Widget _buildRenameTab() {
    // Single file: egui-style dialog layout. Batch (2+): table layout.
    final single = items.length == 1 ? items.first : null;
    return Column(children: [
      Padding(
        padding: const EdgeInsets.all(12),
        child: DropTarget(
          onDragDone: (detail) async {
            final added =
                await _addPaths(detail.files.map((f) => f.path).toList());
            await _proposeItems(added);
          },
          child: Container(
            height: 110,
            width: double.infinity,
            decoration: BoxDecoration(
              border: Border.all(color: Colors.teal, width: 2),
              borderRadius: BorderRadius.circular(12),
            ),
            child: const Center(
              child: Text(
                'Drop files here to rename them\nOne file opens the rename dialog right here. Several files open the batch window.',
                textAlign: TextAlign.center,
                style: TextStyle(fontSize: 16),
              ),
            ),
          ),
        ),
      ),
      Padding(
        padding: const EdgeInsets.symmetric(horizontal: 12),
        child: Wrap(spacing: 8, children: [
          ElevatedButton.icon(
            onPressed: processing ? null : _pickFiles,
            icon: const Icon(Icons.folder_open),
            label: const Text('Add Files'),
          ),
          if (single == null)
            ElevatedButton.icon(
              onPressed: processing ? null : _proposeAll,
              icon: const Icon(Icons.auto_awesome),
              label: const Text('Propose Names'),
            ),
          if (single == null)
            ElevatedButton.icon(
              onPressed: processing ||
                      (items.isNotEmpty &&
                          !items.any((i) =>
                              !i.alreadyCorrect &&
                              i.finalName.isNotEmpty &&
                              i.finalName != i.baseName))
                  ? null
                  : _renameAll,
              icon: const Icon(Icons.drive_file_rename_outline),
              label: const Text('Rename All'),
            ),
          TextButton(
            onPressed: processing
                ? null
                : () {
                    setState(() => items.clear());
                    _setLog('Cleared.');
                  },
            child: const Text('Clear'),
          ),
          if (processing)
            const Padding(
              padding: EdgeInsets.only(left: 8),
              child: SizedBox(
                  width: 20, height: 20, child: CircularProgressIndicator(strokeWidth: 2)),
            ),
        ]),
      ),
      const SizedBox(height: 8),
      Expanded(
        child: items.isEmpty
            ? const Center(child: Text('No files yet. Drop files or add them.'))
            : single != null
                ? _buildSingleFileView(single)
                : _buildBatchTable(),
      ),
      Container(
        width: double.infinity,
        padding: const EdgeInsets.all(12),
        color: Colors.teal.withOpacity(0.1),
        child: Text(processing
            ? 'Working on… $log'
            : renameDone
                ? 'Done: $lastRenamed renamed • $lastFailed failed.'
                : log),
      ),
    ]);
  }

  Widget _buildSingleFileView(FileItem item) {
    final extLabel = item.ext.isEmpty ? '' : '.${item.ext}';
    return SingleChildScrollView(
      padding: const EdgeInsets.all(16),
      child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
        Text('File: ${item.baseName}',
            style: const TextStyle(fontWeight: FontWeight.bold)),
        const SizedBox(height: 12),
        if (item.alreadyCorrect)
          Text('✓ Already correctly named as ${item.finalName}',
              style: const TextStyle(color: Colors.green)),
        if (item.source.isNotEmpty && !item.alreadyCorrect)
          Text(kindForExt(item.ext) == FileKind.document
              ? 'Source: ${item.source}'
              : 'Date source: ${item.source}'),
        const SizedBox(height: 12),
        const Text('Proposed name (edit if you like):'),
        const SizedBox(height: 6),
        Row(children: [
          Expanded(
            child: TextFormField(
              key: ValueKey('single|${item.file.path}|${item.finalName}'),
              initialValue: item.finalName.isEmpty
                  ? ''
                  : (extLabel.isEmpty || !item.finalName.endsWith(extLabel)
                      ? item.finalName
                      : item.finalName.substring(
                          0, item.finalName.length - extLabel.length)),
              decoration: const InputDecoration(
                border: OutlineInputBorder(),
                isDense: true,
              ),
              onChanged: (v) => item.finalName = '$v$extLabel',
            ),
          ),
          const SizedBox(width: 6),
          Text(extLabel),
        ]),
        const SizedBox(height: 4),
        Text(item.proposed.isEmpty
            ? '— press Propose Names —'
            : 'Proposed: ${item.proposed}'),
        const SizedBox(height: 16),
        Wrap(spacing: 8, children: [
          ElevatedButton(
            onPressed: processing || item.alreadyCorrect
                ? null
                : () async {
                    if (item.proposed.isEmpty) {
                      await _proposeAll();
                    } else {
                      await _renameSingle(item);
                    }
                  },
            child: Text(item.proposed.isEmpty ? 'Propose Names' : 'Rename'),
          ),
          ElevatedButton(
            onPressed: processing
                ? null
                : () async {
                    setState(() {
                      processing = true;
                      renameDone = false;
                    });
                    await _proposeOne(item, reroll: true);
                    // Re-apply already-correct detection for the single file.
                    final stemOk = stripTrailingSuffix(item.stem) ==
                            stripTrailingSuffix(item.proposed) &&
                        item.stem == item.proposed ||
                        stripTrailingSuffix(item.stem) == item.proposed;
                    if (stemOk) {
                      item.alreadyCorrect = true;
                      item.finalName = item.baseName;
                      item.status = 'Already correct — left alone';
                    } else {
                      item.alreadyCorrect = false;
                    }
                    setState(() => processing = false);
                    _setLog('New name proposed.');
                  },
            child: const Text('New name'),
          ),
          TextButton(
            onPressed: processing
                ? null
                : () {
                    setState(() => items.clear());
                    _setLog('Cleared.');
                  },
            child: const Text('Cancel'),
          ),
        ]),
      ]),
    );
  }

  Widget _buildBatchTable() {
    // Table-like layout: File | Proposed name | Final name | Source.
    return SingleChildScrollView(
      scrollDirection: Axis.horizontal,
      child: SizedBox(
        width: 920,
        child: SingleChildScrollView(
          child: DataTable(
            columns: const [
              DataColumn(label: Text('File')),
              DataColumn(label: Text('Proposed name')),
              DataColumn(label: Text('Final name')),
              DataColumn(label: Text('Source')),
            ],
            rows: items.map((item) {
              return DataRow(cells: [
                DataCell(SizedBox(
                    width: 220,
                    child: Text(item.baseName, overflow: TextOverflow.ellipsis))),
                DataCell(SizedBox(
                    width: 220,
                    child: Text(
                        item.proposed.isEmpty ? '— press Propose Names —' : item.proposed,
                        overflow: TextOverflow.ellipsis))),
                DataCell(SizedBox(
                  width: 240,
                  child: item.alreadyCorrect
                      ? Text('✓ ${item.finalName}',
                          style: const TextStyle(color: Colors.green))
                      : TextFormField(
                          key: ValueKey('${item.file.path}|${item.finalName}'),
                          initialValue: item.finalName,
                          decoration: const InputDecoration(
                              border: OutlineInputBorder(), isDense: true),
                          onChanged: (v) => item.finalName = v,
                        ),
                )),
                DataCell(SizedBox(
                    width: 180,
                    child: Text(item.source.isNotEmpty ? item.source : item.status,
                        overflow: TextOverflow.ellipsis))),
              ]);
            }).toList(),
          ),
        ),
      ),
    );
  }

  Widget _buildSettingsTab() {
    final sample = formatPhotoName(
        DateTime(2026, 10, 26, 14, 30, 45), settings, true);
    final installed = (String m) =>
        ollamaModels.any((n) => n == m || n.startsWith('$m:'));
    return ListView(padding: const EdgeInsets.all(16), children: [
      // (a) Check for updates at the top, like egui.
      const Text('Check for updates',
          style: TextStyle(fontWeight: FontWeight.bold)),
      Text(updateStatus),
      ElevatedButton(
        onPressed: updateChecking ? null : _checkUpdates,
        child: const Text('Check for updates'),
      ),
      if (updateTag != null) ...[
        const SizedBox(height: 8),
        ElevatedButton(
          onPressed: updateDownloading ? null : _downloadAndInstallUpdate,
          child: const Text('Download & Install'),
        ),
        if (updateDownloading)
          Text('Downloading… ${(updateProgress * 100).toStringAsFixed(0)}%'),
      ],
      const SizedBox(height: 16),
      // (b) Local AI (Ollama).
      const Text('Local AI (Ollama)',
          style: TextStyle(fontWeight: FontWeight.bold)),
      const Text('Ollama (documents only — photos/videos never use AI)'),
      TextFormField(
        key: ValueKey('ollamaUrl_$_settingsVersion'),
        initialValue: settings.ollamaUrl,
        decoration: const InputDecoration(labelText: 'Ollama URL'),
        onChanged: (v) {
          settings.ollamaUrl = v;
          _save();
        },
      ),
      TextFormField(
        key: ValueKey('model_$_settingsVersion'),
        initialValue: settings.model,
        decoration: const InputDecoration(labelText: 'Model (naming documents)'),
        onChanged: (v) {
          settings.model = v;
          _save();
        },
      ),
      TextFormField(
        key: ValueKey('visionModel_$_settingsVersion'),
        initialValue: settings.visionModel,
        decoration: const InputDecoration(labelText: 'Vision model (PDF OCR)'),
        onChanged: (v) {
          settings.visionModel = v;
          _save();
        },
      ),
      CheckboxListTile(
        title: const Text('Force AI OCR for PDFs'),
        subtitle: const Text(
            'Use Ollama vision OCR even when a PDF already has a text layer. Off = OCR only for image-only PDFs.'),
        value: settings.forceOcr,
        onChanged: (v) {
          if (v != null) setState(() => settings.forceOcr = v);
          _save();
        },
      ),
      CheckboxListTile(
        title: const Text('Append date to document names'),
        subtitle: const Text(
            'Documents get _DD-Mon-YYYY, checked against the document itself.'),
        value: settings.appendDate,
        onChanged: (v) {
          if (v != null) setState(() => settings.appendDate = v);
          _save();
        },
      ),
      CheckboxListTile(
        title: const Text('Never add dates to document names'),
        subtitle: const Text(
            'Overrides "Append date" above: document names keep their stem, no date added.'),
        value: settings.neverDates,
        onChanged: (v) {
          if (v != null) setState(() => settings.neverDates = v);
          _save();
        },
      ),
      const SizedBox(height: 8),
      ElevatedButton(
        onPressed: () async {
          await _save();
          setState(() => settingsStatus = 'Settings saved.');
        },
        child: const Text('Save settings'),
      ),
      if (settingsStatus.isNotEmpty) Text(settingsStatus),
      const Text(
          'First-time setup: install Ollama from ollama.com, start it, then download the models in the Models section below — no terminal needed.'),
      const SizedBox(height: 16),
      // (c) Models.
      const Text('Models',
          style: TextStyle(fontWeight: FontWeight.bold)),
      const Text(
          'Edit a name to switch models, then Save settings. Download right here — no terminal needed.'),
      Text('Status: $ollamaStatus'),
      if (ollamaSetupLog.isNotEmpty) Text(ollamaSetupLog),
      if (ollamaModels.isEmpty)
        const Text('No installed models listed yet — press Refresh.')
      else
        ...ollamaModels.map((m) => Row(children: [
              Expanded(child: Text(m)),
              TextButton(
                onPressed: ollamaBusy ? null : () => _pullModel(m),
                child: const Text('Download'),
              ),
            ])),
      Wrap(spacing: 8, children: [
        ElevatedButton(
          onPressed: ollamaBusy ? null : _checkOllama,
          child: const Text('Refresh'),
        ),
        ElevatedButton(
          onPressed: ollamaBusy ? null : _downloadOllama,
          child: const Text('Download & Install Ollama'),
        ),
        ElevatedButton(
          onPressed: ollamaBusy ? null : _downloadMissingModels,
          child: const Text('Download missing models'),
        ),
      ]),
      const SizedBox(height: 8),
      // Per-model rows for the naming + vision models, like egui.
      for (final m in [settings.model, settings.visionModel])
        Row(children: [
          Expanded(
              child: Text(
                  '$m (${m == settings.model ? 'naming documents' : 'PDF OCR'})')),
          Text(installed(m) ? 'Installed' : 'Not installed'),
          const SizedBox(width: 8),
          TextButton(
            onPressed: ollamaBusy ? null : () => _pullModel(m),
            child: const Text('Download'),
          ),
        ]),
      if (!Platform.isWindows)
        const SizedBox(height: 4),
      const SizedBox(height: 16),
      // (d) Right-click menu.
      const Text('Right-click menu',
          style: TextStyle(fontWeight: FontWeight.bold)),
      if (Platform.isWindows) ...[
        Text(menuStatus),
        Wrap(spacing: 8, children: [
          ElevatedButton(
            onPressed: _addMenu,
            child: const Text('Add to right-click menu'),
          ),
          ElevatedButton(
            onPressed: _removeMenu,
            child: const Text('Remove from right-click menu'),
          ),
          TextButton(
            onPressed: _checkMenu,
            child: const Text('Check'),
          ),
        ]),
        const Text(
            'Note: on Windows 11 the item lives under "Show more options". You can also drag files onto this window to rename them.'),
      ] else
        const Text(
            'Right-click menu integration is Windows-only. On Mac, drag files onto this window to rename them — one file opens the rename dialog here, several open the batch window.'),
      const SizedBox(height: 16),
      // (e) Advanced photo/video naming, collapsed like egui.
      ExpansionTile(
        title: const Text('Advanced photo/video naming'),
        children: [
          const Text(
              'These change how photos and videos are named. Defaults reproduce the current logic — change nothing and nothing changes. Save settings (above) or Reset to defaults saves these too.'),
          DropdownButtonFormField<String>(
            value: settings.dateOrder,
            decoration: const InputDecoration(labelText: 'Date order'),
            items: const [
              DropdownMenuItem(value: 'MM-DD-YY', child: Text('MM-DD-YY')),
              DropdownMenuItem(value: 'DD-MM-YY', child: Text('DD-MM-YY')),
              DropdownMenuItem(value: 'YY-MM-DD', child: Text('YY-MM-DD')),
            ],
            onChanged: (v) {
              if (v != null) setState(() => settings.dateOrder = v);
              _save();
            },
          ),
          TextFormField(
            key: ValueKey('separator_$_settingsVersion'),
            initialValue: settings.separator,
            decoration: const InputDecoration(labelText: 'Separator'),
            onChanged: (v) {
              settings.separator = v;
              _save();
            },
          ),
          CheckboxListTile(
            title: const Text('Include time'),
            value: settings.timeOn,
            onChanged: (v) {
              if (v != null) setState(() => settings.timeOn = v);
              _save();
            },
          ),
          DropdownButtonFormField<String>(
            value: settings.secondsRule,
            decoration: const InputDecoration(labelText: 'Seconds rule'),
            items: const [
              DropdownMenuItem(
                  value: 'auto',
                  child: Text('Auto (EXIF keeps seconds, others :00)')),
              DropdownMenuItem(
                  value: 'keep', child: Text('Always keep seconds')),
              DropdownMenuItem(
                  value: 'truncate', child: Text('Always truncate to :00')),
            ],
            onChanged: (v) {
              if (v != null) setState(() => settings.secondsRule = v);
              _save();
            },
          ),
          DropdownButtonFormField<String>(
            value: settings.timestampSource,
            decoration: const InputDecoration(labelText: 'Timestamp source'),
            items: const [
              DropdownMenuItem(value: 'earliest', child: Text('Earliest')),
              DropdownMenuItem(value: 'latest', child: Text('Latest')),
              DropdownMenuItem(
                  value: 'exif-only',
                  child: Text('EXIF-only (fallback: file date)')),
            ],
            onChanged: (v) {
              if (v != null) setState(() => settings.timestampSource = v);
              _save();
            },
          ),
          DropdownButtonFormField<String>(
            value: settings.collisionStyle,
            decoration: const InputDecoration(labelText: 'Collision suffix style'),
            items: const [
              DropdownMenuItem(value: '_1', child: Text('_1')),
              DropdownMenuItem(value: '-1', child: Text('-1')),
              DropdownMenuItem(value: '(1)', child: Text('(1)')),
              DropdownMenuItem(value: '', child: Text('None')),
            ],
            onChanged: (v) {
              if (v != null) setState(() => settings.collisionStyle = v);
              _save();
            },
          ),
          TextFormField(
            key: ValueKey('prefix_$_settingsVersion'),
            initialValue: settings.prefix,
            decoration: const InputDecoration(labelText: 'Prefix'),
            onChanged: (v) {
              settings.prefix = v;
              _save();
            },
          ),
          TextFormField(
            key: ValueKey('suffix_$_settingsVersion'),
            initialValue: settings.suffix,
            decoration: const InputDecoration(labelText: 'Suffix'),
            onChanged: (v) {
              settings.suffix = v;
              _save();
            },
          ),
          const SizedBox(height: 12),
          Text(
              'Live preview (sample 26 Oct 2026, 14:30:45, with EXIF): $sample'),
          const SizedBox(height: 8),
          Text(
              'Document preview: Example Document_${formatDocDate(DateTime(2026, 10, 26))}'),
          const SizedBox(height: 12),
          ElevatedButton.icon(
            onPressed: _resetSettings,
            icon: const Icon(Icons.restore),
            label: const Text('Reset to defaults'),
          ),
          const Text(
              'Reset restores all settings above to their defaults and saves them.'),
          const Text(
              'Note: Save settings (in Local AI above) also saves these. The collision suffix only affects date names — AI document names still use _1.'),
        ],
      ),
      const SizedBox(height: 16),
      // (f) Photo/video dating explainer, egui wording.
      const Text('Photos and videos are named from dates — no AI involved.',
          style: TextStyle(fontWeight: FontWeight.bold)),
      const Text(
          '• By default, photos with a real Date Taken in EXIF keep full seconds (MM-DD-YY-HH-MM-SS).'),
      const Text(
          '• By default, photos without real Date Taken (screenshots, chat downloads) are truncated to the minute (:00 seconds).'),
      const Text(
          '• By default, videos use the embedded media-created date, always truncated to the minute (:00).'),
      const Text(
          '• By default, if the dates disagree, the earliest one wins (EXIF/media date, file Created, file Modified). Note: naming uses the Rust core, so file Created time works exactly like egui.'),
      const Text(
          '• Two files sharing a timestamp get a collision suffix (_1, _2 by default); already-correct names are left alone.'),
      const Text(
          '• The Advanced photo/video naming section above can change any of these defaults.'),
      const SizedBox(height: 16),
      const Text(
        'Gaps: Mac build is NOT implemented yet. Automatic update install is implemented (verified download + delayed installer launch). AI OCR for image-only PDFs now works via the Rust core (Ollama vision model), and file Created time works via Rust. Settings are saved on disk and reloaded on launch.',
        style: TextStyle(color: Colors.grey),
      ),
      if (!_settingsLoaded) const Text('Loading saved settings…'),
    ]);
  }
}

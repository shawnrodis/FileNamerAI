// File Namer AI — Flutter version (v1.4.0). The previous Rust/egui version
// is preserved on the egui-legacy branch; this is now the main app.
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
// KNOWN GAPS (honest, not faked): AI OCR for image-only PDFs via
// Ollama vision; Mac DMG; automatic update installer (update check is
// implemented, it only reports availability). Windows right-click menu
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

import 'package:archive/archive.dart';
import 'package:desktop_drop/desktop_drop.dart';
import 'package:file_picker/file_picker.dart';
import 'package:flutter/material.dart';
import 'package:http/http.dart' as http;
import 'package:path/path.dart' as p;
import 'package:path_provider/path_provider.dart';

// ---------------------------------------------------------------- settings

class Settings {
  String ollamaUrl;
  String model;
  String visionModel;
  String dateOrder; // 'MM-DD-YY', 'DD-MM-YY', 'YY-MM-DD'
  String separator;
  bool timeOn;
  String secondsRule; // 'auto', 'keep', 'truncate'
  String timestampSource; // 'earliest', 'latest', 'exif-only'
  bool collisionSuffix;
  String prefix;
  String suffix;

  Settings({
    this.ollamaUrl = 'http://localhost:11434',
    this.model = 'qwen2.5:3b',
    this.visionModel = 'moondream',
    this.dateOrder = 'MM-DD-YY',
    this.separator = '-',
    this.timeOn = true,
    this.secondsRule = 'auto',
    this.timestampSource = 'earliest',
    this.collisionSuffix = true,
    this.prefix = '',
    this.suffix = '',
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
        'collisionSuffix': collisionSuffix,
        'prefix': prefix,
        'suffix': suffix,
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
        collisionSuffix: j['collisionSuffix'] as bool? ?? true,
        prefix: j['prefix'] as String? ?? '',
        suffix: j['suffix'] as String? ?? '',
      );
}

Future<File> _settingsFile() async {
  final dir = await getApplicationSupportDirectory();
  await dir.create(recursive: true);
  return File(p.join(dir.path, 'filenamer_ai_flutter_settings.json'));
}

Future<Settings> loadSettings() async {
  try {
    final f = await _settingsFile();
    if (await f.exists()) {
      return Settings.fromJson(jsonDecode(await f.readAsString()));
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

String _titleCase(String s) => s
    .split(RegExp(r'\s+'))
    .where((w) => w.isNotEmpty)
    .map((w) => w[0].toUpperCase() + (w.length > 1 ? w.substring(1) : ''))
    .join(' ');

// ------------------------------------------------------------- EXIF (JPEG)

int _u16(ByteData d, int off, bool le) =>
    le ? d.getUint16(off, Endian.little) : d.getUint16(off, Endian.big);
int _u32(ByteData d, int off, bool le) =>
    le ? d.getUint32(off, Endian.little) : d.getUint32(off, Endian.big);

DateTime? _parseTiffDate(ByteData d, int tiffStart, bool le) {
  int readAscii(int ifdOff, int tag) {
    // Returns offset of the ASCII value, or -1.
    if (ifdOff + 2 > d.lengthInBytes) return -1;
    final count = _u16(d, ifdOff, le);
    for (var i = 0; i < count; i++) {
      final e = ifdOff + 2 + i * 12;
      if (e + 12 > d.lengthInBytes) break;
      if (_u16(d, e, le) != tag) continue;
      final type = _u16(d, e + 2, le);
      final n = _u32(d, e + 4, le);
      if (type != 2 || n == 0) return -1;
      final valOff = _u32(d, e + 8, le);
      if (n <= 4) return e + 8;
      final abs = tiffStart + valOff;
      return abs < d.lengthInBytes ? abs : -1;
    }
    return -1;
  }

  String readString(int off) {
    final bytes = <int>[];
    for (var i = off; i < d.lengthInBytes; i++) {
      final b = d.getUint8(i);
      if (b == 0) break;
      bytes.add(b);
    }
    return utf8.decode(bytes, allowMalformed: true);
  }

  DateTime? parseDate(String s) {
    // "YYYY:MM:DD HH:MM:SS"
    final m = RegExp(r'(\d{4}):(\d{2}):(\d{2})[ T](\d{2}):(\d{2}):(\d{2})')
        .firstMatch(s);
    if (m == null) return null;
    try {
      return DateTime(
        int.parse(m[1]!), int.parse(m[2]!), int.parse(m[3]!),
        int.parse(m[4]!), int.parse(m[5]!), int.parse(m[6]!));
    } catch (_) {
      return null;
    }
  }

  final ifd0Off = tiffStart + _u32(d, tiffStart + 4, le);
  // ExifIFD pointer is tag 0x8769 in IFD0; its value is an offset from TIFF start.
  int exifPtr = -1;
  if (ifd0Off + 2 <= d.lengthInBytes) {
    final count = _u16(d, ifd0Off, le);
    for (var i = 0; i < count; i++) {
      final e = ifd0Off + 2 + i * 12;
      if (e + 12 > d.lengthInBytes) break;
      if (_u16(d, e, le) == 0x8769) exifPtr = _u32(d, e + 8, le);
    }
  }
  if (exifPtr >= 0) {
    final off = readAscii(tiffStart + exifPtr, 0x9003); // DateTimeOriginal
    if (off >= 0) {
      final dt = parseDate(readString(off));
      if (dt != null) return dt;
    }
    final off2 = readAscii(tiffStart + exifPtr, 0x9004); // DateTimeDigitized
    if (off2 >= 0) {
      final dt = parseDate(readString(off2));
      if (dt != null) return dt;
    }
  }
  final off0 = readAscii(ifd0Off, 0x0132); // DateTime
  if (off0 >= 0) return parseDate(readString(off0));
  return null;
}

Future<DateTime?> parseExifDateTimeOriginal(File file) async {
  try {
    final bytes = await file.readAsBytes();
    if (bytes.length < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8) return null;
    var i = 2;
    while (i + 4 <= bytes.length) {
      if (bytes[i] != 0xFF) break;
      final marker = bytes[i + 1];
      if (marker == 0xDA || marker == 0xD9) break; // SOS / EOI
      final len = (bytes[i + 2] << 8) | bytes[i + 3];
      if (marker == 0xE1 && len > 8) {
        // APP1
        final seg = bytes.sublist(i + 4, i + 2 + len);
        if (seg.length > 6 &&
            seg[0] == 0x45 && seg[1] == 0x78 && seg[2] == 0x69 && seg[3] == 0x66) {
          final tiff = seg.sublist(6);
          if (tiff.length < 8) return null;
          final le = tiff[0] == 0x49; // 'II' little-endian
          final d = ByteData.sublistView(Uint8List.fromList(tiff));
          return _parseTiffDate(d, 0, le);
        }
      }
      i += 2 + len;
    }
  } catch (_) {}
  return null;
}

// ------------------------------------------------------------ MP4 creation

Future<DateTime?> parseMp4Creation(File file) async {
  try {
    final raf = await file.open();
    final head = await raf.read(2 * 1024 * 1024);
    await raf.close();
    for (var type in ['mvhd', 'mdhd']) {
      final needle = utf8.encode(type);
      for (var i = 0; i + 8 < head.length - 4; i++) {
        if (head[i] == needle[0] && head[i + 1] == needle[1] &&
            head[i + 2] == needle[2] && head[i + 3] == needle[3]) {
          final v = head[i + 4];
          int secs;
          if (v == 1) {
            if (i + 12 >= head.length) continue;
            secs = 0;
            for (var k = 0; k < 8; k++) secs = (secs << 8) | head[i + 5 + k];
          } else {
            if (i + 8 >= head.length) continue;
            secs = (head[i + 5] << 24) | (head[i + 6] << 16) |
                (head[i + 7] << 8) | head[i + 8];
          }
          const macEpoch = 2082844800;
          if (secs > macEpoch) {
            return DateTime.fromMillisecondsSinceEpoch(
                (secs - macEpoch) * 1000, isUtc: true).toLocal();
          }
        }
      }
    }
  } catch (_) {}
  return null;
}

// --------------------------------------------------------- text extraction

String _extractParenText(String s) {
  final out = <String>[];
  final re = RegExp(r'\((?:[^()\\]|\\.)*\)');
  for (final m in re.allMatches(s)) {
    var t = m.group(0)!;
    t = t.substring(1, t.length - 1);
    t = t.replaceAllMapped(RegExp(r'\\(.)'), (mm) => mm[1]!);
    if (t.trim().isNotEmpty) out.add(t);
  }
  // Hex strings <48656C6C6F> that decode to printable text.
  final hexRe = RegExp(r'<([0-9A-Fa-f\s]{4,})>');
  for (final m in hexRe.allMatches(s)) {
    final hex = m.group(1)!.replaceAll(RegExp(r'\s'), '');
    if (hex.length.isOdd) continue;
    final bytes = <int>[];
    var ok = true;
    for (var i = 0; i < hex.length; i += 2) {
      final v = int.tryParse(hex.substring(i, i + 2), radix: 16);
      if (v == null) {
        ok = false;
        break;
      }
      bytes.add(v);
    }
    if (!ok) continue;
    final t = utf8.decode(bytes, allowMalformed: true);
    if (t.trim().isNotEmpty && t.codeUnits.every((c) => c >= 32 && c < 127)) {
      out.add(t);
    }
  }
  return out.join(' ');
}

String _extractPdfText(List<int> bytes) {
  final parts = <String>[];
  // Try Flate-decoded streams first (most real PDFs), then raw text.
  var i = 0;
  while (i < bytes.length) {
    // Find "stream" followed by EOL.
    var idx = -1;
    for (var j = i; j + 6 < bytes.length; j++) {
      if (bytes[j] == 0x73 && bytes[j + 1] == 0x74 && bytes[j + 2] == 0x72 &&
          bytes[j + 3] == 0x65 && bytes[j + 4] == 0x61 && bytes[j + 5] == 0x6D) {
        idx = j;
        break;
      }
    }
    if (idx < 0) break;
    var start = idx + 6;
    if (start < bytes.length && bytes[start] == 0x0D) start++;
    if (start < bytes.length && bytes[start] == 0x0A) start++;
    var end = -1;
    for (var j = start; j + 9 < bytes.length; j++) {
      if (bytes[j] == 0x65 && bytes[j + 1] == 0x6E && bytes[j + 2] == 0x64 &&
          bytes[j + 3] == 0x73 && bytes[j + 4] == 0x74 && bytes[j + 5] == 0x72 &&
          bytes[j + 6] == 0x65 && bytes[j + 7] == 0x61 && bytes[j + 8] == 0x6D) {
        end = j;
        break;
      }
    }
    if (end < 0) break;
    try {
      final decoded = zlib.decode(bytes.sublist(start, end));
      final t = _extractParenText(latin1.decode(decoded, allowInvalid: true));
      if (t.isNotEmpty) parts.add(t);
    } catch (_) {}
    i = end + 9;
    if (parts.length > 200) break;
  }
  if (parts.isNotEmpty) return parts.join(' ');
  return _extractParenText(latin1.decode(bytes, allowInvalid: true));
}

String _stripXml(String xml) {
  var s = xml.replaceAll(RegExp(r'<[^>]+>'), ' ');
  s = s.replaceAll('&amp;', '&').replaceAll('&lt;', '<').replaceAll('&gt;', '>')
      .replaceAll('&quot;', '"').replaceAll('&apos;', "'");
  s = s.replaceAll(RegExp(r'\s+'), ' ');
  return s.trim();
}

String _extractOfficeText(List<int> bytes, String ext) {
  try {
    final archive = ZipDecoder().decodeBytes(bytes);
    final texts = <String>[];
    String? findText(String name) {
      for (final f in archive.files) {
        if (f.name == name && f.content != null) {
          return utf8.decode(f.content as List<int>, allowMalformed: true);
        }
      }
      return null;
    }

    if (ext == 'docx') {
      final xml = findText('word/document.xml');
      if (xml != null) texts.add(_stripXml(xml));
      for (final f in archive.files) {
        if ((f.name.startsWith('word/header') ||
                f.name.startsWith('word/footer')) &&
            f.name.endsWith('.xml') &&
            f.content != null) {
          texts.add(_stripXml(
              utf8.decode(f.content as List<int>, allowMalformed: true)));
        }
      }
    } else if (ext == 'odt') {
      final xml = findText('content.xml');
      if (xml != null) texts.add(_stripXml(xml));
    } else if (ext == 'xlsx') {
      final shared = findText('xl/sharedStrings.xml');
      if (shared != null) texts.add(_stripXml(shared));
      for (final f in archive.files) {
        if (f.name.startsWith('xl/worksheets/sheet') &&
            f.name.endsWith('.xml') && f.content != null) {
          texts.add(_stripXml(
              utf8.decode(f.content as List<int>, allowMalformed: true)));
        }
      }
    } else if (ext == 'pptx') {
      for (final f in archive.files) {
        if (f.name.startsWith('ppt/slides/slide') &&
            f.name.endsWith('.xml') && f.content != null) {
          texts.add(_stripXml(
              utf8.decode(f.content as List<int>, allowMalformed: true)));
        }
      }
    }
    return texts.join(' ').trim();
  } catch (_) {
    return '';
  }
}

Future<String> extractText(File file, String ext) async {
  try {
    final bytes = await file.readAsBytes();
    if (ext == 'txt' || ext == 'md' || ext == 'csv' || ext == 'log') {
      return utf8.decode(bytes, allowMalformed: true);
    }
    if (ext == 'pdf') return _extractPdfText(bytes);
    if (ext == 'docx' || ext == 'odt' || ext == 'xlsx' || ext == 'pptx') {
      return _extractOfficeText(bytes, ext);
    }
    // Other binaries: date-only names, no invented text.
    return '';
  } catch (_) {
    return '';
  }
}

// ------------------------------------------------------------------- Ollama

bool dateAppearsInContent(String content, DateTime dt) {
  final c = content.toLowerCase();
  final day = dt.day.toString();
  final year = dt.year.toString();
  final mon = _months[dt.month - 1].toLowerCase();
  final monFull = _monthNames[dt.month - 1].toLowerCase();
  if (!c.contains(year)) return false;
  if (!c.contains(mon) && !c.contains(monFull)) return false;
  // Day as a standalone-ish number near the month is approximated by the
  // day string appearing at all (plus year+month present) — the egui app
  // verifies dates against content the same way, and strips on failure.
  if (!c.contains(day)) return false;
  return true;
}

bool titleWordsInContent(String title, String content) {
  final c = content.toLowerCase();
  final words = title
      .toLowerCase()
      .split(RegExp(r'[^a-z0-9]+'))
      .where((w) => w.length > 2)
      .toList();
  if (words.isEmpty) return false;
  final hits = words.where((w) => c.contains(w)).length;
  if (words.length <= 3) return hits == words.length;
  return hits / words.length >= 0.7;
}

DateTime? parseDocDate(String s) {
  // Accepts "26-Oct-2026", "26 Oct 2026", "2026-10-26", "10/26/2026".
  final m1 = RegExp(r'(\d{1,2})[ -]([A-Za-z]{3,9})[ -](\d{4})').firstMatch(s);
  if (m1 != null) {
    final monStr = m1[2]!.toLowerCase();
    var mon = _months.indexWhere((m) => m.toLowerCase() == monStr.substring(0, 3));
    if (mon < 0) {
      mon = _monthNames.indexWhere((m) => m.toLowerCase() == monStr);
    }
    if (mon >= 0) {
      try {
        return DateTime(int.parse(m1[3]!), mon + 1, int.parse(m1[1]!));
      } catch (_) {}
    }
  }
  final m2 = RegExp(r'(\d{4})-(\d{2})-(\d{2})').firstMatch(s);
  if (m2 != null) {
    try {
      return DateTime(int.parse(m2[1]!), int.parse(m2[2]!), int.parse(m2[3]!));
    } catch (_) {}
  }
  return null;
}

Future<Map<String, dynamic>?> _ollamaOnce(
    Settings s, String text, double temp) async {
  final url = s.ollamaUrl.replaceAll(RegExp(r'/+$'), '');
  final prompt = '''You name documents from their contents. Read the document
below and return ONLY a JSON object: {"title": "...", "date": "DD-Mon-YYYY or null"}.
Rules: the title must be the document's subject, or sender + document type
(e.g. "Screening Notification"), NEVER a field label from the document
(such as "Name", "Date", "Address"). Use Title Case. The date must be a
date actually written in the document, or null if none is written. Never
invent a date or a title.

Document (truncated):
${text.length > 4000 ? text.substring(0, 4000) : text}''';
  final resp = await http
      .post(
        Uri.parse('$url/api/chat'),
        headers: {'Content-Type': 'application/json'},
        body: jsonEncode({
          'model': s.model,
          'stream': false,
          'options': {'temperature': temp},
          'messages': [
            {'role': 'user', 'content': prompt}
          ],
        }),
      )
      .timeout(const Duration(seconds: 120));
  if (resp.statusCode != 200) return null;
  final body = jsonDecode(resp.body) as Map<String, dynamic>;
  final content =
      ((body['message'] as Map<String, dynamic>?)?['content'] as String?) ?? '';
  final jm = RegExp(r'\{[^}]*\}').firstMatch(content);
  if (jm == null) return null;
  try {
    return jsonDecode(jm.group(0)!) as Map<String, dynamic>;
  } catch (_) {
    return null;
  }
}

/// Returns (title, date-or-null, statusNote).
Future<(String?, DateTime?, String)> proposeDocument(
    Settings s, File file, String ext, {bool forceReroll = false}) async {
  final text = await extractText(file, ext);
  if (text.trim().isEmpty) return (null, null, 'No text layer — date fallback');
  // Normal path: 0.2, re-roll at 0.7 only if the title guard fails.
  // New name (single-file dialog): force the 0.7 re-roll, like egui.
  final temps = forceReroll ? [0.7] : [0.2, 0.7];
  for (final temp in temps) {
    Map<String, dynamic>? out;
    try {
      out = await _ollamaOnce(s, text, temp);
    } catch (_) {
      return (null, null, 'Ollama unavailable — date fallback');
    }
    if (out == null) continue;
    final rawTitle = (out['title'] as String?)?.trim() ?? '';
    if (rawTitle.isEmpty) continue;
    if (!titleWordsInContent(rawTitle, text)) continue; // guard: re-roll
    final title = _titleCase(rawTitle);
    DateTime? date;
    final rawDate = out['date'] as String?;
    if (rawDate != null && rawDate.toLowerCase() != 'null') {
      final parsed = parseDocDate(rawDate);
      if (parsed != null && dateAppearsInContent(text, parsed)) {
        date = parsed;
      } // else: strip the date, keep the title (guard)
    }
    return (title, date, temp == 0.7 ? 'Re-rolled' : 'AI named');
  }
  return (null, null, 'AI could not verify title — date fallback');
}

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

void main() {
  runApp(const FileNamerApp());
}

class FileNamerApp extends StatelessWidget {
  const FileNamerApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'File Namer AI (Flutter Trial)',
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
      home: const HomePage(),
    );
  }
}

class HomePage extends StatefulWidget {
  const HomePage({super.key});

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
  String menuStatus = 'Not checked';
  bool menuInstalled = false;
  int lastRenamed = 0;
  int lastFailed = 0;
  bool renameDone = false;

  @override
  void initState() {
    super.initState();
    loadSettings().then((s) {
      if (mounted) setState(() {
        settings = s;
        _settingsLoaded = true;
      });
    });
    if (Platform.isWindows) _checkMenu();
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
      final url = settings.ollamaUrl.replaceAll(RegExp(r'/+$'), '');
      final resp = await http
          .get(Uri.parse('$url/api/tags'))
          .timeout(const Duration(seconds: 10));
      if (resp.statusCode == 200) {
        final body = jsonDecode(resp.body) as Map<String, dynamic>;
        final models = ((body['models'] as List?) ?? [])
            .map((m) => (m as Map<String, dynamic>)['name'] as String? ?? '')
            .where((n) => n.isNotEmpty)
            .toList();
        setState(() {
          ollamaModels = models;
          ollamaStatus = 'Ollama running — ${models.length} model(s) installed';
          ollamaSetupLog = models.isEmpty
              ? 'No models installed yet.'
              : 'Installed: ${models.join(', ')}';
        });
      } else {
        setState(() => ollamaStatus = 'Ollama returned HTTP ${resp.statusCode}');
      }
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
      final tmp = await getTemporaryDirectory();
      final dest = File(p.join(tmp.path, 'OllamaSetup.exe'));
      final client = http.Client();
      final req = http.Request(
          'GET', Uri.parse('https://ollama.com/download/OllamaSetup.exe'));
      final resp = await client.send(req);
      if (resp.statusCode != 200) {
        setState(() { ollamaSetupLog = 'Download failed: HTTP ${resp.statusCode}'; ollamaBusy = false; });
        return;
      }
      final sink = dest.openWrite();
      await resp.stream.pipe(sink);
      await Process.start(dest.path, []);
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
      final url = settings.ollamaUrl.replaceAll(RegExp(r'/+$'), '');
      final client = http.Client();
      final req = http.Request('POST', Uri.parse('$url/api/pull'))
        ..headers['Content-Type'] = 'application/json'
        ..body = jsonEncode({'name': model, 'stream': true});
      final resp = await client.send(req);
      if (resp.statusCode != 200) {
        setState(() { ollamaSetupLog = 'Pull failed: HTTP ${resp.statusCode}'; ollamaBusy = false; });
        return;
      }
      var done = false;
      await for (final line in resp.stream
          .transform(utf8.decoder)
          .transform(const LineSplitter())) {
        if (line.trim().isEmpty) continue;
        try {
          final v = jsonDecode(line) as Map<String, dynamic>;
          if (v['error'] != null) {
            setState(() { ollamaSetupLog = 'Pull failed: ${v['error']}'; ollamaBusy = false; });
            return;
          }
          final status = (v['status'] as String?) ?? '';
          final completed = (v['completed'] as num?)?.toDouble() ?? 0;
          final total = (v['total'] as num?)?.toDouble() ?? 0;
          setState(() => ollamaSetupLog = total > 0
              ? 'Pulling $model — $status ${(completed / total * 100).toStringAsFixed(0)}%'
              : 'Pulling $model — $status');
          if (status == 'success') {
            done = true;
            break;
          }
        } catch (_) {}
      }
      setState(() => ollamaSetupLog = done ? 'Pulled $model.' : 'Pulled $model (stream ended).');
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

  Future<void> _addPaths(List<String> paths) async {
    var added = 0;
    for (final path in paths) {
      final f = File(path);
      if (!await f.exists()) continue;
      if (items.any((it) => it.file.path == path)) continue;
      items.add(FileItem(f));
      added++;
    }
    setState(() {});
    _setLog(added > 0 ? 'Added $added file(s). Press Propose Names.' : 'No new files added.');
  }

  Future<void> _pickFiles() async {
    final result = await FilePicker.platform.pickFiles(allowMultiple: true);
    if (result == null) return;
    await _addPaths(result.paths.whereType<String>().toList());
  }

  Future<void> _proposeOne(FileItem item, {bool reroll = false}) async {
    final kind = kindForExt(item.ext);
    try {
      if (kind == FileKind.photo || kind == FileKind.video) {
        DateTime? exif;
        var hasRealExif = false;
        var hasMediaDate = false;
        if (kind == FileKind.photo) {
          exif = await parseExifDateTimeOriginal(item.file);
          hasRealExif = exif != null;
        } else {
          exif = await parseMp4Creation(item.file);
          hasMediaDate = exif != null;
        }
        final stat = await item.file.stat();
        final modified = stat.modified;
        final candidates = <DateTime>[];
        if (exif != null) candidates.add(exif);
        candidates.add(modified);
        DateTime chosen;
        if (settings.timestampSource == 'latest') {
          chosen = candidates.reduce((a, b) => a.isAfter(b) ? a : b);
        } else if (settings.timestampSource == 'exif-only') {
          chosen = exif ?? modified;
        } else {
          chosen = candidates.reduce((a, b) => a.isBefore(b) ? a : b);
        }
        if (kind == FileKind.video) {
          chosen = DateTime(chosen.year, chosen.month, chosen.day,
              chosen.hour, chosen.minute, 0);
        }
        final datePart = formatPhotoName(chosen, settings, hasRealExif);
        item.proposed = '${settings.prefix}$datePart${settings.suffix}';
        item.status = kind == FileKind.photo
            ? (hasRealExif ? 'Date-based (EXIF)' : 'Date-based (file date)')
            : 'Date-based (video)';
        item.source = kind == FileKind.photo
            ? (hasRealExif ? 'EXIF date taken' : 'file date')
            : (hasMediaDate ? 'embedded media-created date' : 'file date');
      } else if (kind == FileKind.document) {
        item.status = 'Proposing…';
        setState(() {});
        final (title, date, note) = await proposeDocument(
            settings, item.file, item.ext, forceReroll: reroll);
        if (title != null) {
          item.proposed = date != null
              ? '${settings.prefix}$title${settings.suffix}_${formatDocDate(date)}'
              : '${settings.prefix}$title${settings.suffix}';
          item.status = note;
        } else {
          final stat = await item.file.stat();
          item.proposed =
              '${settings.prefix}${formatPhotoName(stat.modified, settings, false)}${settings.suffix}';
          item.status = note;
        }
        item.source = item.status;
      } else {
        final stat = await item.file.stat();
        item.proposed =
            '${settings.prefix}${formatPhotoName(stat.modified, settings, false)}${settings.suffix}';
        item.status = 'Date-based (other file)';
        item.source = 'file date';
      }
    } catch (e) {
      item.proposed = item.stem;
      item.status = 'Error: $e';
      item.source = '';
    }
    item.finalName = item.proposed +
        (item.ext.isEmpty ? '' : '.${item.ext}');
  }

  Future<void> _proposeAll() async {
    if (items.isEmpty) {
      _setLog('No files to name.');
      return;
    }
    setState(() {
      processing = true;
      renameDone = false;
    });
    for (final item in items) {
      await _proposeOne(item);
    }

    // Already-correct detection + collisions.
    final used = <String>{};
    for (final item in items) {
      final extPart = item.ext.isEmpty ? '' : '.${item.ext}';
      if (stripTrailingSuffix(item.stem) == stripTrailingSuffix(item.proposed) &&
          item.stem == item.proposed ||
          stripTrailingSuffix(item.stem) == item.proposed) {
        // Already correct (trailing _N ignored).
        item.alreadyCorrect = true;
        item.finalName = item.baseName;
        item.status = 'Already correct — left alone';
        used.add(item.stem);
        continue;
      }
      item.alreadyCorrect = false;
      var candidate = item.proposed;
      if (settings.collisionSuffix) {
        var n = 1;
        final dir = p.dirname(item.file.path);
        while (used.contains(candidate) ||
            (File(p.join(dir, '$candidate$extPart')).existsSync() &&
                p.join(dir, '$candidate$extPart') != item.file.path)) {
          candidate = '${item.proposed}_$n';
          n++;
        }
      }
      used.add(candidate);
      item.finalName = '$candidate$extPart';
    }

    setState(() => processing = false);
    _setLog('Proposed names for ${items.length} file(s). Edit Final Name, then Rename All.');
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
        const current = '1.4.0';
        if (tag.isEmpty) {
          setState(() => updateStatus = 'Could not check for updates (no releases found).');
        } else if (tag == current) {
          setState(() => updateStatus = 'Up to date (v$current).');
        } else {
          setState(() => updateStatus =
              'Update available: $tag (you have v$current). Auto-install is not implemented yet — download it from the releases page.');
        }
      }
    } catch (e) {
      setState(() => updateStatus = 'Could not check for updates: $e');
    }
    setState(() => updateChecking = false);
  }

  Future<void> _checkMenu() async {
    if (!Platform.isWindows) return;
    try {
      final r = await Process.run(
          'reg', ['query', r'HKCU\Software\Classes\*\shell\FileNamerAI']);
      setState(() {
        menuInstalled = r.exitCode == 0;
        menuStatus = menuInstalled
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
      final exe = Platform.resolvedExecutable;
      await Process.run('reg', [
        'add', r'HKCU\Software\Classes\*\shell\FileNamerAI',
        '/ve', '/d', 'Rename with File Namer AI', '/f'
      ]);
      await Process.run('reg', [
        'add', r'HKCU\Software\Classes\*\shell\FileNamerAI\command',
        '/ve', '/d', '"$exe" "%1"', '/f'
      ]);
      setState(() => menuStatus = 'Right-click menu: installed.');
      await _checkMenu();
    } catch (e) {
      setState(() => menuStatus = 'Right-click menu: failed to install ($e).');
    }
  }

  Future<void> _removeMenu() async {
    if (!Platform.isWindows) return;
    try {
      await Process.run('reg', [
        'delete', r'HKCU\Software\Classes\*\shell\FileNamerAI', '/f'
      ]);
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
          title: const Text('File Namer AI — Flutter Trial'),
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
            await _addPaths(detail.files.map((f) => f.path).toList());
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
              onPressed: processing ? null : _renameAll,
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
            onPressed: processing
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
                  child: TextFormField(
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
          SwitchListTile(
            title: const Text('Include time'),
            value: settings.timeOn,
            onChanged: (v) {
              setState(() => settings.timeOn = v);
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
          SwitchListTile(
            title: const Text('Collision suffix (_1, _2)'),
            value: settings.collisionSuffix,
            onChanged: (v) {
              setState(() => settings.collisionSuffix = v);
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
          '• By default, if the dates disagree, the earliest one wins (EXIF/media date, file Created, file Modified). Note: Dart does not expose file Created on all platforms, so this build uses EXIF + Modified only.'),
      const Text(
          '• Two files sharing a timestamp get a collision suffix (_1, _2 by default); already-correct names are left alone.'),
      const Text(
          '• The Advanced photo/video naming section above can change any of these defaults.'),
      const SizedBox(height: 16),
      const Text(
        'Gaps: AI OCR for image-only PDFs, Mac build, and automatic updater are NOT implemented yet. Settings are saved on disk and reloaded on launch.',
        style: TextStyle(color: Colors.grey),
      ),
      if (!_settingsLoaded) const Text('Loading saved settings…'),
    ]);
  }
}

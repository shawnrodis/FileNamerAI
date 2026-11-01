// Update helpers for FileNamerAI — pure Dart, no imports.
//
// These mirror Homestead's working updater behavior (numeric version
// comparison, exact versioned-asset matching, macOS open-the-.dmg
// install) in this app's own stack. They live here, not in main.dart,
// so they are unit-testable without pulling in Flutter, dart:io, or
// the Rust FFI bridge. Both CI workflows copy the whole lib/
// directory into the generated runner, so this file ships with the
// app on Windows and macOS.
//
// The Mac install flow used to be a self-replace shell script
// (_installMacUpdate): it wrote a script whose $VARIABLE references
// were escaped as \$ in Dart strings, so bash received literal
// $DMG/$MNT text instead of expansions — hdiutil attach failed,
// the script exited before rm/cp/open ever ran, the app had
// already quit, and the user was left on the old version with no
// error. That script is gone: macOS now opens the downloaded .dmg
// like Homestead does, and the app keeps running.

/// Parse "vX.Y.Z" (or "X.Y.Z", tolerating a build suffix like "+1")
/// into numeric components, mirroring Homestead's parse_version:
/// trim, strip a leading 'v', split on '.', take the leading digits
/// of each part (or 0 when a part has none).
List<int> parseUpdateVersion(String s) {
  final clean = s.trim().replaceAll(RegExp(r'^v'), '');
  return clean.split('.').map((part) {
    final digits = RegExp(r'^\d+').firstMatch(part)?.group(0) ?? '';
    return digits.isEmpty ? 0 : int.parse(digits);
  }).toList();
}

/// True when [latest] is a strictly newer X.Y.Z than [current]
/// (numeric: 1.10.0 > 1.9.0), mirroring Homestead's version_newer.
/// Equal versions and older versions both return false, so a
/// rollback or a re-offer of the installed version is never
/// presented as an update.
bool updateVersionNewer(String latest, String current) {
  final a = parseUpdateVersion(latest);
  final b = parseUpdateVersion(current);
  final len = a.length > b.length ? a.length : b.length;
  for (var i = 0; i < len; i++) {
    final x = i < a.length ? a[i] : 0;
    final y = i < b.length ? b[i] : 0;
    if (x != y) return x > y;
  }
  return false;
}

/// The release asset THIS platform installs from — exact name,
/// never a zip or a stray file. macOS: the versioned .dmg.
/// Windows: the NSIS installer. [tag] is the version WITHOUT a
/// leading 'v' (the check strips it from tag_name first).
String expectedUpdateAssetName(String tag, bool isMacOS) {
  return isMacOS ? 'FileNamerAI-v$tag.dmg' : 'FileNamerAI-Setup-v$tag.exe';
}

/// Pick this platform's asset out of a release's asset list — an
/// exact name match, the same strictness Homestead applies.
/// Returns the asset map, or null when the expected file is absent.
Map<String, dynamic>? selectUpdateAsset(
    List<dynamic> assets, String expected) {
  for (final a in assets) {
    if (a is Map<String, dynamic> && a['name'] == expected) return a;
  }
  return null;
}

/// The macOS install command: open the downloaded .dmg and let the
/// user drag the app into Applications (Homestead's flow). The app
/// itself never touches its own bundle and does not exit.
List<String> macOpenCommand(String dmgPath) => ['open', dmgPath];

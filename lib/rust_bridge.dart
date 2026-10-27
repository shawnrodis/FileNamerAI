// Rust core bridge (RustDesk-style: Rust logic, Flutter UI).
//
// The naming, extraction, Ollama, model-pull, and menu logic lives in
// rust/src/lib.rs (extracted from the egui app) and is exposed as a
// plain C ABI: fna_propose, fna_extract_text, fna_resolve_batch,
// fna_check_models, fna_pull_model, fna_download_ollama_installer,
// fna_menu_installed, fna_install_menu, fna_uninstall_menu,
// fna_version, fna_free_string.
//
// This file deliberately uses hand-written C-ABI FFI, not
// flutter_rust_bridge codegen: codegen output is version-sensitive
// to the Flutter/Dart toolchain and cannot be verified on a Linux
// VM without a Flutter SDK, whereas a C ABI cdylib needs only
// `cargo build` on the Actions Windows runner. The Rust library is
// filenamer_core.dll, built from rust/ and copied next to the Flutter
// exe by the workflow before zipping / running the NSIS installer.
//
// If the DLL is missing, these calls throw — they do NOT fall back
// to Dart logic, so a missing Rust build can never silently become
// a pure-Dart app again.
//
// Network calls (propose for documents, check/pull models, download
// installer) run in a background isolate, so the UI thread is never
// blocked by Ollama. Fast calls (resolve_batch, menu check) run
// directly.

import 'dart:convert';
import 'dart:ffi' as ffi;
import 'dart:io';
import 'dart:isolate';

import 'package:ffi/ffi.dart';

typedef _FnaProposeNative = ffi.Pointer<Utf8> Function(
    ffi.Pointer<Utf8>, ffi.Pointer<Utf8>, ffi.Uint32);
typedef _FnaProposeDart = ffi.Pointer<Utf8> Function(
    ffi.Pointer<Utf8>, ffi.Pointer<Utf8>, int);
typedef _FnaStringNative = ffi.Pointer<Utf8> Function(
    ffi.Pointer<Utf8>);
typedef _FnaStringDart = ffi.Pointer<Utf8> Function(
    ffi.Pointer<Utf8>);
typedef _FnaTwoStringNative = ffi.Pointer<Utf8> Function(
    ffi.Pointer<Utf8>, ffi.Pointer<Utf8>);
typedef _FnaTwoStringDart = ffi.Pointer<Utf8> Function(
    ffi.Pointer<Utf8>, ffi.Pointer<Utf8>);
typedef _FnaNoArgNative = ffi.Pointer<Utf8> Function();
typedef _FnaNoArgDart = ffi.Pointer<Utf8> Function();
typedef _FnaFreeNative = ffi.Void Function(ffi.Pointer<Utf8>);
typedef _FnaFreeDart = void Function(ffi.Pointer<Utf8>);
typedef _FnaVoidNative = ffi.Void Function();
typedef _FnaVoidDart = void Function();
typedef _FnaMenuNative = ffi.Int32 Function();
typedef _FnaMenuDart = int Function();

class RustBridge {
  static ffi.DynamicLibrary _open() {
    if (Platform.isWindows) {
      return ffi.DynamicLibrary.open('filenamer_core.dll');
    }
    if (Platform.isMacOS) {
      // A bare dlopen name is not reliably resolved from inside a
      // macOS .app bundle, so try the bundle's Frameworks folder
      // first (…/Contents/Frameworks/libfilenamer_core.dylib, next to
      // …/Contents/MacOS/<exe>) and fall back to the bare name.
      try {
        final exe = Platform.resolvedExecutable;
        final marker = '/Contents/MacOS/';
        final idx = exe.indexOf(marker);
        if (idx >= 0) {
          final fw = '${exe.substring(0, idx)}/Contents/Frameworks/libfilenamer_core.dylib';
          if (File(fw).existsSync()) {
            return ffi.DynamicLibrary.open(fw);
          }
        }
      } catch (_) {}
      return ffi.DynamicLibrary.open('libfilenamer_core.dylib');
    }
    return ffi.DynamicLibrary.open('libfilenamer_core.so');
  }

  static String _readAndFree(
      ffi.DynamicLibrary lib, ffi.Pointer<Utf8> ptr) {
    if (ptr == ffi.nullptr) {
      throw StateError('Rust core returned a null string');
    }
    final result = ptr.toDartString();
    final free = lib.lookupFunction<_FnaFreeNative, _FnaFreeDart>(
        'fna_free_string');
    free(ptr);
    return result;
  }

  static Map<String, dynamic> _decode(String s) {
    return jsonDecode(s) as Map<String, dynamic>;
  }

  /// Propose a name for one file, via the Rust core. attempt 0 = temp
  /// 0.2, attempt > 0 = temp 0.7 (re-roll). Runs in an isolate because
  /// document naming can block on Ollama for up to 180s.
  static Future<Map<String, dynamic>> propose(
      String path, String configJson, int attempt) async {
    final json = await Isolate.run(() => _proposeIsolate((path, configJson, attempt)));
    return _decode(json);
  }

  /// Batch collision / already-correct resolution, via the Rust core.
  /// Fast (filesystem checks only), so it runs directly.
  static Map<String, dynamic> resolveBatch(String inputJson) {
    final lib = _open();
    final fn = lib
        .lookupFunction<_FnaStringNative, _FnaStringDart>('fna_resolve_batch');
    final ptr = inputJson.toNativeUtf8();
    try {
      return _decode(_readAndFree(lib, fn(ptr)));
    } finally {
      malloc.free(ptr);
    }
  }

  static Future<List<String>> checkModels(String ollamaUrl) async {
    final json = await Isolate.run(() => _checkModelsIsolate(ollamaUrl));
    final result = _decode(json);
    if (result['error'] != null) {
      throw StateError(result['error'] as String);
    }
    return ((result['models'] as List?) ?? [])
        .map((m) => m.toString())
        .toList();
  }

  /// Pull one model via the Rust core (blocking, no progress
  /// callback in this version — the UI shows an indeterminate
  /// "Pulling…" state, then refreshes the model list).
  static Future<void> pullModel(String ollamaUrl, String model) async {
    final json = await Isolate.run(() => _pullModelIsolate((ollamaUrl, model)));
    final result = _decode(json);
    if (result['ok'] != true) {
      throw StateError((result['error'] as String?) ?? 'Pull failed');
    }
  }

  /// Download the Ollama installer via the Rust core; returns the
  /// downloaded file path. The caller launches it.
  static Future<String> downloadOllamaInstaller() async {
    final json = await Isolate.run(() => _downloadOllamaIsolate(0));
    final result = _decode(json);
    if (result['path'] == null) {
      throw StateError((result['error'] as String?) ?? 'Download failed');
    }
    return result['path'] as String;
  }

  static void ensureMultiselect() {
    try {
      final lib = _open();
      final fn = lib.lookupFunction<_FnaVoidNative, _FnaVoidDart>(
          'fna_ensure_multiselect');
      fn();
    } catch (_) {
      // Best effort only: a missing core must not block app launch.
    }
  }

  static bool menuInstalled() {
    final lib = _open();
    final fn =
        lib.lookupFunction<_FnaMenuNative, _FnaMenuDart>('fna_menu_installed');
    return fn() != 0;
  }

  static Future<void> installMenu(String exePath) async {
    final json = await Isolate.run(() => _installMenuIsolate(exePath));
    final result = _decode(json);
    if (result['ok'] != true) {
      throw StateError((result['error'] as String?) ?? 'Install failed');
    }
  }

  static Future<void> uninstallMenu() async {
    final json = await Isolate.run(() => _uninstallMenuIsolate(0));
    final result = _decode(json);
    if (result['ok'] != true) {
      throw StateError((result['error'] as String?) ?? 'Remove failed');
    }
  }

  static String version() {
    final lib = _open();
    final fn =
        lib.lookupFunction<_FnaNoArgNative, _FnaNoArgDart>('fna_version');
    return _readAndFree(lib, fn());
  }
}

// Top-level isolate entry points (must not be closures: isolate
// arguments and results must be sendable, so these take plain
// strings/records and return the Rust JSON string).

String _proposeIsolate((String, String, int) args) {
  final (path, configJson, attempt) = args;
  final lib = RustBridge._open();
  final fn = lib.lookupFunction<_FnaProposeNative, _FnaProposeDart>(
      'fna_propose');
  final pathPtr = path.toNativeUtf8();
  final cfgPtr = configJson.toNativeUtf8();
  try {
    return RustBridge._readAndFree(lib, fn(pathPtr, cfgPtr, attempt));
  } finally {
    malloc.free(pathPtr);
    malloc.free(cfgPtr);
  }
}

String _checkModelsIsolate(String ollamaUrl) {
  final lib = RustBridge._open();
  final fn = lib.lookupFunction<_FnaStringNative, _FnaStringDart>(
      'fna_check_models');
  final ptr = ollamaUrl.toNativeUtf8();
  try {
    return RustBridge._readAndFree(lib, fn(ptr));
  } finally {
    malloc.free(ptr);
  }
}

String _pullModelIsolate((String, String) args) {
  final (ollamaUrl, model) = args;
  final lib = RustBridge._open();
  final fn = lib.lookupFunction<_FnaTwoStringNative, _FnaTwoStringDart>(
      'fna_pull_model');
  final urlPtr = ollamaUrl.toNativeUtf8();
  final modelPtr = model.toNativeUtf8();
  try {
    return RustBridge._readAndFree(lib, fn(urlPtr, modelPtr));
  } finally {
    malloc.free(urlPtr);
    malloc.free(modelPtr);
  }
}

String _downloadOllamaIsolate(int _) {
  final lib = RustBridge._open();
  final fn = lib.lookupFunction<_FnaNoArgNative, _FnaNoArgDart>(
      'fna_download_ollama_installer');
  return RustBridge._readAndFree(lib, fn());
}

String _installMenuIsolate(String exePath) {
  final lib = RustBridge._open();
  final fn = lib.lookupFunction<_FnaStringNative, _FnaStringDart>(
      'fna_install_menu');
  final ptr = exePath.toNativeUtf8();
  try {
    return RustBridge._readAndFree(lib, fn(ptr));
  } finally {
    malloc.free(ptr);
  }
}

String _uninstallMenuIsolate(int _) {
  final lib = RustBridge._open();
  final fn = lib.lookupFunction<_FnaNoArgNative, _FnaNoArgDart>(
      'fna_uninstall_menu');
  return RustBridge._readAndFree(lib, fn());
}

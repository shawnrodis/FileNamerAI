// Unit tests for the update helpers in lib/updater.dart.
//
// These cover the exact Mac-updater breakages fixed in v1.4.7:
// version comparison (numeric, not string equality, and never
// fooled by a stale current version), exact versioned-asset
// selection per platform, and the macOS open-the-.dmg command.
// Run with: flutter test
// (No Flutter toolchain exists on the Linux build VM, so these
// are verified by CI / a Mac checkout, not locally.)

import 'package:filenamer_ai_flutter/updater.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  group('parseUpdateVersion', () {
    test('strips a leading v', () {
      expect(parseUpdateVersion('v1.4.6'), [1, 4, 6]);
    });
    test('ignores a build suffix', () {
      expect(parseUpdateVersion('1.4.6+1'), [1, 4, 6]);
    });
    test('keeps multi-digit components numeric', () {
      expect(parseUpdateVersion('1.10.0'), [1, 10, 0]);
    });
  });

  group('updateVersionNewer', () {
    test('newer patch is an update', () {
      expect(updateVersionNewer('1.4.6', '1.4.5'), isTrue);
    });
    test('same version is not an update (no phantom re-offer)', () {
      expect(updateVersionNewer('1.4.6', '1.4.6'), isFalse);
    });
    test('older release is not an update (no downgrade)', () {
      expect(updateVersionNewer('1.4.5', '1.4.6'), isFalse);
    });
    test('numeric, not lexicographic: 1.10.0 > 1.9.0', () {
      expect(updateVersionNewer('1.10.0', '1.9.0'), isTrue);
      expect(updateVersionNewer('1.9.0', '1.10.0'), isFalse);
    });
    test('v prefix on the tag is tolerated', () {
      expect(updateVersionNewer('v1.4.6', '1.4.5'), isTrue);
    });
  });

  group('expectedUpdateAssetName', () {
    test('macOS gets the versioned dmg', () {
      expect(expectedUpdateAssetName('1.4.6', true), 'FileNamerAI-v1.4.6.dmg');
    });
    test('Windows gets the versioned installer', () {
      expect(
          expectedUpdateAssetName('1.4.6', false), 'FileNamerAI-Setup-v1.4.6.exe');
    });
  });

  group('selectUpdateAsset', () {
    final assets = [
      {'name': 'FileNamerAI-v1.4.6.dmg', 'browser_download_url': 'https://dl/dmg'},
      {'name': 'FileNamerAI-Windows.zip', 'browser_download_url': 'https://dl/zip'},
      {'name': 'FileNamerAI-Setup-v1.4.6.exe', 'browser_download_url': 'https://dl/exe'},
    ];

    test('macOS selects the dmg, never the zip or exe', () {
      final asset = selectUpdateAsset(
          assets, expectedUpdateAssetName('1.4.6', true));
      expect(asset?['name'], 'FileNamerAI-v1.4.6.dmg');
    });
    test('Windows selects the installer, never the zip or dmg', () {
      final asset = selectUpdateAsset(
          assets, expectedUpdateAssetName('1.4.6', false));
      expect(asset?['name'], 'FileNamerAI-Setup-v1.4.6.exe');
    });
    test('missing asset returns null', () {
      expect(selectUpdateAsset(assets, 'FileNamerAI-v9.9.9.dmg'), isNull);
      expect(selectUpdateAsset([], 'FileNamerAI-v1.4.6.dmg'), isNull);
    });
  });

  group('macOpenCommand', () {
    test('opens the downloaded dmg path', () {
      expect(macOpenCommand('/tmp/FileNamerAI-v1.4.6.dmg'),
          ['open', '/tmp/FileNamerAI-v1.4.6.dmg']);
    });
  });
}

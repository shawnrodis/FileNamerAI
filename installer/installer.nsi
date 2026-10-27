; File Namer AI — Flutter installer (NSIS 3.x)
; Per-user install — no administrator rights required.
; Installs to %LOCALAPPDATA%\FileNamerAI, registers the right-click
; context menu (HKCU), and creates an Add/Remove Programs entry.
; Mirrors the egui app's installer, adapted for a Flutter build:
; the exe plus flutter_windows.dll, plugin DLLs, and the data/ folder
; (app.so, icudtl.dat, flutter_assets) must all be installed together.

!define APP_NAME "File Namer AI"
!define APP_VERSION "1.4.0"
!define APP_PUBLISHER "Shawn Rodis"
!define APP_URL "https://github.com/shawnrodis/FileNamerAI"

Name "${APP_NAME} ${APP_VERSION}"
OutFile "FileNamerAI-Setup-v${APP_VERSION}.exe"
InstallDir "$LOCALAPPDATA\FileNamerAI"
RequestExecutionLevel user
ShowInstDetails show

!include "MUI2.nsh"
!define MUI_ABORTWARNING
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "$INSTDIR\filenamer_ai_flutter.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Launch File Namer AI"
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "File Namer AI" SecApp
  SectionIn RO  ; required — cannot be unchecked
  SetOutPath "$INSTDIR"
  ; All files from the Flutter Windows Release folder, including data/.
  File /r "staging\*.*"
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  ; Right-click context menu (per-user, no admin).
  ; Mirrors the egui installer: label, icon, Player multi-select model,
  ; and the "%1" command. The app itself also manages this verb from
  ; its Settings > Right-click menu section.
  WriteRegStr HKCU "Software\Classes\*\shell\FileNamerAI" "" "Rename with File Namer AI"
  WriteRegStr HKCU "Software\Classes\*\shell\FileNamerAI" "Icon" "$INSTDIR\filenamer_ai_flutter.exe"
  WriteRegStr HKCU "Software\Classes\*\shell\FileNamerAI" "MultiSelectModel" "Player"
  WriteRegStr HKCU "Software\Classes\*\shell\FileNamerAI\command" "" '"$INSTDIR\filenamer_ai_flutter.exe" "%1"'

  ; Add/Remove Programs entry (per-user).
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FileNamerAI" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FileNamerAI" "DisplayVersion" "${APP_VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FileNamerAI" "Publisher" "${APP_PUBLISHER}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FileNamerAI" "URLInfoAbout" "${APP_URL}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FileNamerAI" "UninstallString" "$\"$INSTDIR\Uninstall.exe$\""
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FileNamerAI" "DisplayIcon" "$INSTDIR\filenamer_ai_flutter.exe"
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FileNamerAI" "EstimatedSize" 21500

  ; Start Menu shortcuts (per-user).
  CreateDirectory "$SMPROGRAMS\File Namer AI"
  CreateShortcut "$SMPROGRAMS\File Namer AI\File Namer AI.lnk" "$INSTDIR\filenamer_ai_flutter.exe"
  CreateShortcut "$SMPROGRAMS\File Namer AI\Uninstall.lnk" "$INSTDIR\Uninstall.exe"
SectionEnd

Section "Desktop Icon" SecDesktop
  CreateShortcut "$DESKTOP\File Namer AI.lnk" "$INSTDIR\filenamer_ai_flutter.exe"
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecApp} "Installs the application, registers the right-click context menu, and adds Start Menu shortcuts."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} "Adds a File Namer AI icon to your desktop."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  Delete "$INSTDIR\filenamer_ai_flutter.exe"
  Delete "$INSTDIR\flutter_windows.dll"
  Delete "$INSTDIR\desktop_drop_plugin.dll"
  Delete "$INSTDIR\dartjni.dll"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir /r "$INSTDIR\data"
  RMDir "$INSTDIR"

  DeleteRegKey HKCU "Software\Classes\*\shell\FileNamerAI"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FileNamerAI"

  Delete "$SMPROGRAMS\File Namer AI\File Namer AI.lnk"
  Delete "$SMPROGRAMS\File Namer AI\Uninstall.lnk"
  RMDir "$SMPROGRAMS\File Namer AI"

  Delete "$DESKTOP\File Namer AI.lnk"
SectionEnd

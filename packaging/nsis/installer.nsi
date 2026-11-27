Unicode True
!include "MUI2.nsh"
!include "FileFunc.nsh"

!ifndef VERSION
  !define VERSION "1.0.0"
!endif
!ifndef PRODUCT_NAME
  !define PRODUCT_NAME "Restic Tray"
!endif
!ifndef MAIN_BINARY_NAME
  !define MAIN_BINARY_NAME "restic-tray"
!endif
!ifndef PUBLISHER
  !define PUBLISHER "nayuengin"
!endif
!ifndef APP_IDENTIFIER
  !define APP_IDENTIFIER "io.github.nayuengin.restic-tray"
!endif

Name "${PRODUCT_NAME} ${VERSION}"
OutFile "${OUT_FILE}"
InstallDir "$LOCALAPPDATA\Programs\${PRODUCT_NAME}"
InstallDirRegKey HKCU "Software\${PUBLISHER}\${PRODUCT_NAME}" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma

!define MUI_ICON "${ICON_PATH}"
!define MUI_UNICON "${ICON_PATH}"
!define MUI_ABORTWARNING

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${LICENSE_PATH}"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "Japanese"
!insertmacro MUI_LANGUAGE "English"

!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}"
!define TOAST_KEY "Software\Classes\AppUserModelId\${APP_IDENTIFIER}"

Section "MainSection" SEC01
  SetOutPath "$INSTDIR"

  ; A running copy (the tray app, or an --mcp server an agent started) keeps the exe open.
  ; Windows then refuses to overwrite it, and a silent install would skip the file and still
  ; report success. A running exe can be renamed, so move it aside; the app deletes
  ; "<exe>.*.old" files the next time it starts.
  Delete "$INSTDIR\${MAIN_BINARY_NAME}.exe"
  IfFileExists "$INSTDIR\${MAIN_BINARY_NAME}.exe" 0 exe_free
    GetTempFileName $0 "$INSTDIR"
    Delete "$0"
    ${GetFileName} "$0" $1
    Rename "$INSTDIR\${MAIN_BINARY_NAME}.exe" "$INSTDIR\${MAIN_BINARY_NAME}.exe.$1.old"
  exe_free:
  File "${BIN_PATH}"
  File "${RESTIC_PATH}"
  File "${ICON_PATH}"
  File "${LICENSE_PATH}"
  File "${NOTICES_PATH}"
  File "${README_PATH}"

  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateDirectory "$SMPROGRAMS\${PRODUCT_NAME}"
  CreateShortCut "$SMPROGRAMS\${PRODUCT_NAME}\${PRODUCT_NAME}.lnk" "$INSTDIR\${MAIN_BINARY_NAME}.exe" "" "$INSTDIR\icon.ico" 0
  CreateShortCut "$DESKTOP\${PRODUCT_NAME}.lnk" "$INSTDIR\${MAIN_BINARY_NAME}.exe" "" "$INSTDIR\icon.ico" 0

  ; Clean up stale shortcuts from legacy "R2 Restic Tray" installs so upgrades do not leave duplicates.
  Delete "$DESKTOP\R2 Restic Tray.lnk"
  Delete "$SMPROGRAMS\R2 Restic Tray\R2 Restic Tray.lnk"
  RMDir "$SMPROGRAMS\R2 Restic Tray"

  WriteRegStr HKCU "Software\${PUBLISHER}\${PRODUCT_NAME}" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${PRODUCT_NAME}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\${MAIN_BINARY_NAME}.exe,0"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" "$\"$INSTDIR\uninstall.exe$\" /S"
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1

  ; Gives the app's notifications its name and icon.
  WriteRegStr HKCU "${TOAST_KEY}" "DisplayName" "${PRODUCT_NAME}"
  WriteRegStr HKCU "${TOAST_KEY}" "IconUri" "$INSTDIR\icon.ico"
SectionEnd

Section "Uninstall"
  Delete "$DESKTOP\${PRODUCT_NAME}.lnk"
  Delete "$SMPROGRAMS\${PRODUCT_NAME}\${PRODUCT_NAME}.lnk"
  RMDir "$SMPROGRAMS\${PRODUCT_NAME}"

  Delete "$INSTDIR\${MAIN_BINARY_NAME}.exe"
  Delete "$INSTDIR\${MAIN_BINARY_NAME}.exe.*.old"
  Delete "$INSTDIR\restic.exe"
  Delete "$INSTDIR\icon.ico"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\THIRD_PARTY_NOTICES.md"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey HKCU "${UNINST_KEY}"
  DeleteRegKey HKCU "${TOAST_KEY}"
  DeleteRegKey HKCU "Software\${PUBLISHER}\${PRODUCT_NAME}"
SectionEnd

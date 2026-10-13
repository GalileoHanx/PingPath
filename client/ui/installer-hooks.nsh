; PingPath's hooks into Tauri's NSIS installer (tauri.conf.json:
; bundle > windows > nsis > installerHooks). The installer is per-machine
; and runs elevated, so the service's subcommands can register, stop and
; remove the Windows service (docs/decisions/007-client-architecture.md):
;
; - before files are copied: refuse an install folder outside Program
;   Files (the service runs as LocalSystem from it), then close the
;   PingPath window, then
;   `pingpath-service stop`, so the running service doesn't hold
;   pingpath-service.exe and wintun.dll open and an upgrade can replace
;   them. The window goes first: if the player won't close it, nothing
;   has been stopped yet (the same goes for uninstalling);
; - after: `pingpath-service install` registers the auto-start service
;   (or updates an existing one), secures %ProgramData%\PingPath and starts
;   it. If that fails, the installation fails visibly: PingPath can't work
;   without its service;
; - before uninstalling: `pingpath-service uninstall` stops and deletes the
;   service, removes leftover routes and the adapter, and deletes
;   %ProgramData%\PingPath. An uninstaller run with /UPDATE (Tauri's
;   updater) only stops it, so the settings stay;
; - after uninstalling, if the player ticked "Delete the application
;   data": the window's own logs in %LOCALAPPDATA%\PingPath too (Tauri
;   deletes only the folders named after the app's identifier).
;
; Upgrading: Tauri's installer offers to uninstall the old version first
; (the default choice). That runs the old uninstaller in full, so the
; service and %ProgramData%\PingPath, client.toml included, are removed
; and installed afresh. Choosing not to uninstall keeps them: the stop
; above lets the files be replaced, and `install` updates the service.
;
; The commands run through nsExec, so no console window flashes up; what
; they print goes to the installer's details, which stay on screen when
; something fails. The file starts with a UTF-8 byte order mark, so
; makensis reads its Arabic as UTF-8 however it is run (the bundler also
; passes -INPUTCHARSET UTF8). It is included before the installer's languages
; are loaded, so messages pick their language from $LANGUAGE at run time
; rather than through LangString. Checked by compiling it into
; tauri-bundler 2.9 and 2.10's templates with makensis.

!define PINGPATH_SERVICE_EXE "$INSTDIR\pingpath-service.exe"
; Windows language IDs, compared with $LANGUAGE.
!define PINGPATH_LANG_ARABIC 1025

; Runs `pingpath-service.exe COMMAND`; leaves its exit code (or "error" if
; it couldn't start) in $0.
!macro PINGPATH_SERVICE COMMAND
  DetailPrint `"${PINGPATH_SERVICE_EXE}" ${COMMAND}`
  nsExec::ExecToLog `"${PINGPATH_SERVICE_EXE}" ${COMMAND}`
  Pop $0
  DetailPrint "pingpath-service ${COMMAND}: $0"
!macroend

; Asks the player to close the PingPath window (or closes it, when
; silent), and aborts if they won't. Tauri's installer and uninstaller do
; the same, but only after the hooks below: asking first means a player
; who keeps the window open leaves the service as it was. tauri-bundler
; 2.10 finds the window by its path (Restart Manager), 2.9 by its process
; name.
!macro PINGPATH_CLOSE_WINDOW
  !ifmacrodef CheckIfAppIsRunning
    !ifmacrodef RestartManager_StartSession
      !insertmacro CheckIfAppIsRunning "$INSTDIR\${MAINBINARYNAME}.exe" "${PRODUCTNAME}"
    !else
      !insertmacro CheckIfAppIsRunning "${MAINBINARYNAME}.exe" "${PRODUCTNAME}"
    !endif
  !endif
!macroend

!macro NSIS_HOOK_PREINSTALL
  Push $0
  Push $1
  ; The service runs as LocalSystem from the install folder and loads
  ; wintun.dll from it, so only administrators may be able to change that
  ; folder. Program Files is such a folder. One picked elsewhere (a new
  ; folder on C:\ or on another drive) usually lets every signed-in user
  ; change its files, and so run code as SYSTEM. So the folder must be
  ; inside Program Files, checked before anything runs from it or is
  ; copied into it. GetFullPathName resolves "..", so a path like
  ; "Program Files\..\x" doesn't pass.
  GetFullPathName $0 "$INSTDIR"
  StrLen $1 "$PROGRAMFILES64\"
  StrCpy $1 "$0\" $1
  ${If} $1 != "$PROGRAMFILES64\"
  ${OrIf} $0 == "$PROGRAMFILES64"
    ${If} $LANGUAGE == ${PINGPATH_LANG_ARABIC}
      MessageBox MB_OK|MB_ICONSTOP|MB_RIGHT|MB_RTLREADING "يجب تثبيت PingPath داخل المجلد $PROGRAMFILES64، لأن خدمته تعمل بصلاحيات النظام ولا يجوز أن يتمكن غير المسؤولين من تعديل ملفاتها. المجلد المختار: $INSTDIR" /SD IDOK
      Pop $1
      Pop $0
      Abort "يجب تثبيت PingPath داخل $PROGRAMFILES64."
    ${Else}
      MessageBox MB_OK|MB_ICONSTOP "PingPath must be installed inside $PROGRAMFILES64: its service runs with full system rights, so only administrators may be able to change its files. The folder chosen was $INSTDIR." /SD IDOK
      Pop $1
      Pop $0
      Abort "PingPath must be installed inside $PROGRAMFILES64."
    ${EndIf}
  ${EndIf}
  Pop $1
  ${If} ${FileExists} "${PINGPATH_SERVICE_EXE}"
    !insertmacro PINGPATH_CLOSE_WINDOW
    pingpath_stop:
    !insertmacro PINGPATH_SERVICE "stop"
    ${If} $0 != "0"
      ; Its files can't be replaced while it runs. Say so now, rather than
      ; let the copy fail on a locked file.
      ${If} $LANGUAGE == ${PINGPATH_LANG_ARABIC}
        MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION|MB_RIGHT|MB_RTLREADING "تعذّر إيقاف خدمة PingPath (الرمز $0)، فلا يمكن استبدال ملفاتها. أعد المحاولة، أو أعد تشغيل الجهاز ثم شغّل المثبّت مجددًا." /SD IDCANCEL IDRETRY pingpath_stop
      ${Else}
        MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "The PingPath service couldn't be stopped (code $0), so its files can't be replaced. Try again, or restart Windows and run the installer again." /SD IDCANCEL IDRETRY pingpath_stop
      ${EndIf}
      Pop $0
      Abort
    ${EndIf}
  ${EndIf}
  Pop $0
!macroend

!macro NSIS_HOOK_POSTINSTALL
  Push $0
  !insertmacro PINGPATH_SERVICE "install"
  ${If} $0 != "0"
    ; The app's files are in place, and so is its uninstaller, but the app
    ; can't work without its service: fail the installation (exit code 2
    ; for a silent one), with the service's own words in the details.
    ${If} $LANGUAGE == ${PINGPATH_LANG_ARABIC}
      MessageBox MB_OK|MB_ICONSTOP|MB_RIGHT|MB_RTLREADING "تعذّر تثبيت خدمة PingPath أو تشغيلها (الرمز $0)، ولن يعمل PingPath بدونها. التفاصيل في نافذة المثبّت. أعد تشغيل الجهاز ثم شغّل المثبّت مجددًا، وإن تكرر الخطأ فأرسل هذه التفاصيل مع بلاغك." /SD IDOK
      Pop $0
      Abort "تعذّر تثبيت خدمة PingPath."
    ${Else}
      MessageBox MB_OK|MB_ICONSTOP "The PingPath service couldn't be installed or started (code $0), and PingPath can't work without it. The installer's details say why. Restart Windows and run the installer again; if it happens again, send those details with your report." /SD IDOK
      Pop $0
      Abort "The PingPath service couldn't be installed."
    ${EndIf}
  ${EndIf}
  Pop $0
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  Push $0
  ${If} ${FileExists} "${PINGPATH_SERVICE_EXE}"
    ${If} $UpdateMode = 1
      !insertmacro PINGPATH_SERVICE "stop"
    ${Else}
      !insertmacro PINGPATH_CLOSE_WINDOW
      !insertmacro PINGPATH_SERVICE "uninstall"
      ${If} $0 != "0"
        ; Removing the files now would strand a registered service, or
        ; routes, with nothing left to remove them. Ask; a silent
        ; uninstall carries on but fails its exit code.
        SetErrorLevel 2
        ${If} $LANGUAGE == ${PINGPATH_LANG_ARABIC}
          MessageBox MB_YESNO|MB_ICONEXCLAMATION|MB_DEFBUTTON2|MB_RIGHT|MB_RTLREADING "تعذّر إيقاف خدمة PingPath وإزالتها (الرمز $0). أعد تشغيل الجهاز ثم أعد المحاولة.$\r$\n$\r$\nهل تريد إزالة بقية PingPath على أي حال؟" /SD IDYES IDYES pingpath_uninstall_anyway
        ${Else}
          MessageBox MB_YESNO|MB_ICONEXCLAMATION|MB_DEFBUTTON2 "The PingPath service couldn't be stopped and removed (code $0). Restart Windows and try again.$\r$\n$\r$\nRemove the rest of PingPath anyway?" /SD IDYES IDYES pingpath_uninstall_anyway
        ${EndIf}
        Pop $0
        Abort
        pingpath_uninstall_anyway:
      ${EndIf}
    ${EndIf}
  ${EndIf}
  Pop $0
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; The window logs to %LOCALAPPDATA%\PingPath\logs (pingpath-diagnostics),
  ; which Tauri's "Delete the application data" leaves behind.
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    SetShellVarContext current
    RMDir /r "$LOCALAPPDATA\PingPath"
  ${EndIf}
!macroend

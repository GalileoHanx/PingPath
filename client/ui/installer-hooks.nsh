; PingPath's hooks into Tauri's NSIS installer (tauri.conf.json:
; bundle > windows > nsis > installerHooks). The installer is per-machine
; and runs elevated, so the service's subcommands can register, stop and
; remove the Windows service (docs/decisions/007-client-architecture.md):
;
; - before files are copied: `pingpath-service stop`, so the running
;   service doesn't hold pingpath-service.exe and wintun.dll open;
; - after: `pingpath-service install` registers the auto-start service
;   (or updates it), secures %ProgramData%\PingPath and starts it;
; - before uninstalling: `pingpath-service uninstall` stops and deletes the
;   service, removes leftover routes and the adapter, and deletes
;   %ProgramData%\PingPath. When Tauri's installer removes the old version
;   as part of an update (/UPDATE), only `stop` runs, so the settings stay.
;
; The commands run through nsExec, so no console window flashes up; what
; they print goes to the installer's details log. makensis reads this file
; as UTF-8 (the bundler passes -INPUTCHARSET UTF8).

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

!macro NSIS_HOOK_PREINSTALL
  Push $0
  ; Nothing to stop on a first install.
  ${If} ${FileExists} "${PINGPATH_SERVICE_EXE}"
    !insertmacro PINGPATH_SERVICE "stop"
    ; If it couldn't stop, copying over its files fails next and NSIS
    ; offers to retry, which is the right question to ask.
  ${EndIf}
  Pop $0
!macroend

!macro NSIS_HOOK_POSTINSTALL
  Push $0
  !insertmacro PINGPATH_SERVICE "install"
  ${If} $0 != "0"
    ; The app is installed but can't work without its service: say so,
    ; and fail the (silent) installation's exit code.
    SetErrorLevel 2
    ${If} $LANGUAGE == ${PINGPATH_LANG_ARABIC}
      MessageBox MB_OK|MB_ICONEXCLAMATION|MB_RIGHT|MB_RTLREADING "تعذّر تثبيت خدمة PingPath أو تشغيلها (الرمز $0). لن يعمل PingPath بدونها. أعد تشغيل الجهاز ثم أعد تشغيل المثبّت، وإن تكرر الخطأ فأرسل سجل المثبّت مع بلاغك." /SD IDOK
    ${Else}
      MessageBox MB_OK|MB_ICONEXCLAMATION "The PingPath service couldn't be installed or started (code $0). PingPath can't work without it. Restart Windows and run the installer again; if it happens again, send the installer's details with your report." /SD IDOK
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

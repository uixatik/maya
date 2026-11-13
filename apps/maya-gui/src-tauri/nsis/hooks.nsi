!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "Processing Maya Integration Options..."

  ; Default to checked if unset (e.g. passive mode)
  ${If} $CheckboxCliPathState == ""
    StrCpy $CheckboxCliPathState ${BST_CHECKED}
  ${EndIf}
  ${If} $CheckboxBrowserExtState == ""
    StrCpy $CheckboxBrowserExtState ${BST_CHECKED}
  ${EndIf}

  ; 1. Add Maya CLI to User PATH if selected
  ${If} $CheckboxCliPathState == ${BST_CHECKED}
    DetailPrint "Adding Maya CLI to User PATH..."
    ReadRegStr $0 HKCU "Environment" "Path"
    StrCmp $0 "" 0 +3
      StrCpy $0 "$INSTDIR\bin"
      Goto +2
      StrCpy $0 "$0;$INSTDIR\bin"
    WriteRegExpandStr HKCU "Environment" "Path" $0
    SendMessage 0xFFFF 0x001A 0 "STR:Environment" /TIMEOUT=5000
    DetailPrint "Maya CLI successfully added to User PATH."
  ${Else}
    DetailPrint "User chose not to add Maya CLI to PATH."
  ${EndIf}

  ; 2. Register Native Messaging Host manifests and Chromium External Extension keys if selected
  ${If} $CheckboxBrowserExtState == ${BST_CHECKED}
    DetailPrint "Registering Native Messaging Host manifests..."
    IfFileExists "$INSTDIR\bin\maya-nmh.exe" 0 +2
      ExecWait '"$INSTDIR\bin\maya-nmh.exe" --install-manifest'

    DetailPrint "Configuring Chromium External Extension Registry..."
    WriteRegStr HKCU "Software\Google\Chrome\Extensions\knldjmfmopnppmplflldnbgahglgbeoh" "path" "$INSTDIR\extensions\browser-capture"
    WriteRegStr HKCU "Software\Google\Chrome\Extensions\knldjmfmopnppmplflldnbgahglgbeoh" "version" "0.1.0"
    WriteRegStr HKCU "Software\Microsoft\Edge\Extensions\knldjmfmopnppmplflldnbgahglgbeoh" "path" "$INSTDIR\extensions\browser-capture"
    WriteRegStr HKCU "Software\Microsoft\Edge\Extensions\knldjmfmopnppmplflldnbgahglgbeoh" "version" "0.1.0"
    DetailPrint "Browser extension integration successfully configured."
  ${Else}
    DetailPrint "User chose not to configure browser integration."
  ${EndIf}

  ; 3. Optionally launch browser to verify extension
  ${If} $CheckboxLaunchBrowserState == ${BST_CHECKED}
    ExecShell "open" "chrome.exe" "chrome://extensions"
    ${If} ${Errors}
      ExecShell "open" "msedge.exe" "edge://extensions"
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  DetailPrint "Deregistering Native Messaging Host manifests..."
  IfFileExists "$INSTDIR\bin\maya-nmh.exe" 0 +2
    ExecWait '"$INSTDIR\bin\maya-nmh.exe" --uninstall-manifest'

  DetailPrint "Cleaning Chromium External Extension Registry..."
  DeleteRegKey HKCU "Software\Google\Chrome\Extensions\knldjmfmopnppmplflldnbgahglgbeoh"
  DeleteRegKey HKCU "Software\Microsoft\Edge\Extensions\knldjmfmopnppmplflldnbgahglgbeoh"

  DetailPrint "Broadcasting environment change..."
  SendMessage 0xFFFF 0x001A 0 "STR:Environment" /TIMEOUT=5000
!macroend

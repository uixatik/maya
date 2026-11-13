!macro customInstall
  DetailPrint "Registering Native Messaging Host manifests..."
  IfFileExists "$INSTDIR\bin\maya-nmh.exe" 0 +2
    ExecWait '"$INSTDIR\bin\maya-nmh.exe" --install-manifest'

  DetailPrint "Configuring Chromium External Extension Registry..."
  WriteRegStr HKCU "Software\Google\Chrome\Extensions\knldjmfmopnppmplflldnbgahglgbeoh" "path" "$INSTDIR\extensions\browser-capture"
  WriteRegStr HKCU "Software\Google\Chrome\Extensions\knldjmfmopnppmplflldnbgahglgbeoh" "version" "0.1.0"
  WriteRegStr HKCU "Software\Microsoft\Edge\Extensions\knldjmfmopnppmplflldnbgahglgbeoh" "path" "$INSTDIR\extensions\browser-capture"
  WriteRegStr HKCU "Software\Microsoft\Edge\Extensions\knldjmfmopnppmplflldnbgahglgbeoh" "version" "0.1.0"

  DetailPrint "Adding Maya CLI to User PATH..."
  ReadRegStr $0 HKCU "Environment" "Path"
  StrCmp $0 "" 0 +3
    StrCpy $0 "$INSTDIR\bin"
    Goto +2
    StrCpy $0 "$0;$INSTDIR\bin"
  WriteRegExpandStr HKCU "Environment" "Path" $0
  SendMessage 0xFFFF 0x001A 0 "STR:Environment" /TIMEOUT=5000
!macroend

!macro customUninstaller
  DetailPrint "Deregistering Native Messaging Host manifests..."
  IfFileExists "$INSTDIR\bin\maya-nmh.exe" 0 +2
    ExecWait '"$INSTDIR\bin\maya-nmh.exe" --uninstall-manifest'

  DetailPrint "Cleaning Chromium External Extension Registry..."
  DeleteRegKey HKCU "Software\Google\Chrome\Extensions\knldjmfmopnppmplflldnbgahglgbeoh"
  DeleteRegKey HKCU "Software\Microsoft\Edge\Extensions\knldjmfmopnppmplflldnbgahglgbeoh"

  DetailPrint "Broadcasting environment change..."
  SendMessage 0xFFFF 0x001A 0 "STR:Environment" /TIMEOUT=5000
!macroend

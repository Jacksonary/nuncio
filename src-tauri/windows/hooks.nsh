; NSIS installer hooks: register Nuncio as a mail client so it shows up in Windows "Default apps".
; The installer is per-machine (installMode "perMachine"), so SHCTX resolves to HKLM and
; these keys are registered machine-wide, next to the app under Program Files.
; Users still choose the default themselves (Windows does not allow programs to set it).
!macro NSIS_HOOK_POSTINSTALL
  ; v0.1.4 and earlier installed per user and registered these under HKCU. HKCU
  ; shadows HKLM, so a leftover set would keep mailto:/.eml/webcal pointing at the
  ; old per-user copy. Removing a key that does not exist is a no-op.
  DeleteRegKey HKCU "Software\Clients\Mail\Nuncio"
  DeleteRegKey HKCU "Software\Classes\Nuncio.mailto"
  DeleteRegKey HKCU "Software\Classes\Nuncio.webcal"
  DeleteRegKey HKCU "Software\Classes\Nuncio.eml"
  DeleteRegKey HKCU "Software\Classes\Nuncio.ics"
  DeleteRegKey HKCU "Software\Classes\Nuncio.vcf"
  DeleteRegValue HKCU "Software\RegisteredApplications" "Nuncio"

  WriteRegStr SHCTX "Software\Clients\Mail\Nuncio" "" "Nuncio"
  WriteRegStr SHCTX "Software\Clients\Mail\Nuncio\Capabilities" "ApplicationName" "Nuncio"
  WriteRegStr SHCTX "Software\Clients\Mail\Nuncio\Capabilities" "ApplicationDescription" "Lightweight mail, calendar, contacts and to-do"
  WriteRegStr SHCTX "Software\Clients\Mail\Nuncio\Capabilities\URLAssociations" "mailto" "Nuncio.mailto"
  WriteRegStr SHCTX "Software\Clients\Mail\Nuncio\Capabilities\URLAssociations" "webcal" "Nuncio.webcal"
  WriteRegStr SHCTX "Software\Clients\Mail\Nuncio\Capabilities\URLAssociations" "webcals" "Nuncio.webcal"
  WriteRegStr SHCTX "Software\Clients\Mail\Nuncio\Capabilities\FileAssociations" ".eml" "Nuncio.eml"
  WriteRegStr SHCTX "Software\Clients\Mail\Nuncio\Capabilities\FileAssociations" ".ics" "Nuncio.ics"
  WriteRegStr SHCTX "Software\Clients\Mail\Nuncio\Capabilities\FileAssociations" ".vcf" "Nuncio.vcf"

  ; mailto: URL protocol
  WriteRegStr SHCTX "Software\Classes\Nuncio.mailto" "" "URL:MailTo Protocol"
  WriteRegStr SHCTX "Software\Classes\Nuncio.mailto" "URL Protocol" ""
  WriteRegStr SHCTX "Software\Classes\Nuncio.mailto\shell\open\command" "" '"$INSTDIR\nuncio.exe" "%1"'

  ; webcal:/webcals: subscription URL protocol
  WriteRegStr SHCTX "Software\Classes\Nuncio.webcal" "" "URL:Calendar Subscription Protocol"
  WriteRegStr SHCTX "Software\Classes\Nuncio.webcal" "URL Protocol" ""
  WriteRegStr SHCTX "Software\Classes\Nuncio.webcal\shell\open\command" "" '"$INSTDIR\nuncio.exe" "%1"'

  ; .eml / .ics / .vcf file associations
  WriteRegStr SHCTX "Software\Classes\Nuncio.eml" "" "Email Message"
  WriteRegStr SHCTX "Software\Classes\Nuncio.eml\DefaultIcon" "" '"$INSTDIR\nuncio.exe",0'
  WriteRegStr SHCTX "Software\Classes\Nuncio.eml\shell\open\command" "" '"$INSTDIR\nuncio.exe" "%1"'

  WriteRegStr SHCTX "Software\Classes\Nuncio.ics" "" "iCalendar"
  WriteRegStr SHCTX "Software\Classes\Nuncio.ics\DefaultIcon" "" '"$INSTDIR\nuncio.exe",0'
  WriteRegStr SHCTX "Software\Classes\Nuncio.ics\shell\open\command" "" '"$INSTDIR\nuncio.exe" "%1"'

  WriteRegStr SHCTX "Software\Classes\Nuncio.vcf" "" "vCard"
  WriteRegStr SHCTX "Software\Classes\Nuncio.vcf\DefaultIcon" "" '"$INSTDIR\nuncio.exe",0'
  WriteRegStr SHCTX "Software\Classes\Nuncio.vcf\shell\open\command" "" '"$INSTDIR\nuncio.exe" "%1"'

  WriteRegStr SHCTX "Software\RegisteredApplications" "Nuncio" "Software\Clients\Mail\Nuncio\Capabilities"
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  DeleteRegKey SHCTX "Software\Clients\Mail\Nuncio"
  DeleteRegKey SHCTX "Software\Classes\Nuncio.mailto"
  DeleteRegKey SHCTX "Software\Classes\Nuncio.webcal"
  DeleteRegKey SHCTX "Software\Classes\Nuncio.eml"
  DeleteRegKey SHCTX "Software\Classes\Nuncio.ics"
  DeleteRegKey SHCTX "Software\Classes\Nuncio.vcf"
  DeleteRegValue SHCTX "Software\RegisteredApplications" "Nuncio"
!macroend

; NSIS installer hooks: register Nuncio as a mail client so it shows up in Windows "Default apps".
; Users still choose the default themselves (Windows does not allow programs to set it).
!macro NSIS_HOOK_POSTINSTALL
  WriteRegStr HKCU "Software\Clients\Mail\Nuncio" "" "Nuncio"
  WriteRegStr HKCU "Software\Clients\Mail\Nuncio\Capabilities" "ApplicationName" "Nuncio"
  WriteRegStr HKCU "Software\Clients\Mail\Nuncio\Capabilities" "ApplicationDescription" "Lightweight mail, calendar, contacts and to-do"
  WriteRegStr HKCU "Software\Clients\Mail\Nuncio\Capabilities\URLAssociations" "mailto" "Nuncio.mailto"
  WriteRegStr HKCU "Software\Clients\Mail\Nuncio\Capabilities\URLAssociations" "webcal" "Nuncio.webcal"
  WriteRegStr HKCU "Software\Clients\Mail\Nuncio\Capabilities\URLAssociations" "webcals" "Nuncio.webcal"
  WriteRegStr HKCU "Software\Clients\Mail\Nuncio\Capabilities\FileAssociations" ".eml" "Nuncio.eml"
  WriteRegStr HKCU "Software\Clients\Mail\Nuncio\Capabilities\FileAssociations" ".ics" "Nuncio.ics"
  WriteRegStr HKCU "Software\Clients\Mail\Nuncio\Capabilities\FileAssociations" ".vcf" "Nuncio.vcf"

  ; mailto: URL protocol
  WriteRegStr HKCU "Software\Classes\Nuncio.mailto" "" "URL:MailTo Protocol"
  WriteRegStr HKCU "Software\Classes\Nuncio.mailto" "URL Protocol" ""
  WriteRegStr HKCU "Software\Classes\Nuncio.mailto\shell\open\command" "" '"$INSTDIR\nuncio.exe" "%1"'

  ; webcal:/webcals: subscription URL protocol
  WriteRegStr HKCU "Software\Classes\Nuncio.webcal" "" "URL:Calendar Subscription Protocol"
  WriteRegStr HKCU "Software\Classes\Nuncio.webcal" "URL Protocol" ""
  WriteRegStr HKCU "Software\Classes\Nuncio.webcal\shell\open\command" "" '"$INSTDIR\nuncio.exe" "%1"'

  ; .eml / .ics / .vcf file associations
  WriteRegStr HKCU "Software\Classes\Nuncio.eml" "" "Email Message"
  WriteRegStr HKCU "Software\Classes\Nuncio.eml\DefaultIcon" "" '"$INSTDIR\nuncio.exe",0'
  WriteRegStr HKCU "Software\Classes\Nuncio.eml\shell\open\command" "" '"$INSTDIR\nuncio.exe" "%1"'

  WriteRegStr HKCU "Software\Classes\Nuncio.ics" "" "iCalendar"
  WriteRegStr HKCU "Software\Classes\Nuncio.ics\DefaultIcon" "" '"$INSTDIR\nuncio.exe",0'
  WriteRegStr HKCU "Software\Classes\Nuncio.ics\shell\open\command" "" '"$INSTDIR\nuncio.exe" "%1"'

  WriteRegStr HKCU "Software\Classes\Nuncio.vcf" "" "vCard"
  WriteRegStr HKCU "Software\Classes\Nuncio.vcf\DefaultIcon" "" '"$INSTDIR\nuncio.exe",0'
  WriteRegStr HKCU "Software\Classes\Nuncio.vcf\shell\open\command" "" '"$INSTDIR\nuncio.exe" "%1"'

  WriteRegStr HKCU "Software\RegisteredApplications" "Nuncio" "Software\Clients\Mail\Nuncio\Capabilities"
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  DeleteRegKey HKCU "Software\Clients\Mail\Nuncio"
  DeleteRegKey HKCU "Software\Classes\Nuncio.mailto"
  DeleteRegKey HKCU "Software\Classes\Nuncio.webcal"
  DeleteRegKey HKCU "Software\Classes\Nuncio.eml"
  DeleteRegKey HKCU "Software\Classes\Nuncio.ics"
  DeleteRegKey HKCU "Software\Classes\Nuncio.vcf"
  DeleteRegValue HKCU "Software\RegisteredApplications" "Nuncio"
!macroend

; Tailsend NSIS installer hooks (tauri.conf.json → bundle.windows.nsis.installerHooks).
; The receiver listens on TCP 4782 and the LAN discovery responder on UDP
; 4782; Windows Firewall drops both inbound by default, which made same-Wi-Fi
; transfers to a Windows box silently time out. The installer runs elevated,
; so netsh can open the port for this executable. Deletes are best-effort —
; a rule that doesn't exist yet returns an error we ignore.

!macro NSIS_HOOK_POSTINSTALL
  nsExec::Exec 'netsh advfirewall firewall delete rule name="Tailsend"'
  nsExec::Exec 'netsh advfirewall firewall delete rule name="Tailsend UDP"'
  nsExec::ExecToLog 'netsh advfirewall firewall add rule name="Tailsend" dir=in action=allow program="$INSTDIR\Tailsend.exe" enable=yes profile=any protocol=TCP'
  nsExec::ExecToLog 'netsh advfirewall firewall add rule name="Tailsend UDP" dir=in action=allow program="$INSTDIR\Tailsend.exe" enable=yes profile=any protocol=UDP'
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  nsExec::Exec 'netsh advfirewall firewall delete rule name="Tailsend"'
  nsExec::Exec 'netsh advfirewall firewall delete rule name="Tailsend UDP"'
!macroend

# Änderungen

## [1.0.0] - 2026-11-05

Erste öffentliche Version.

- Neue Oberfläche: Cockpit mit Modus-Karte, Kosten, Guthaben und Verlauf, dazu eine Schublade für Einstellungen.
- Obsidian wahlweise mit Max-Abo oder mit API-Key starten; die Anzeige macht klar, wer bezahlt.
- Lokal geschätzte Kosten aus den Claude-Protokollen (nur Metadaten) und API-Kosten laut Anthropic mit Admin-Key.
- Guthaben-Schätzung, Monatsbudget und Warnungen im Fenster und am Tray-Symbol.
- Installer mit Startmenü-Eintrag, optionaler Desktop-Verknüpfung und optionalem Autostart; Installation pro Benutzer, ohne Admin-Rechte.
- Deinstallation fragt, ob Einstellungen, Zählerstand und gespeicherte Keys ebenfalls entfernt werden sollen.
- Update-Funktion: sucht beim Start und danach täglich nach neuen Versionen (abschaltbar), installiert nur nach Klick und nur mit gültiger Ed25519-Signatur.
- MIT-Lizenz, Quellcode öffentlich auf GitHub.

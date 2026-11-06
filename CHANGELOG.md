# Changelog

## [1.1.0] - 2026-11-06

- All settings now live in one file, `settings.json`, including the API key and the admin key. Keys are stored encrypted with Windows DPAPI by default. A switch in the settings drawer stores them as plain text instead, which makes the file portable to another PC. Either way, any program running under the same Windows user can read the keys.
- On the first start of this version, existing keys are moved from the Windows Credential Manager into the settings file automatically. The old entry is deleted only after the new one was read back and matched. If any step fails, the old entry stays and the move is tried again at the next start.
- Languages are files: German and English ship with the app, and a new language is one JSON file in the `lang` folder next to the app. The language is chosen in the settings drawer, applies at once and is saved in `settings.json`. On the first start the Windows display language is used if a matching file exists, otherwise English.
- Every text of the interface comes from a language file: window, tray menu and tooltip, banners, error messages.
- The installer places `de.json` and `en.json` in `lang`. Updates overwrite these two; your own language files stay. Uninstalling with "Yes" removes the `lang` folder too.
- Going back to 1.0.x by hand is not supported. 1.0.x reads its keys from the Credential Manager, where 1.1.0 no longer keeps them, so you would have to enter the keys again.

## [1.0.1] - 2026-11-06

- Test der Update-Funktion, keine Funktionsänderung.

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

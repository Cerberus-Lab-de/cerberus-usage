# Cerberus Usage

Kleine Windows-App im Infobereich (Tray). Sie startet Obsidian wahlweise mit dem Max-Abo oder mit einem API-Key und zeigt, was Claude kostet.

## Was die App kann

- **Obsidian starten** im Max-Modus (ohne `ANTHROPIC_API_KEY`) oder im API-Key-Modus. Der Key wird nur an den Obsidian-Prozess übergeben, nie dauerhaft gesetzt.
- **Lokal geschätzte Kosten** aus den Claude-Protokollen unter `%USERPROFILE%\.claude\projects` (heute, Monat, letzte 5 Std., nach Projekt und Modell). Es werden nur Metadaten gelesen, nie Inhalte. Das ist eine Rechnung nach Listenpreis, nicht die Rechnung von Anthropic.
- **API-Kosten laut Anthropic** über die Kosten-API, wenn ein Admin-Key gespeichert ist.
- **Guthaben (geschätzt)**: Startwert aus der Console, plus Aufladungen, minus die API-Kosten seit dem Startwert.
- **Warnungen** bei Monatsbudget, niedrigem Guthaben und Max-Pause: als Banner im Fenster und als farbiger Punkt am Tray-Symbol.

## Voraussetzungen

- Windows 10/11, x86-64
- Zum Bauen: Rust stable (Edition 2024) und die Visual-Studio-Build-Tools

## Bauen

In PowerShell (nicht in Git Bash, dessen `link.exe` stört das Linken):

```powershell
cargo build --release
```

Die App liegt danach unter `target\release\cerberus-usage.exe`. Tests: `cargo test`.

## Installieren

1. `cerberus-usage.exe` nach `%LOCALAPPDATA%\Programs\CerberusUsage\` kopieren.
2. Eine Verknüpfung auf die `.exe` auf dem Desktop anlegen.

Ein Autostart ist nicht eingerichtet. Eine zweite Instanz wird abgelehnt.

## Wo liegen die Keys?

Im Windows-Anmeldeinformationsspeicher (Credential Manager), Dienst `cerberus-usage`. Nie in Dateien, Logs oder der Oberfläche. Die Eingabefelder sind maskiert.

- **API-Key** (`sk-ant-…`): nur für „Mit API-Key starten“. Er muss einem Workspace zugeordnet sein (Geltungsbereich Default), sonst lehnt die API Anfragen ab.
- **Admin-Key** (`sk-ant-admin01-…`): nur für die Kosten-API.

## Wo liegen die Daten?

Unter `%LOCALAPPDATA%\CerberusUsage\`:

- `settings.json`: Startwert, Aufladungen, Monatsbudget, Warngrenze (Beträge in Nano-USD)
- `scan_state.json`: Lesestand der Claude-Protokolle
- `prices.json` (optional): eigene Preisliste, überschreibt die eingebaute

Ist `settings.json` kaputt, zeigt die App eine Meldung mit dem Pfad und sperrt das Speichern, damit die Datei nicht überschrieben wird.

## Andere Pfade

Liegt Obsidian oder der Claude-Protokollordner woanders, trägst du den Pfad in den Einstellungen unter „Pfade“ ein (Feld, „Durchsuchen“, „Speichern“; „Zurücksetzen“ geht zurück auf automatisch). Die Zeile „Verwendet“ zeigt, was gerade gilt. Beide Felder stehen in `settings.json` (`obsidian_path`, `logs_root`).

Suchreihenfolge Obsidian:

1. der eingestellte Pfad (muss auf eine `.exe` zeigen; fehlt die Datei, gibt es einen Fehler und keine stille Suche)
2. `%LOCALAPPDATA%\Programs\Obsidian\Obsidian.exe`
3. `%ProgramFiles%\Obsidian\Obsidian.exe`
4. `%ProgramFiles(x86)%\Obsidian\Obsidian.exe`

Der Prozessname zum Erkennen und Schließen kommt aus dem Dateinamen der gefundenen `.exe`. Eine Änderung gilt beim nächsten Start von Obsidian. Wer den Pfad ändert, während Obsidian läuft, schließt Obsidian vorher; der neue Pfad greift erst beim nächsten Start.

Suchreihenfolge Protokolle:

1. der eingestellte Ordner
2. `%CLAUDE_CONFIG_DIR%\projects`
3. `%USERPROFILE%\.claude\projects`

Ohne `USERPROFILE` zeigt das Fenster einen Hinweis und liest nichts. Eine Änderung des Protokollordners gilt nach einem Neustart der App; die gelesenen Zahlen werden dann aus dem neuen Ordner neu aufgebaut.

## Einschränkung: Einzelkonto

Die Kosten-API gibt es nur für Organisations-Konten. Bei einem Einzelkonto antwortet Anthropic mit „nicht verfügbar“. Die Karte „API-Kosten laut Anthropic“ sagt das dann offen, und das Guthaben wird ohne Kostenabzug gezeigt: Startwert plus Aufladungen. Wer den Betrag aus der Console neu einträgt, hat wieder einen frischen Stand. Mit einer Organisation und einem Admin-Key werden die Kosten seit dem Startwert automatisch abgezogen.

## Alle Kosten sind Schätzungen

Lokale Kosten sind nach Listenpreis berechnet. Die Werte der Kosten-API können laut Anthropic mehrere Stunden hinterherhinken (etwa 4 Std., bis zu 24 Std.). Das Guthaben ist eine Rechnung aus Startwert, Aufladungen und Kosten, nicht der Kontostand der Console.

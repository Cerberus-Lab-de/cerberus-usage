# Cerberus Usage

Kleine Windows-App im Infobereich (Tray). Sie startet Obsidian wahlweise mit dem Claude-Max-Abo oder mit einem API-Key und zeigt, was Claude kostet. Die Anzeige macht klar, wer gerade bezahlt.

![Cerberus Usage](docs/screenshot.png)

*English in short: Cerberus Usage is a small Windows tray app. It launches Obsidian either with your Claude Max subscription or with an API key, and shows what Claude costs, estimated locally from Claude's log metadata, plus the real API cost from Anthropic if you provide an admin key. Windows 10/11 x86-64 only, MIT licensed, no telemetry. The installer is not code-signed, so SmartScreen warns on first run; updates are verified with an Ed25519 signature.*

## Was die App kann

- **Obsidian starten** im Max-Modus (ohne `ANTHROPIC_API_KEY`) oder im API-Key-Modus. Der Key wird nur an den Obsidian-Prozess übergeben, nie dauerhaft gesetzt. Beide Modi entfernen auch `ANTHROPIC_AUTH_TOKEN` für den Obsidian-Prozess, weil er sonst jede andere Anmeldung übersteuert. Ist `ANTHROPIC_BASE_URL` oder `CLAUDE_CODE_USE_BEDROCK/VERTEX/FOUNDRY` gesetzt, bleibt es unverändert, aber das Fenster warnt und nennt den Namen (nie den Wert). Die Anzeige „Max“ bzw. „API-Key“ gilt nur, sofern in Claude selbst (`apiKeyHelper`, `env`-Block in den Claude-Einstellungen) keine andere Anmeldung eingerichtet ist; das kann die App nicht sehen.
- **Prüfung, ob Obsidian läuft**: schlägt sie fehl (z. B. `tasklist` gesperrt), schließt und startet die App nichts und sagt es.
- **Lokal geschätzte Kosten** aus den Claude-Protokollen unter `%USERPROFILE%\.claude\projects` (heute, Monat, letzte 5 Std., nach Projekt und Modell). Es werden nur Metadaten gelesen, nie Inhalte. Das ist eine Rechnung nach Listenpreis, nicht die Rechnung von Anthropic.
- **API-Kosten laut Anthropic** über die Kosten-API, wenn ein Admin-Key gespeichert ist.
- **Guthaben (geschätzt)**: Startwert aus der Console, plus Aufladungen, minus die API-Kosten seit dem Startwert.
- **Warnungen** bei Monatsbudget, niedrigem Guthaben und Max-Pause: als Banner im Fenster und als farbiger Punkt am Tray-Symbol.
- **Updates**: siehe unten.

## Installieren

1. Den neuesten `CerberusUsage-Setup-<Version>.exe` von der [Releases-Seite](https://github.com/Cerberus-Lab-de/cerberus-usage/releases) laden und starten.
2. Der Installer läuft pro Benutzer, ohne Admin-Rechte, und installiert nach `%LOCALAPPDATA%\Programs\CerberusUsage\`. Er legt einen Startmenü-Eintrag an; Desktop-Verknüpfung (an) und Autostart (aus) sind Häkchen im Setup. Die App erscheint unter Windows → Apps.

**SmartScreen:** Der Installer ist nicht mit einem Windows-Zertifikat signiert (das kostet Geld und ist eine eigene Entscheidung). Windows zeigt deshalb beim ersten Start „Der Computer wurde durch Windows geschützt“. Klick auf **Weitere Informationen → Trotzdem ausführen**. Wer prüfen will, was er startet, baut die App aus dem Quellcode (siehe unten).

**Deinstallieren:** Windows → Apps → Cerberus Usage. Die Deinstallation fragt, ob Einstellungen, Zählerstand und gespeicherte Keys ebenfalls entfernt werden sollen. Bei „Nein“ (und bei stiller Deinstallation) bleiben sie erhalten.

## Updates und Signatur

Die App sucht beim Start und danach einmal täglich nach einer neuen Version (Releases dieses Repos). Gibt es eine, erscheint im Cockpit ein dezenter Hinweis. Installiert wird nur nach Klick auf „Jetzt aktualisieren“, nie im Hintergrund.

Jedes Release enthält neben dem Installer eine Manifest-Datei (`…exe.manifest.json`) mit Version, Dateiname, SHA-256 und einer **Ed25519-Signatur**. Der öffentliche Schlüssel ist in die App eingebaut; der private liegt nur im Windows-Anmeldeinformationsspeicher des Entwicklers, nie im Repo oder auf GitHub. Die App installiert nichts, wenn die Signatur falsch ist oder fehlt, die Prüfsumme nicht passt oder die Version nicht neuer ist als die laufende. Auch ein gekapertes GitHub-Konto kann so kein Update unterschieben.

Der Schalter „Nach Updates suchen“ in der Schublade schaltet das ab (dann geht keine Anfrage an GitHub raus). Dort stehen auch die aktuelle Version und „Jetzt prüfen“.

## Datenschutz: was die App liest und sendet

- **Liest lokal:** die Claude-Protokolle unter `%USERPROFILE%\.claude\projects` (oder `%CLAUDE_CONFIG_DIR%\projects`), und zwar **nur Metadaten** wie Zeit, Modell und Token-Zahlen, nie Gesprächsinhalte.
- **Sendet an Anthropic** (`api.anthropic.com`) nur, wenn du einen **Admin-Key** gespeichert hast: die Abfrage der Kosten-API. Ohne Admin-Key geht nichts an Anthropic. Der normale API-Key geht nur an den von dir gestarteten Obsidian-Prozess, nicht an die App-Anfragen.
- **GitHub** (`api.github.com`, `github.com` und die Download-Server für Releases) wird **nur für die Update-Prüfung** und den Update-Download angesprochen, und nur solange „Nach Updates suchen“ an ist. Die Anfrage enthält nur die Versionsabfrage.
- **Keine Telemetrie.** Es werden keine Nutzungsdaten gesendet, es gibt keine Konten und keinen eigenen Server.
- **Keys** liegen im Windows-Anmeldeinformationsspeicher (Credential Manager), Dienst `cerberus-usage`. Nie in Dateien, Logs oder der Oberfläche; die Eingabefelder sind maskiert.
  - **API-Key** (`sk-ant-…`): nur für „Mit API-Key starten“. Er muss einem Workspace zugeordnet sein (Geltungsbereich Default), sonst lehnt die API Anfragen ab.
  - **Admin-Key** (`sk-ant-admin01-…`): nur für die Kosten-API.

### Wo liegen die Daten?

Unter `%LOCALAPPDATA%\CerberusUsage\`:

- `settings.json`: Startwert, Aufladungen, Monatsbudget, Warngrenze (Beträge in Nano-USD), Update-Schalter
- `scan_state.json`: Lesestand der Claude-Protokolle und die gelesenen Verbrauchsdaten. Ist die Datei nicht lesbar, wird sie als `scan_state.json.bad` zur Seite gelegt und das Fenster sagt es einmal.
- `crash.log`: kleine Textdatei mit Zeit, Ort und Meldung bei Abstürzen
- `prices.json` (optional): eigene Preisliste, überschreibt die eingebaute

Ist `settings.json` kaputt, zeigt die App eine Meldung mit dem Pfad und sperrt das Speichern, damit die Datei nicht überschrieben wird.

## Andere Pfade

Liegt Obsidian oder der Claude-Protokollordner woanders, trägst du den Pfad in der Schublade unter „Pfade“ ein (Feld, „Durchsuchen“, „Speichern“; „Zurücksetzen“ geht zurück auf automatisch). Beide Felder stehen in `settings.json` (`obsidian_path`, `logs_root`).

Suchreihenfolge Obsidian: eingestellter Pfad (muss auf eine vorhandene `.exe` zeigen), dann `%LOCALAPPDATA%\Programs\Obsidian\Obsidian.exe`, `%ProgramFiles%\Obsidian\Obsidian.exe`, `%ProgramFiles(x86)%\Obsidian\Obsidian.exe`. Eine Änderung gilt beim nächsten Start von Obsidian.

Suchreihenfolge Protokolle: eingestellter Ordner, dann `%CLAUDE_CONFIG_DIR%\projects`, dann `%USERPROFILE%\.claude\projects`. Eine Änderung gilt nach einem Neustart der App.

## Einschränkung: Einzelkonto

Die Kosten-API gibt es nur für Organisations-Konten. Bei einem Einzelkonto antwortet Anthropic mit „nicht verfügbar“. Die Karte „API-Kosten laut Anthropic“ sagt das dann offen, und das Guthaben wird ohne Kostenabzug gezeigt: Startwert plus Aufladungen. Ohne Kosten-API wird die Budget-Warnung nie ausgelöst.

## Alle Kosten sind Schätzungen

Lokale Kosten sind nach Listenpreis berechnet. Die Werte der Kosten-API können laut Anthropic mehrere Stunden hinterherhinken. Das Guthaben ist eine Rechnung aus Startwert, Aufladungen und Kosten, nicht der Kontostand der Console.

## Fenster und Beenden

Das X-Symbol versteckt das Fenster im Tray. Ein Klick auf das Tray-Symbol holt es zurück. „Beenden“ im Tray-Menü wartet, falls gerade ein Start läuft, bis dieser fertig ist.

## Aus dem Quellcode bauen

Voraussetzungen: Windows 10/11 x86-64, Rust stable (Edition 2024) und die Visual-Studio-Build-Tools. In PowerShell (nicht in Git Bash, dessen `link.exe` stört das Linken):

```powershell
cargo test
cargo build --release
```

Die App liegt danach unter `target\release\cerberus-usage.exe`.

## Release-Prozess

Für Maintainer. Zusätzlich nötig: [Inno Setup 6](https://jrsoftware.org/isinfo.php) (`winget install JRSoftware.InnoSetup`), die GitHub CLI (`gh`, angemeldet) und der Signaturschlüssel im Credential Store (einmalig mit `cargo run --release --bin sign-release -- keygen` erzeugt).

1. Version in `Cargo.toml` erhöhen, Abschnitt in `CHANGELOG.md` ergänzen, committen und pushen.
2. `.\release.ps1 -DryRun` prüft Tests, Build und Installer, ohne zu signieren oder hochzuladen.
3. `.\release.ps1` baut den Installer nach `dist\`, signiert das Manifest und fragt „Release v… auf GitHub veröffentlichen? (j/n)“. Erst bei „j“ wird das Release angelegt. Das Skript pusht nie Commits.

Die CI (`.github/workflows/ci.yml`) führt bei jedem Push Tests und Release-Build aus. Sie signiert und veröffentlicht nichts.

## Lizenz

[MIT](LICENSE), Copyright (c) 2026 Cerberus Lab.

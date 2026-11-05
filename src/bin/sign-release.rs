//! Release signing tool (not shipped in the installer).
//!
//!   sign-release keygen [--force] [--print-private-once]
//!   sign-release sign <installer> <version>
//!
//! The private key lives only in the Windows credential store
//! (service `cerberus-usage-release`, entry `signing-key`, hex).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cerberus_usage::release_sign::{
    generate_keypair, private_key_hex, public_key_hex, sign_manifest, signing_key_from_hex,
};

const SERVICE: &str = "cerberus-usage-release";
const ENTRY: &str = "signing-key";

const USAGE: &str = "Verwendung:
  sign-release keygen [--force] [--print-private-once]
  sign-release sign <installer> <version>

keygen  erzeugt den Signaturschlüssel im Windows-Credential-Store und gibt NUR den
        öffentlichen Schlüssel aus. --force überschreibt einen vorhandenen Schlüssel.
        --print-private-once gibt den privaten Schlüssel einmalig für das Backup aus.
sign    schreibt <installer>.manifest.json neben den Installer.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("Fehler: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("--help" | "-h" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some("keygen") => keygen(&args[1..]),
        Some("sign") => sign(&args[1..]),
        _ => Err(format!("unbekannter oder fehlender Befehl.\n\n{USAGE}")),
    }
}

fn entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, ENTRY).map_err(|_| "Credential-Store nicht verfügbar".to_string())
}

fn keygen(flags: &[String]) -> Result<(), String> {
    let mut force = false;
    let mut print_private = false;
    for f in flags {
        match f.as_str() {
            "--force" => force = true,
            "--print-private-once" => print_private = true,
            other => return Err(format!("unbekannte Option {other}\n\n{USAGE}")),
        }
    }
    let entry = entry()?;
    match entry.get_password() {
        Ok(_) if !force => {
            return Err("es existiert bereits ein Signaturschlüssel. Mit --force überschreiben \
                        (bereits ausgelieferte Apps können neue Releases dann nicht mehr verifizieren)."
                .to_string());
        }
        Ok(_) | Err(keyring::Error::NoEntry) => {}
        Err(_) => return Err("Credential-Store konnte nicht gelesen werden".to_string()),
    }
    let key = generate_keypair()?;
    entry
        .set_password(&private_key_hex(&key))
        .map_err(|_| "Schlüssel konnte nicht gespeichert werden".to_string())?;
    println!("Öffentlicher Schlüssel (hex): {}", public_key_hex(&key));
    println!(
        "Hinweis: Sichere den privaten Schlüssel in deinem Passwort-Manager. \
         Geht er verloren, kann keine Update-Signatur mehr erzeugt werden."
    );
    if print_private {
        println!("Privater Schlüssel (hex, einmalig): {}", private_key_hex(&key));
    }
    Ok(())
}

fn sign(args: &[String]) -> Result<(), String> {
    let [installer, version] = args else {
        return Err(format!("sign erwartet <installer> <version>\n\n{USAGE}"));
    };
    let version =
        semver::Version::parse(version).map_err(|_| "ungültige Version (Semver)".to_string())?;
    let installer = PathBuf::from(installer);
    let file_name = installer
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("ungültiger Installer-Pfad")?
        .to_string();
    let bytes = std::fs::read(&installer)
        .map_err(|_| "Installer konnte nicht gelesen werden".to_string())?;
    let key = match entry()?.get_password() {
        Ok(hex) => signing_key_from_hex(&hex)?,
        Err(keyring::Error::NoEntry) => {
            return Err(
                "kein Signaturschlüssel vorhanden. Erst `sign-release keygen` ausführen."
                    .to_string(),
            );
        }
        Err(_) => return Err("Credential-Store konnte nicht gelesen werden".to_string()),
    };
    let manifest = sign_manifest(&key, &version, &bytes, &file_name);
    let json = serde_json::to_vec_pretty(&manifest)
        .map_err(|_| "Manifest nicht serialisierbar".to_string())?;
    let mut out = installer.as_os_str().to_owned();
    out.push(".manifest.json");
    let out = PathBuf::from(out);
    write_atomic(&out, &json)
        .map_err(|e| format!("Manifest konnte nicht geschrieben werden ({})", e.kind()))?;
    println!("Manifest geschrieben: {}", out.display());
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

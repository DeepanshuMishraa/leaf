//! The simulator pane's 3D phones. They aren't shipped inside leaf, or kept
//! in its repository, so nobody downloads them just by upgrading:
//! `leaf sim install` lists them, asks, downloads them from leaf's model
//! store, checks each against its size and checksum, and turns the simulator
//! on in leaf's config so Rex's palette offers it. macOS only.
//!
//! The store is the Cloudflare R2 bucket `leaf-sim-models`, served at
//! `STORE`. A changed model goes up under a new version folder, and `STORE`
//! and `MODELS` move to it, so a URL always names the same bytes:
//!
//! ```text
//! wrangler r2 object put leaf-sim-models/v2/<file> --file <file> \
//!   --content-type model/vnd.usdz+zip \
//!   --cache-control "public, max-age=31536000, immutable" --remote
//! ```
//!
//! The models, and what the pane relies on in each (see `Phone.models` in
//! src/simulator.swift):
//!
//! - iphone.usdz: made for leaf by its author. Screen material
//!   COLOUR_Cherry_Screen.
//! - iphone-duo-open.usdz: the iPhone Duo, open, baked for leaf from "Apple
//!   iPhone Duo Fold Night Sky 2026" by extraakash
//!   (https://sketchfab.com/AakashMansukhani), Sketchfab Standard. Screen
//!   COLOUR_Cherry_Screen, cover display C_NS_Screen_Outer; the pane folds
//!   it by turning the left half half a turn about x = 0, z = 0.002584.
//! - iphone-air.usdz (sky blue), iphone-17-pro.usdz (deep blue),
//!   iphone-16.usdz (teal): supplied by leaf's author. Screens _7Air_Screen
//!   (facing -Z), Screen and HdvBvHLgXAUNOwl; the 17 Pro file shows the phone
//!   twice, front and back, and the pane keeps the one with the screen.

use std::fmt;
use std::fs;
use std::io::{self, IsTerminal, Read as _, Write as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

use crate::spinner;

/// Where the models are served from. A new set of models goes under a new
/// version, so a file's URL always names the same bytes.
const STORE: &str = "https://leaf-models.dipxsy.app/v1";
const USER_AGENT: &str = concat!("leaf/", env!("CARGO_PKG_VERSION"));
/// The Duo's model, which can't be swapped for another.
const DUO: &str = "iphone-duo-open.usdz";

/// One phone model in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    /// Its file name, in the store and on disk.
    pub file: &'static str,
    /// The iPhones it's drawn for.
    pub phones: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

/// Every model `leaf sim install` downloads. `Body::model` in simulator.rs
/// names one of these files.
pub const MODELS: [Model; 5] = [
    Model {
        file: "iphone.usdz",
        phones: "Every other iPhone",
        bytes: 2_018_308,
        sha256: "6d25abe9fa1e05e0fac98e18055071cf638212700a5e186b19aea54d464cc04d",
    },
    Model {
        file: "iphone-duo-open.usdz",
        phones: "iPhone Duo (folds and opens)",
        bytes: 5_518_334,
        sha256: "35a0d21666ba69c778678c85625eb1e630302c078808fe0c2b47b49f5a04daab",
    },
    Model {
        file: "iphone-air.usdz",
        phones: "iPhone Air",
        bytes: 845_282,
        sha256: "72368185f3336edc87f91176b5039dbbd035db5bfc809b476df5beab139a35eb",
    },
    Model {
        file: "iphone-17-pro.usdz",
        phones: "iPhone 17 Pro, 17 Pro Max",
        bytes: 620_541,
        sha256: "b0779541ac312843bb0cce4e1ef503483a858a60fab1dff029e3f166c5a0d8e6",
    },
    Model {
        file: "iphone-16.usdz",
        phones: "iPhone 16, 16 Plus",
        bytes: 2_241_500,
        sha256: "70065a1d63ed1929d71cf682b3c58c551f7f0a45f3300bc4eba576ef086823f0",
    },
];

/// Why the models couldn't be installed or found.
#[derive(Debug)]
pub enum ModelsFailed {
    /// HOME is unset, so there's no Library folder to keep them in.
    NoHome,
    /// The person said no, or there was no one to ask.
    Declined {
        interactive: bool,
    },
    Download {
        file: &'static str,
        url: String,
        cause: String,
    },
    Corrupt {
        file: &'static str,
        expected: &'static str,
        actual: String,
    },
    Disk {
        path: PathBuf,
        cause: io::Error,
    },
}

impl fmt::Display for ModelsFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModelsFailed::NoHome => write!(
                f,
                "HOME is unset, so there's nowhere to keep the simulator's 3D models. Set HOME and run `leaf sim install` again."
            ),
            ModelsFailed::Declined { interactive: true } => write!(
                f,
                "nothing was installed. Run `leaf sim install` again when you want the simulator pane."
            ),
            ModelsFailed::Declined { interactive: false } => write!(
                f,
                "`leaf sim install` needs a yes before it downloads, and there's no terminal to ask in. Run `leaf sim install --yes` to accept."
            ),
            ModelsFailed::Download { file, url, cause } => write!(
                f,
                "couldn't download {file} from {url} ({cause}). The models already installed are kept; check the connection and run `leaf sim install` again to finish."
            ),
            ModelsFailed::Corrupt {
                file,
                expected,
                actual,
            } => write!(
                f,
                "{file} arrived damaged (its SHA-256 is {actual}, not {expected}), so it was thrown away. Run `leaf sim install` again; if it keeps happening, please report it."
            ),
            ModelsFailed::Disk { path, cause } => write!(
                f,
                "couldn't write {} ({cause}). Nothing there was changed; free some space or fix its permissions, then run `leaf sim install` again.",
                path.display()
            ),
        }
    }
}

impl From<ModelsFailed> for String {
    fn from(failed: ModelsFailed) -> String {
        failed.to_string()
    }
}

impl Model {
    fn url(&self) -> String {
        format!("{STORE}/{}", self.file)
    }

    /// True when it's on disk whole: its size and SHA-256 match. A damaged
    /// or half-replaced file counts as missing, so `leaf sim install`
    /// fetches it again.
    fn installed(&self) -> Result<bool, ModelsFailed> {
        Ok(self.is_whole(&dir()?.join(self.file)))
    }

    /// True when `path` holds exactly this model.
    fn is_whole(&self, path: &std::path::Path) -> bool {
        if !fs::metadata(path).is_ok_and(|meta| meta.len() == self.bytes) {
            return false;
        }
        let Ok(mut file) = fs::File::open(path) else {
            return false;
        };
        let mut hash = Sha256::new();
        let mut chunk = [0; 64 * 1024];
        loop {
            match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => hash.update(&chunk[..read]),
                Err(_) => return false,
            }
        }
        hex(&hash.finalize()) == self.sha256
    }
}

/// Where the models live: `~/Library/Application Support/leaf/simulator`.
pub fn dir() -> Result<PathBuf, ModelsFailed> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or(ModelsFailed::NoHome)?;
    Ok(PathBuf::from(home).join("Library/Application Support/leaf/simulator"))
}

/// The models not yet installed.
pub fn missing() -> Result<Vec<Model>, ModelsFailed> {
    let mut missing = Vec::new();
    for model in MODELS {
        if !model.installed()? {
            missing.push(model);
        }
    }
    Ok(missing)
}

/// The installed model `file`, falling back to the one every other iPhone
/// uses (but not for the Duo, which only its own model can fold); None when
/// neither is installed.
pub fn path(file: &str) -> Result<Option<PathBuf>, ModelsFailed> {
    let fallback = MODELS[0].file;
    let candidates = MODELS.iter().filter(|model| model.file == file).chain(
        MODELS
            .iter()
            .filter(|model| model.file == fallback && file != DUO),
    );
    for model in candidates {
        if model.installed()? {
            return Ok(Some(dir()?.join(model.file)));
        }
    }
    Ok(None)
}

/// `leaf sim install`: lists the models, asks (unless `yes`), downloads the
/// missing ones, turns the simulator on in leaf's config and has Rex reload
/// its palette.
pub fn install(yes: bool) -> Result<(), ModelsFailed> {
    let dir = dir()?;
    let missing = missing()?;
    println!("The iOS Simulator pane draws each simulator on a 3D iPhone. Its models:\n");
    let width = MODELS
        .iter()
        .map(|model| model.phones.len())
        .max()
        .unwrap_or(0);
    for model in MODELS {
        let state = if missing.contains(&model) {
            ""
        } else {
            "  installed"
        };
        println!(
            "  {:<width$}  {:>8}{state}",
            model.phones,
            spinner::megabytes(model.bytes)
        );
    }
    if missing.is_empty() {
        println!("\nAll installed in {}.", dir.display());
    } else {
        let total: u64 = missing.iter().map(|model| model.bytes).sum();
        println!(
            "\n{} to download ({}) into {}.",
            plural(missing.len(), "model"),
            spinner::megabytes(total),
            dir.display()
        );
        if !yes {
            confirm("Install them?")?;
        }
        fs::create_dir_all(&dir).map_err(|cause| ModelsFailed::Disk {
            path: dir.clone(),
            cause,
        })?;
        for model in &missing {
            spinner::step(&format!("Downloading {}", model.phones), |spinner| {
                fetch(model, |got| {
                    spinner.detail(format!(
                        "({} of {})",
                        spinner::megabytes(got),
                        spinner::megabytes(model.bytes)
                    ))
                })
            })?;
        }
    }
    let config = crate::config::enable_simulator()
        .map_err(|(path, cause)| ModelsFailed::Disk { path, cause })?;
    println!("Turned the simulator on in {}.", config.display());
    if reload_rex() {
        println!("Rex's palette now has Leaf: Open Simulator….");
    } else {
        println!(
            "Restart Rex (or run `rex config reload`) to see Leaf: Open Simulator… in its palette."
        );
    }
    println!("From a terminal: `leaf sim list`, then `leaf sim open <udid>` in a Rex pane.");
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Asks a yes-or-no question on the terminal; anything but yes declines.
fn confirm(question: &str) -> Result<(), ModelsFailed> {
    if !io::stdin().is_terminal() {
        return Err(ModelsFailed::Declined { interactive: false });
    }
    print!("{question} [y/N] ");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    let _ = io::stdin().read_line(&mut answer);
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        Err(ModelsFailed::Declined { interactive: true })
    }
}

/// Downloads `model` beside its place, checks its size and checksum, then
/// moves it in; a failed download leaves nothing behind.
fn fetch(model: &Model, progress: impl Fn(u64)) -> Result<(), ModelsFailed> {
    let url = model.url();
    let failed = |cause: String| ModelsFailed::Download {
        file: model.file,
        url: url.clone(),
        cause,
    };
    let dir = dir()?;
    let target = dir.join(model.file);
    // Each process downloads into a file of its own, so two installs at
    // once can't truncate or publish each other's half-written download.
    let partial = dir.join(format!("{}.{}.partial", model.file, std::process::id()));
    let disk = |path: &PathBuf| {
        let path = path.clone();
        move |cause| ModelsFailed::Disk { path, cause }
    };
    let body = ureq::get(&url)
        .header("user-agent", USER_AGENT)
        .call()
        .map_err(|e| failed(e.to_string()))?
        .into_body();
    let mut reader = body.into_with_config().limit(model.bytes + 1).reader();
    let mut out = fs::File::create(&partial).map_err(disk(&partial))?;
    let mut hash = Sha256::new();
    let mut got = 0u64;
    let mut chunk = [0; 64 * 1024];
    let result = (|| {
        loop {
            let read = reader.read(&mut chunk).map_err(|e| failed(e.to_string()))?;
            if read == 0 {
                break;
            }
            hash.update(&chunk[..read]);
            out.write_all(&chunk[..read]).map_err(disk(&partial))?;
            got += read as u64;
            progress(got);
        }
        let actual = hex(&hash.finalize());
        if got != model.bytes || actual != model.sha256 {
            return Err(ModelsFailed::Corrupt {
                file: model.file,
                expected: model.sha256,
                actual,
            });
        }
        out.sync_all().map_err(disk(&partial))?;
        fs::rename(&partial, &target).map_err(disk(&target))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

/// Has Rex read its config again, so the palette's simulator actions match
/// leaf's config; false when Rex isn't there to ask.
fn reload_rex() -> bool {
    let Ok(rex) = crate::agent::rex_program() else {
        return false;
    };
    Command::new(rex)
        .args(["config", "reload"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_are_sha256_hex() {
        for model in MODELS {
            assert_eq!(model.sha256.len(), 64, "{}", model.file);
            assert!(
                model.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                "{}",
                model.file
            );
        }
    }

    #[test]
    fn a_damaged_model_of_the_right_size_is_missing() {
        let dir = std::env::temp_dir().join(format!("leaf-simmodels-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("model.usdz");
        let bytes = b"leaf model bytes";
        let right = Model {
            file: "model.usdz",
            phones: "test",
            bytes: bytes.len() as u64,
            // `printf 'leaf model bytes' | shasum -a 256`
            sha256: "263bcf23644ae362f22c13b0a08b0f27087d056a2ac5b82c6a72378bc47fc79f",
        };
        fs::write(&path, bytes).expect("write");
        assert!(right.is_whole(&path));
        // Same size, one byte changed.
        let mut damaged = bytes.to_vec();
        damaged[3] ^= 1;
        fs::write(&path, &damaged).expect("write");
        assert!(!right.is_whole(&path));
        // Wrong size.
        fs::write(&path, b"short").expect("write");
        assert!(!right.is_whole(&path));
        assert!(!right.is_whole(&dir.join("absent.usdz")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn urls_are_versioned() {
        assert!(MODELS[0].url().ends_with("/v1/iphone.usdz"));
    }
}

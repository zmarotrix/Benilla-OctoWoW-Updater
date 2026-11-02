//! The folder the launcher looks after, and the one rule it never breaks: it writes only inside
//! that folder, and never into a normal OctoWoW install (`Fresh Install` least of all).
//!
//! ```text
//! <root>/
//!   benilla-launcher(.exe)   this program
//!   benilla(.exe)            the game, put here from a Benilla release; never updated here
//!   OctoWoW/                 the game files benilla reads ($WOW_DATA = OctoWoW/Data)
//!   benilla-config/          the player's state; never touched here
//!   launcher/                the launcher's own state (baseline torrent, resume)
//! ```

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub const GAME_DIR: &str = "OctoWoW";
pub const STATE_DIR: &str = "launcher";
/// Where a player gets Benilla itself, which this updater leaves alone.
pub const BENILLA_RELEASES: &str = "https://github.com/zmarotrix/Benilla-OctoWoW/releases";

/// The install's binaries benilla hashes for the server's version check
/// (`benilla-protocol`'s `INTEGRITY_FILES`); benilla never runs them.
pub const INTEGRITY_FILES: [&str; 5] = [
    "WoW.exe",
    "fmod.dll",
    "ijl15.dll",
    "dbghelp.dll",
    "unicows.dll",
];

/// A folder holding any of these is a player's real client, set up by the OctoWoW Manager or by
/// hand: the launcher reads from one at most, and never writes into it.
const CLIENT_MARKERS: [&str; 4] = [
    "VanillaFixes.exe",
    "WoW_Tweaked.exe",
    "nampower.dll",
    "SuperWoWhook.dll",
];

pub fn game_exe_name() -> &'static str {
    if cfg!(windows) {
        "benilla.exe"
    } else {
        "benilla"
    }
}

/// Files of the OctoWoW release benilla needs: all of `Data`, plus [`INTEGRITY_FILES`]. Plugin
/// DLLs are left out, since benilla has every one of them built in.
pub fn wanted(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.starts_with("data/") || INTEGRITY_FILES.iter().any(|f| f.eq_ignore_ascii_case(&p))
}

#[derive(Clone)]
pub struct Install {
    pub root: PathBuf,
}

impl Install {
    /// `$BENILLA_INSTALL`, else the launcher's own folder.
    pub fn locate() -> Result<Self, String> {
        let root = match std::env::var_os("BENILLA_INSTALL") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(Path::to_path_buf))
                .ok_or("Couldn't tell which folder the launcher is in.")?,
        };
        check_writable(&root)?;
        Ok(Self { root })
    }

    pub fn game_dir(&self) -> PathBuf {
        self.root.join(GAME_DIR)
    }

    pub fn data_dir(&self) -> PathBuf {
        self.game_dir().join("Data")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join(STATE_DIR)
    }

    pub fn game_exe(&self) -> PathBuf {
        self.root.join(game_exe_name())
    }

    /// Starts benilla on this install's game files. The game keeps running after the launcher
    /// exits.
    pub fn launch(&self) -> Result<(), String> {
        let exe = self.game_exe();
        if !exe.is_file() {
            return Err(format!(
                "Benilla isn't in this folder yet. Get it from {BENILLA_RELEASES}"
            ));
        }
        std::process::Command::new(&exe)
            .env("WOW_DATA", self.data_dir())
            .current_dir(&self.root)
            .spawn()
            .map(drop)
            .map_err(|e| format!("Couldn't start Benilla: {e}"))
    }
}

/// Refuses a folder that is, or sits inside, a `Fresh Install` folder or a normal OctoWoW client.
pub fn check_writable(root: &Path) -> Result<(), String> {
    let full = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if full.components().any(|c| {
        c.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case("Fresh Install")
    }) {
        return Err(format!("{} is inside \"Fresh Install\", which the launcher never writes into. Put the launcher in a folder of its own.", root.display()));
    }
    for dir in [full.clone(), full.join(GAME_DIR)] {
        // Our own `OctoWoW` folder holds a WoW.exe too; the root never does.
        if (dir == full && dir.join("WoW.exe").is_file())
            || CLIENT_MARKERS.iter().any(|m| dir.join(m).exists())
        {
            return Err(format!("{} is a normal OctoWoW install. Put the launcher in a folder of its own; it keeps a separate copy of the game files.", dir.display()));
        }
    }
    Ok(())
}

/// The client folder of an install the player points at: the folder itself, or the parent when
/// they picked its `Data` folder.
pub fn client_root_of(picked: &Path) -> Option<PathBuf> {
    if picked.join("Data").is_dir() {
        Some(picked.to_path_buf())
    } else if picked
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case("Data"))
    {
        picked.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

/// Copies `files` (relative `/` paths with their release sizes) from another install into
/// `dest`, skipping any whose size there differs from the release (those get downloaded). The
/// source is only ever opened for reading. Each file is a real copy and never a hard link: a later
/// update writes into these files in place, which through a link would change the source too.
pub fn import(
    source: &Path,
    dest: &Path,
    files: &[(String, u64)],
    progress: &dyn Fn(u64, u64),
    cancel: &AtomicBool,
) -> Result<usize, String> {
    let src_full = std::fs::canonicalize(source)
        .map_err(|e| format!("Couldn't open {}: {e}", source.display()))?;
    let dest_full = std::fs::canonicalize(dest).unwrap_or_else(|_| dest.to_path_buf());
    if src_full == dest_full {
        return Err("That's this install's own game folder.".into());
    }
    let local = |root: &Path, rel: &str| rel.split('/').fold(root.to_path_buf(), |p, c| p.join(c));
    let todo: Vec<&(String, u64)> = files
        .iter()
        .filter(|(rel, len)| {
            std::fs::metadata(local(source, rel)).is_ok_and(|m| m.is_file() && m.len() == *len)
        })
        .filter(|(rel, len)| std::fs::metadata(local(dest, rel)).map_or(true, |m| m.len() != *len))
        .collect();
    let total: u64 = todo.iter().map(|(_, len)| len).sum();
    let mut done = 0u64;
    let mut buf = vec![0u8; 8 << 20];
    for (rel, _) in &todo {
        let to = local(dest, rel);
        if let Some(dir) = to.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
        }
        let part = to.with_extension("part");
        let mut input = std::fs::File::open(local(source, rel))
            .map_err(|e| format!("Couldn't read {rel}: {e}"))?;
        let mut output =
            std::fs::File::create(&part).map_err(|e| format!("Couldn't write {rel}: {e}"))?;
        loop {
            if cancel.load(Ordering::Relaxed) {
                drop(output);
                let _ = std::fs::remove_file(&part);
                return Err("Cancelled.".into());
            }
            let n = input
                .read(&mut buf)
                .map_err(|e| format!("Couldn't read {rel}: {e}"))?;
            if n == 0 {
                break;
            }
            output
                .write_all(&buf[..n])
                .map_err(|e| format!("Couldn't write {rel}: {e}"))?;
            done += n as u64;
            progress(done, total);
        }
        drop(output);
        std::fs::rename(&part, &to).map_err(|e| format!("Couldn't write {rel}: {e}"))?;
    }
    Ok(todo.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("benilla-launcher-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn wanted_files() {
        assert!(wanted("Data/patch-3.mpq"));
        assert!(wanted("Data/Interface/GlueXML/x.xml"));
        assert!(wanted("WoW.exe"));
        assert!(wanted("fmod.dll"));
        assert!(!wanted("nampower.dll"));
        assert!(!wanted("VanillaFixes.exe"));
        assert!(!wanted("realmlist.wtf"));
    }

    #[test]
    fn never_writes_into_fresh_install_or_a_client() {
        assert!(check_writable(Path::new(r"G:\Projects\OctoWoW Benilla\Fresh Install")).is_err());
        assert!(
            check_writable(Path::new(r"G:\Projects\OctoWoW Benilla\fresh install\Data")).is_err()
        );

        let client = temp("client");
        std::fs::write(client.join("WoW.exe"), b"x").unwrap();
        assert!(
            check_writable(&client).is_err(),
            "a folder holding WoW.exe is a client"
        );

        let modded = temp("modded");
        std::fs::create_dir_all(modded.join(GAME_DIR)).unwrap();
        std::fs::write(modded.join(GAME_DIR).join("nampower.dll"), b"x").unwrap();
        assert!(
            check_writable(&modded).is_err(),
            "a game folder with plugin DLLs is a client"
        );

        let ours = temp("ours");
        std::fs::create_dir_all(ours.join(GAME_DIR)).unwrap();
        std::fs::write(ours.join(GAME_DIR).join("WoW.exe"), b"x").unwrap();
        assert!(
            check_writable(&ours).is_ok(),
            "our own OctoWoW folder holds WoW.exe"
        );
        for d in [client, modded, ours] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[test]
    fn import_copies_only_matching_files() {
        let src = temp("import-src");
        let dest = temp("import-dest");
        std::fs::create_dir_all(src.join("Data")).unwrap();
        std::fs::write(src.join("Data/a.mpq"), b"aaaa").unwrap();
        std::fs::write(src.join("Data/b.mpq"), b"bb").unwrap(); // wrong size: left to download
        std::fs::write(src.join("WoW.exe"), b"exe").unwrap();
        let files = vec![
            ("Data/a.mpq".to_string(), 4),
            ("Data/b.mpq".to_string(), 3),
            ("WoW.exe".to_string(), 3),
            ("fmod.dll".to_string(), 1),
        ];
        let n = import(&src, &dest, &files, &|_, _| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(n, 2);
        assert_eq!(std::fs::read(dest.join("Data/a.mpq")).unwrap(), b"aaaa");
        assert!(!dest.join("Data/b.mpq").exists());
        assert!(import(&dest, &dest, &files, &|_, _| {}, &AtomicBool::new(false)).is_err());
        // The source is untouched.
        assert_eq!(std::fs::read(src.join("Data/b.mpq")).unwrap(), b"bb");
        std::fs::remove_dir_all(src).ok();
        std::fs::remove_dir_all(dest).ok();
    }
}

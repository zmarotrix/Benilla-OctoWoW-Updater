//! Game file sync against the official OctoWoW torrent, trimmed from the OctoWoW Manager's
//! updater to the files benilla reads ([`crate::install::wanted`]).
//!
//! * **Check**: fetch the .torrent (a 304 when it's unchanged), compare file sizes on disk, and
//!   diff piece hashes against the last synced release to find files that changed without
//!   changing size. Only ambiguous pieces get hashed locally.
//! * **Repair**: hash every piece on disk to find corruption.
//! * **Update**: download just the files that need it from the release's HTTPS mirror (web seed),
//!   verifying every piece against the torrent's SHA-1 hashes, resuming where a previous attempt
//!   stopped.

use crate::install::wanted;
use crate::torrent::{diff, Delta, Torrent};
use crate::verify::{blame, verify_pieces};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub const TORRENT_URL: &str = "https://dl.octowow.st/download/client.torrent";

/// Archives older clients shipped under names the current one no longer uses.
/// Matched by name *and* exact size so player mods reusing a name are safe.
const LEGACY_ARCHIVES: [(&str, u64); 5] = [
    ("patch-6.mpq", 451_195_806),
    ("patch-7.mpq", 175_256_564),
    ("patch-8.mpq", 484_649_870),
    ("patch-9.mpq", 506_808_141),
    ("patch-a.mpq", 241_751_337),
];

pub struct FileCheck {
    pub path: String,
    pub size: u64,
    pub download: bool,
    pub detail: String,
}

pub struct Plan {
    /// Every release file benilla needs, by index into the torrent's file list.
    pub files: Vec<(usize, FileCheck)>,
    pub download_bytes: u64,
    /// Files to remove: legacy archives, and files of the last synced release that this one
    /// dropped (still at the size that release gave them).
    pub obsolete: Vec<String>,
    /// Most of the game files are missing: a first install, not an update.
    pub not_installed: bool,
    pub release_changed: bool,
}

impl Plan {
    pub fn downloads(&self) -> impl Iterator<Item = &(usize, FileCheck)> {
        self.files.iter().filter(|(_, f)| f.download)
    }

    pub fn up_to_date(&self) -> bool {
        self.downloads().next().is_none() && self.obsolete.is_empty()
    }
}

#[derive(Serialize, Deserialize)]
struct Baseline {
    info_hash: String,
    verified: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct Cache {
    etag: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct ActiveDownload {
    info_hash: String,
    /// Pieces already verified and written (for resuming).
    #[serde(default)]
    done_pieces: Vec<usize>,
}

pub struct Progress {
    pub done: u64,
    pub total: u64,
    pub speed: u64,
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    std::fs::write(
        path,
        serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

/// Sizes in status lines, e.g. "1.9 GB".
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// The launcher's record of the game files: the release last synced, and the cached .torrent.
pub struct GameFiles {
    /// `launcher/` in the install.
    state: PathBuf,
    /// `OctoWoW/` in the install.
    pub root: PathBuf,
    http: reqwest::Client,
}

impl GameFiles {
    pub fn new(state: PathBuf, root: PathBuf) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(crate::USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("http client");
        Self { state, root, http }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.state.join(name)
    }

    /// Downloads the .torrent, reusing the saved copy when the server says it's unchanged.
    pub async fn fetch_release(&self) -> Result<Torrent, String> {
        std::fs::create_dir_all(&self.state).map_err(|e| e.to_string())?;
        let cached = std::fs::read(self.path("release.torrent")).ok();
        let cache: Cache = read_json(&self.path("release.json")).unwrap_or_default();
        let mut req = self.http.get(TORRENT_URL);
        if let (Some(etag), Some(_)) = (&cache.etag, &cached) {
            req = req.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let res = req
            .send()
            .await
            .map_err(|e| format!("Couldn't reach the OctoWoW servers: {e}"))?;
        let bytes =
            if let (reqwest::StatusCode::NOT_MODIFIED, Some(cached)) = (res.status(), cached) {
                cached
            } else if res.status().is_success() {
                let etag = res
                    .headers()
                    .get(reqwest::header::ETAG)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let bytes = res.bytes().await.map_err(|e| e.to_string())?.to_vec();
                std::fs::write(self.path("release.torrent"), &bytes).map_err(|e| e.to_string())?;
                write_json(&self.path("release.json"), &Cache { etag })?;
                bytes
            } else {
                return Err(format!(
                    "The OctoWoW release server answered HTTP {}",
                    res.status()
                ));
            };
        Torrent::parse(&bytes)
    }

    /// The release last synced into the game folder, if any.
    fn baseline(&self) -> Option<Torrent> {
        let b: Baseline = read_json(&self.path("baseline.json"))?;
        let t = Torrent::parse(&std::fs::read(self.path("baseline.torrent")).ok()?).ok()?;
        (t.info_hash == b.info_hash).then_some(t)
    }

    /// Records `release` as what the game folder now holds.
    pub fn save_baseline(&self, verified: bool) -> Result<(), String> {
        let bytes = std::fs::read(self.path("release.torrent")).map_err(|e| e.to_string())?;
        let info_hash = Torrent::parse(&bytes)?.info_hash;
        std::fs::write(self.path("baseline.torrent"), bytes).map_err(|e| e.to_string())?;
        write_json(
            &self.path("baseline.json"),
            &Baseline {
                info_hash,
                verified,
            },
        )
    }

    /// Builds the per-file plan. Blocking (stats files, may hash pieces). `deep` hashes every
    /// file; without a baseline to diff against, only sizes are compared.
    pub fn analyze(
        &self,
        t: &Torrent,
        deep: bool,
        log: &dyn Fn(String),
        progress: &(dyn Fn(u64, u64) + Sync),
        cancel: &AtomicBool,
    ) -> Result<Plan, String> {
        let root = &self.root;
        let baseline = self.baseline();
        let release_changed = baseline
            .as_ref()
            .is_some_and(|b| b.info_hash != t.info_hash);
        let deltas = baseline.as_ref().map(|old| diff(old, t));

        let mut files: Vec<(usize, FileCheck)> = Vec::new();
        // Files whose content must be hashed to decide, and the pieces to hash.
        let mut candidates: HashSet<usize> = HashSet::new();
        let mut pieces: Vec<usize> = Vec::new();
        let mut missing = 0;

        for (i, f) in t.files.iter().enumerate().filter(|(_, f)| wanted(&f.path)) {
            let local_size = std::fs::metadata(t.local_path(root, i))
                .ok()
                .filter(|m| m.is_file())
                .map(|m| m.len());
            let delta = deltas.as_ref().map(|d| &d[i]);
            let mut r = FileCheck {
                path: f.path.clone(),
                size: f.length,
                download: false,
                detail: String::new(),
            };
            match local_size {
                None => {
                    missing += 1;
                    (r.download, r.detail) = (true, "not on disk".into());
                }
                Some(s) if s != f.length => {
                    r.download = true;
                    r.detail = match delta {
                        Some(Delta::Resized { old }) if *old == s => {
                            "updated in this release".into()
                        }
                        _ => format!("size on disk is {}, expected {}", human(s), human(f.length)),
                    };
                }
                Some(_) => match delta {
                    Some(Delta::Changed) => {
                        (r.download, r.detail) = (true, "changed in this release".into())
                    }
                    Some(Delta::Unknown { pieces: p }) if !deep => {
                        candidates.insert(i);
                        pieces.extend(p);
                    }
                    // Already the new size: probably updated by hand or a previous partial sync.
                    Some(Delta::New | Delta::Resized { .. }) if !deep => {
                        candidates.insert(i);
                        pieces.extend(t.pieces_for(i));
                    }
                    _ if deep => {
                        candidates.insert(i);
                        pieces.extend(t.pieces_for(i));
                    }
                    _ => {}
                },
            }
            files.push((i, r));
        }

        if !pieces.is_empty() {
            pieces.sort_unstable();
            pieces.dedup();
            let bytes: u64 = pieces
                .iter()
                .map(|&p| t.piece_span(p).end - t.piece_span(p).start)
                .sum();
            log(format!(
                "Checking {} file(s) ({})…",
                candidates.len(),
                human(bytes)
            ));
            let failed = verify_pieces(t, root, &pieces, progress, cancel);
            if cancel.load(Ordering::Relaxed) {
                return Err("Cancelled.".into());
            }
            // Files benilla doesn't keep are absent, so a piece they share always fails; blame
            // them first so a good neighbour isn't re-downloaded for nothing.
            let known_bad: HashSet<usize> = (0..t.files.len())
                .filter(|i| {
                    !candidates.contains(i) && files.iter().all(|(fi, r)| fi != i || r.download)
                })
                .collect();
            for i in blame(t, &failed, &known_bad) {
                if !candidates.contains(&i) {
                    continue;
                }
                let changed = matches!(
                    deltas.as_ref().map(|d| &d[i]),
                    Some(Delta::Unknown { .. } | Delta::New | Delta::Resized { .. })
                );
                if let Some((_, r)) = files.iter_mut().find(|(fi, _)| *fi == i) {
                    r.download = true;
                    r.detail = if changed {
                        "changed in this release".into()
                    } else {
                        "damaged or modified".into()
                    };
                }
            }
        }

        for (_, r) in files.iter().filter(|(_, r)| r.download) {
            log(format!("{}: {}", r.path, r.detail));
        }

        let in_release: HashSet<String> = t
            .files
            .iter()
            .map(|f| f.path.to_ascii_lowercase())
            .collect();
        let mut obsolete = Vec::new();
        if let Ok(entries) = std::fs::read_dir(root.join("Data")) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let lower = name.to_ascii_lowercase();
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                if !in_release.contains(&format!("data/{lower}"))
                    && LEGACY_ARCHIVES
                        .iter()
                        .any(|(n, s)| *n == lower && *s == size)
                {
                    obsolete.push(format!("Data/{name}"));
                }
            }
        }
        // The game folder is the launcher's own, so what the last release put there and this one
        // dropped goes too, unless the player has since replaced it with something else.
        if let Some(old) = &baseline {
            for (i, f) in old.files.iter().enumerate() {
                if wanted(&f.path)
                    && !in_release.contains(&f.path.to_ascii_lowercase())
                    && std::fs::metadata(old.local_path(root, i)).is_ok_and(|m| m.len() == f.length)
                {
                    obsolete.push(f.path.clone());
                }
            }
        }

        let download_bytes = files
            .iter()
            .filter(|(_, f)| f.download)
            .map(|(_, f)| f.size)
            .sum();
        Ok(Plan {
            not_installed: missing * 2 > files.len(),
            files,
            download_bytes,
            obsolete,
            release_changed,
        })
    }

    /// Downloads every file the plan marks. Returns how many files were fetched.
    pub async fn download(
        &self,
        t: &Torrent,
        plan: &Plan,
        cancel: Arc<AtomicBool>,
        log: &(dyn Fn(String) + Sync),
        progress: &(dyn Fn(Progress) + Sync),
    ) -> Result<usize, String> {
        let root = &self.root;
        std::fs::create_dir_all(root)
            .map_err(|e| format!("Couldn't create the game folder: {e}"))?;
        for o in &plan.obsolete {
            log(format!("Removing {o}, which this release no longer uses"));
            let path = o.split('/').fold(root.clone(), |p, c| p.join(c));
            std::fs::remove_file(&path).map_err(|e| format!("Couldn't remove {o}: {e}"))?;
        }

        let selected: Vec<usize> = plan.downloads().map(|(i, _)| *i).collect();
        if selected.is_empty() {
            return Ok(0);
        }

        // Resume: pieces a previous attempt at this release already verified and wrote, minus any
        // whose file has since gone missing or changed size.
        let active_path = self.path("download.json");
        let intact = |fi: usize| {
            std::fs::metadata(t.local_path(root, fi))
                .map(|m| m.len())
                .ok()
                == Some(t.files[fi].length)
        };
        let wanted_pieces: HashSet<usize> =
            selected.iter().flat_map(|&f| t.pieces_for(f)).collect();
        let already: HashSet<usize> = read_json::<ActiveDownload>(&active_path)
            .filter(|p| p.info_hash == t.info_hash)
            .map(|p| {
                p.done_pieces
                    .into_iter()
                    .filter(|piece| {
                        wanted_pieces.contains(piece)
                            && t.files_in(t.piece_span(*piece))
                                .filter(|f| selected.contains(f))
                                .all(intact)
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !already.is_empty() {
            log(format!(
                "Resuming: {} of {} pieces were already downloaded",
                already.len(),
                wanted_pieces.len()
            ));
        }

        let total: u64 = selected.iter().map(|&i| t.files[i].length).sum();
        log(format!(
            "Downloading {} file(s), {}, from the OctoWoW mirror",
            selected.len(),
            human(total)
        ));
        let checkpoint = |done: &HashSet<usize>| {
            let _ = write_json(
                &active_path,
                &ActiveDownload {
                    info_hash: t.info_hash.clone(),
                    done_pieces: done.iter().copied().collect(),
                },
            );
        };
        let report = |p: crate::webseed::Progress| {
            let done = selected
                .iter()
                .zip(&p.done)
                .map(|(&fi, &d)| d.min(t.files[fi].length))
                .sum();
            progress(Progress {
                done,
                total,
                speed: p.speed,
            });
        };
        crate::webseed::download(t, root, &selected, already, cancel, &report, &checkpoint).await?;

        let short: Vec<&str> = selected
            .iter()
            .filter(|&&i| !intact(i))
            .map(|&i| t.files[i].path.as_str())
            .collect();
        let _ = std::fs::remove_file(&active_path);
        if !short.is_empty() {
            return Err(format!(
                "The download finished but {} file(s) are incomplete: {}",
                short.len(),
                short.join(", ")
            ));
        }
        Ok(selected.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_sizes() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(2_060_279_959), "1.9 GB");
    }

    // Live test against the real OctoWoW release. Run with:
    //   cargo test -p benilla-launcher live_ -- --ignored --nocapture
    // With OCTOWOW_TEST_CLIENT=<an install> it also compares the download with that install's
    // copies (read only).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn live_download_integrity_files() {
        use crate::install::INTEGRITY_FILES;
        let base =
            std::env::temp_dir().join(format!("benilla-launcher-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let files = GameFiles::new(base.join("launcher"), base.join("OctoWoW"));
        let t = files.fetch_release().await.unwrap();
        let cancel = AtomicBool::new(false);
        let mut plan = files
            .analyze(&t, false, &|m| println!("{m}"), &|_, _| {}, &cancel)
            .unwrap();
        assert!(plan.not_installed);
        assert!(
            plan.files.iter().all(|(_, f)| wanted(&f.path)),
            "no plugin DLLs in the plan"
        );
        println!(
            "full install: {} files, {}",
            plan.files.len(),
            human(plan.download_bytes)
        );
        for (_, f) in plan.files.iter_mut() {
            f.download = INTEGRITY_FILES
                .iter()
                .any(|n| n.eq_ignore_ascii_case(&f.path));
        }
        let got = files
            .download(
                &t,
                &plan,
                Arc::new(AtomicBool::new(false)),
                &|m| println!("{m}"),
                &|_| {},
            )
            .await
            .unwrap();
        assert_eq!(got, INTEGRITY_FILES.len());
        // Nothing else was written: neighbouring files in shared pieces are only hashed.
        let on_disk: Vec<_> = std::fs::read_dir(base.join("OctoWoW"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(on_disk.len(), INTEGRITY_FILES.len(), "{on_disk:?}");
        if let Ok(client) = std::env::var("OCTOWOW_TEST_CLIENT") {
            for name in INTEGRITY_FILES {
                let same = std::fs::read(base.join("OctoWoW").join(name)).unwrap()
                    == std::fs::read(Path::new(&client).join(name)).unwrap();
                println!(
                    "{name}: {}",
                    if same {
                        "same as the install"
                    } else {
                        "DIFFERS from the install"
                    }
                );
            }
        }
        // The second fetch is answered from the saved copy.
        assert_eq!(files.fetch_release().await.unwrap().info_hash, t.info_hash);
        std::fs::remove_dir_all(&base).ok();
    }
}

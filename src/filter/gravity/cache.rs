//! Platten-Cache: pro Quelle eine Liste plus Validatoren, dazu das Alterslimit.
//!
//! Der Cache ist nie kritisch: fehlt er oder ist er nicht schreibbar, läuft der Lauf
//! weiter und nur die Persistenz fehlt.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::fs::File;
use xxhash_rust::xxh3::xxh3_64;

/// Validatoren einer HTTP-Antwort für Conditional GET.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Validators {
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
}

impl Validators {
    pub(super) fn is_empty(&self) -> bool {
        self.etag.is_none() && self.last_modified.is_none()
    }

    pub(super) fn serialize(&self, url: &str) -> String {
        let mut out = String::with_capacity(128);
        out.push_str("url\t");
        out.push_str(&hex_encode(url));
        out.push('\n');
        if let Some(etag) = &self.etag {
            out.push_str("etag\t");
            out.push_str(&hex_encode(etag));
            out.push('\n');
        }
        if let Some(last_modified) = &self.last_modified {
            out.push_str("last-modified\t");
            out.push_str(&hex_encode(last_modified));
            out.push('\n');
        }
        out
    }

    /// `None`, wenn das Meta-File zu einer anderen URL gehört oder unlesbar ist.
    pub(super) fn parse(text: &str, url: &str) -> Option<Self> {
        let mut stored_url = None;
        let mut etag = None;
        let mut last_modified = None;

        for line in text.lines() {
            let (key, value) = line.split_once('\t')?;
            let value = hex_decode(value)?;
            match key {
                "url" => stored_url = Some(value),
                "etag" => etag = Some(value),
                "last-modified" => last_modified = Some(value),
                _ => {}
            }
        }

        if stored_url.as_deref() != Some(url) {
            return None;
        }
        if etag.is_none() && last_modified.is_none() {
            return None;
        }
        Some(Self {
            etag,
            last_modified,
        })
    }
}

fn hex_encode(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len() * 2);
    for byte in value.as_bytes() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn hex_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        decoded.push((high * 16 + low) as u8);
    }
    String::from_utf8(decoded).ok()
}

/// Zustand der Cache-Liste einer URL.
pub(super) enum CachedList {
    /// Existiert und ist jünger als das Maximalalter.
    Fresh(File),
    /// Existiert, ist aber zu alt: wird weder geladen noch revalidiert.
    Stale { age: Duration },
    /// Existiert nicht (oder Persistenz ist abgeschaltet).
    Missing,
}

/// Platten-Cache: pro URL eine Liste und ein Meta-File mit den Validatoren.
pub(super) struct GravityCache {
    dir: Option<PathBuf>,
    max_age: Option<Duration>,
}

impl GravityCache {
    pub(super) fn new(dir: Option<PathBuf>, max_age: Option<Duration>) -> Self {
        Self { dir, max_age }
    }

    /// `(list_path, meta_path)`; `None`, wenn Persistenz abgeschaltet ist.
    pub(super) fn paths(&self, url: &str) -> Option<(PathBuf, PathBuf)> {
        let dir = self.dir.as_ref()?;
        let hash = cache_hash(url);
        Some((
            dir.join(format!("{hash}.list")),
            dir.join(format!("{hash}.meta")),
        ))
    }

    pub(super) async fn ensure_dir(&self) -> io::Result<()> {
        if let Some(dir) = &self.dir {
            tokio::fs::create_dir_all(dir).await?;
        }
        Ok(())
    }

    pub(super) async fn validators(&self, url: &str) -> Option<Validators> {
        let (_, meta_path) = self.paths(url)?;
        let text = tokio::fs::read_to_string(&meta_path).await.ok()?;
        Validators::parse(&text, url)
    }

    pub(super) async fn store_validators(
        &self,
        url: &str,
        validators: &Validators,
    ) -> io::Result<()> {
        let Some((_, meta_path)) = self.paths(url) else {
            return Ok(());
        };
        self.ensure_dir().await?;
        let tmp = meta_path.with_extension("meta.tmp");
        tokio::fs::write(&tmp, validators.serialize(url)).await?;
        tokio::fs::rename(&tmp, &meta_path).await
    }

    /// Öffnet die Cache-Liste und prüft ihr Alter anhand der mtime.
    pub(super) async fn open_list(&self, url: &str) -> CachedList {
        let Some((list_path, _)) = self.paths(url) else {
            return CachedList::Missing;
        };
        let Ok(file) = File::open(&list_path).await else {
            return CachedList::Missing;
        };
        let age = match file
            .metadata()
            .await
            .ok()
            .and_then(|metadata| metadata.modified().ok())
        {
            Some(modified) => SystemTime::now()
                .duration_since(modified)
                .unwrap_or(Duration::ZERO),
            None => Duration::ZERO,
        };
        match self.max_age {
            Some(max_age) if age > max_age => CachedList::Stale { age },
            _ => CachedList::Fresh(file),
        }
    }

    /// Entfernt Cache-Dateien, die zu keiner konfigurierten URL mehr gehören, und
    /// liegengebliebene `.tmp`-Reste abgebrochener Läufe.
    pub(super) async fn prune(&self, urls: &[String]) -> io::Result<usize> {
        let Some(dir) = self.dir.as_ref() else {
            return Ok(0);
        };
        let known: HashSet<String> = urls.iter().map(|url| cache_hash(url)).collect();
        let mut entries = match tokio::fs::read_dir(dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(err) => return Err(err),
        };

        let mut removed = 0usize;
        while let Some(entry) = entries.next_entry().await? {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            let Some(hash) = cache_hash_from_file_name(name) else {
                // Keine Datei von uns – Finger weg.
                continue;
            };
            let orphan = !known.contains(hash);
            if !orphan && !name.ends_with(".tmp") {
                continue;
            }
            if tokio::fs::remove_file(entry.path()).await.is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Legt eine temporäre Datei an; erst nach erfolgreichem Lesen wird sie committet.
    pub(super) async fn create_temp(
        &self,
        url: &str,
    ) -> io::Result<Option<(File, PathBuf, PathBuf)>> {
        let Some((list_path, _)) = self.paths(url) else {
            return Ok(None);
        };
        self.ensure_dir().await?;
        let tmp = list_path.with_extension("list.tmp");
        let file = File::create(&tmp).await?;
        Ok(Some((file, tmp, list_path)))
    }

    pub(super) async fn commit(&self, tmp: &Path, final_path: &Path) -> io::Result<()> {
        tokio::fs::rename(tmp, final_path).await
    }
}

fn cache_hash(url: &str) -> String {
    format!("{:016x}", xxh3_64(url.as_bytes()))
}

/// Erkennt `<16 hex>.list`, `.meta` sowie deren `.list.tmp` / `.meta.tmp`-Varianten.
fn cache_hash_from_file_name(name: &str) -> Option<&str> {
    let stem = name
        .strip_suffix(".list")
        .or_else(|| name.strip_suffix(".meta"))
        .or_else(|| name.strip_suffix(".list.tmp"))
        .or_else(|| name.strip_suffix(".meta.tmp"))?;
    if stem.len() == 16 && stem.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(stem)
    } else {
        None
    }
}

pub(super) fn fmt_age(age: Duration) -> String {
    let seconds = age.as_secs();
    if seconds < 3600 {
        format!("{} min", seconds / 60)
    } else if seconds < 48 * 3600 {
        format!("{:.1} h", age.as_secs_f64() / 3600.0)
    } else {
        format!("{:.1} tage", age.as_secs_f64() / 86_400.0)
    }
}

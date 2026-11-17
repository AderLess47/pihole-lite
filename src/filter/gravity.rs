//! Gravity-Updater: Blocklisten streamen, cachen und den Filterbestand atomar tauschen.
//!
//! Ablauf eines Updates:
//! 1. Ein frisches Backend desselben Tiers aufbauen (das alte bleibt währenddessen aktiv).
//! 2. Jede Quelle über `AsyncBufRead` zeilenweise lesen, Parser direkt in das Backend
//!    füttern – die Datei liegt nie komplett im Speicher.
//! 3. `seal` und erst dann per [`FilterEngine::replace_backend`] austauschen.
//!
//! Preload und Update bauen den Bestand **vollständig** neu auf: lokale Dateien,
//! Inline-Einträge und alle Remote-Listen. Sonst würde der Tausch lokal geladene Domains
//! stillschweigend verlieren. Beide laufen deshalb durch denselben `Rebuild`; sie
//! unterscheiden sich nur darin, woher der Remote-Teil kommt (`Mode::Offline` liest den
//! Platten-Cache, `Mode::Refresh` lädt) und was ein leerer Bestand bedeutet.
//!
//! # Platten-Cache und Conditional GET
//!
//! Jede Remote-Quelle hat im Cache-Verzeichnis eine eigene Liste (`<hash>.list`) plus
//! Validatoren (`<hash>.meta`). Beim Update schickt `http://` daher `If-None-Match` /
//! `If-Modified-Since`: antwortet der Server mit `304`, wird die vorhandene Liste
//! verwendet statt neu geladen zu werden. Beim Start baut [`GravityUpdater::preload`]
//! den Bestand ohne einen einzigen Netzwerkzugriff aus lokalen Dateien und diesen Caches
//! auf – der Listener filtert also sofort, auch wenn der erste Download noch läuft.
//!
//! Der externe Fetcher (`https://`) bekommt bewusst keinen Conditional Request: ohne
//! Statuszeile könnten wir eine leere `304`-Antwort nicht von einer leeren Liste
//! unterscheiden und würden den Cache löschen. Er schreibt den Cache trotzdem.
//!
//! # Transport
//!
//! `http://` wird nativ über `TcpStream` gelesen (HTTP/1.0 + `Connection: close`, damit
//! der Body mit dem Socket endet und kein Chunked-Parser nötig ist). `https://` geht durch
//! einen externen Fetcher (Default: `curl -fsSL`), dessen stdout über denselben
//! Streaming-Parser läuft – TLS-Stacks sind auf ARMv6/256 MB nicht vertretbar, und der
//! Download ist ein periodischer Job, kein Hot Path.
//!
//! # Modulschnitt
//!
//! `gravity.rs` hält Config, Fehler und den Updater; alles andere liegt in
//! Untermodulen: `update` (Quellen öffnen und streamen), `rebuild` (der gemeinsame
//! Neuaufbau), `stream` (Zeilenparser), `source` (offene Quelle + Cache-Senke), `cache`
//! (Platten-Cache, Validatoren) und `http` (Request, Statuszeile, URL).

mod cache;
mod http;
mod rebuild;
mod source;
mod stream;
#[cfg(test)]
mod tests;
mod update;

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use self::cache::GravityCache;
use self::rebuild::Mode;
use super::engine::FilterEngine;

/// Längste Zeile, die als Blocklisten-Eintrag akzeptiert wird. Alles darüber wird
/// verworfen, statt den Zeilenpuffer wachsen zu lassen.
const MAX_LINE_BYTES: usize = 512;
/// Timeout für den TCP-Connect; der Gesamt-Timeout je Quelle kommt aus der Config.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Maximal akzeptierte Header-Größe einer HTTP-Antwort.
const MAX_HEADER_BYTES: usize = 8 * 1024;
/// Standard-Fetcher für HTTPS-Quellen. `{url}` kann im Template stehen, sonst wird die
/// URL angehängt.
pub const DEFAULT_FETCHER: &str = "curl -fsSL --max-time 120";

/// Einstellungen des Updaters.
#[derive(Debug, Clone)]
pub struct GravityConfig {
    /// Blocklisten-URLs (`http://` oder `https://`).
    pub urls: Vec<String>,
    /// Lokale Blocklisten, die bei jedem Update mit aufgebaut werden.
    pub local_files: Vec<PathBuf>,
    /// Einzelne Domains aus der Config, die den Tausch ebenfalls überleben müssen.
    pub inline: Vec<String>,
    /// Externer Fetcher für `https://`; `None` nutzt [`DEFAULT_FETCHER`].
    pub fetcher: Option<String>,
    /// Verzeichnis für Listen + Validatoren; `None` schaltet die Persistenz ab.
    pub cache_dir: Option<PathBuf>,
    /// Maximalalter einer Cache-Liste; ältere werden weder geladen noch revalidiert.
    /// `None` = unbegrenzt.
    pub cache_max_age: Option<Duration>,
    /// Intervall zwischen Updates; `0` = nur einmal beim Start.
    pub interval: Duration,
    /// Gesamt-Timeout je Quelle.
    pub source_timeout: Duration,
    /// Pro-Zeile-Logging.
    pub verbose: bool,
}

/// Ergebnis eines erfolgreichen Updates oder Preloads.
#[derive(Debug, Clone, Copy)]
pub struct UpdateReport {
    /// Eindeutige Domains im neuen Bestand.
    pub domains: usize,
    /// Gelesene Zeilen insgesamt.
    pub lines: u64,
    /// Übersprungene Zeilen (Kommentare, Boilerplate, Parserfehler).
    pub skipped: u64,
    /// Anzahl Quellen.
    pub sources: usize,
    /// Laufzeit.
    pub duration: Duration,
    /// Heap-Schätzung des neuen Backends.
    pub heap_bytes: usize,
}

/// Fehler eines Update-Laufs.
#[derive(Debug)]
pub enum GravityError {
    /// Keine Quellen konfiguriert.
    NoSources,
    /// Scheme wird nicht unterstützt (nur `http://` und `https://`).
    UnsupportedScheme(String),
    /// URL ist syntaktisch kaputt.
    InvalidUrl(String),
    /// Quelle hat das Zeitlimit überschritten.
    Timeout(String),
    /// HTTP- oder Fetcher-Fehler.
    Http { url: String, reason: String },
    /// Lokale Datei oder Cache nicht lesbar.
    Local { path: String, reason: String },
    /// I/O-Fehler beim Lesen.
    Io(io::Error),
    /// Kein einziger Eintrag übrig – dann lieber den alten Stand behalten.
    Empty,
}

impl fmt::Display for GravityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSources => write!(f, "keine Blocklisten-URL konfiguriert"),
            Self::UnsupportedScheme(url) => {
                write!(f, "nicht unterstütztes URL-Scheme: {url}")
            }
            Self::InvalidUrl(url) => write!(f, "ungültige URL: {url}"),
            Self::Timeout(url) => write!(f, "timeout beim lesen von {url}"),
            Self::Http { url, reason } => write!(f, "{url}: {reason}"),
            Self::Local { path, reason } => write!(f, "{path}: {reason}"),
            Self::Io(err) => write!(f, "lesefehler: {err}"),
            Self::Empty => write!(f, "quellen lieferten keine einzige domain"),
        }
    }
}

impl std::error::Error for GravityError {}

impl From<io::Error> for GravityError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Der Updater selbst: hält Config und Platten-Cache, der Bestand liegt in der Engine.
pub struct GravityUpdater {
    config: GravityConfig,
    cache: GravityCache,
}

impl GravityUpdater {
    /// Erzeugt den Updater.
    pub fn new(config: GravityConfig) -> Self {
        let cache = GravityCache::new(config.cache_dir.clone(), config.cache_max_age);
        Self { config, cache }
    }

    /// `true`, wenn Remote-Quellen konfiguriert sind (dann läuft der Hintergrund-Task).
    pub fn is_enabled(&self) -> bool {
        !self.config.urls.is_empty()
    }

    /// Entfernt Cache-Dateien, die zu keiner konfigurierten URL mehr gehören, und
    /// liegengebliebene `.tmp`-Reste.
    async fn prune_cache(&self) -> io::Result<usize> {
        self.cache.prune(&self.config.urls).await
    }

    /// Baut den Bestand ohne Netzwerkzugriff auf: lokale Dateien, Inline-Domains und
    /// vorhandene Caches.
    ///
    /// Wird vor dem Öffnen des DNS-Sockets aufgerufen, damit ein Neustart sofort filtert.
    pub async fn preload(&self, engine: &FilterEngine) -> Result<UpdateReport, GravityError> {
        self.rebuild(engine, Mode::Offline).await
    }

    /// Ein Update-Lauf: alle Quellen in ein frisches Backend streamen und tauschen.
    pub async fn update(&self, engine: &FilterEngine) -> Result<UpdateReport, GravityError> {
        if self.config.urls.is_empty() {
            return Err(GravityError::NoSources);
        }
        self.rebuild(engine, Mode::Refresh).await
    }

    /// Hintergrund-Task: erstes Update sofort, danach im konfigurierten Intervall.
    pub async fn run(self: Arc<Self>, engine: Arc<FilterEngine>) {
        loop {
            match self.update(&engine).await {
                Ok(report) => println!(
                    "[gravity] {} domains aus {} quelle(n), {} zeilen ({} übersprungen) in {} ms, ~{} heap",
                    report.domains,
                    report.sources,
                    report.lines,
                    report.skipped,
                    report.duration.as_millis(),
                    crate::fmt_bytes(report.heap_bytes as u64)
                ),
                Err(err) => eprintln!(
                    "[gravity] update fehlgeschlagen: {err} (vorheriger stand bleibt aktiv)"
                ),
            }

            if self.config.interval.is_zero() {
                return;
            }
            tokio::time::sleep(self.config.interval).await;
        }
    }
}

//! Gravity-Updater: Blocklisten streamen, cachen und den Filterbestand atomar tauschen.
//!
//! Ablauf eines Updates:
//! 1. Ein frisches Backend desselben Tiers aufbauen (das alte bleibt währenddessen aktiv).
//! 2. Jede Quelle über [`AsyncBufRead`] zeilenweise lesen, Parser direkt in das Backend
//!    füttern – die Datei liegt nie komplett im Speicher.
//! 3. `seal` und erst dann per [`FilterEngine::replace_backend`] austauschen.
//!
//! Ein Update baut den Bestand **vollständig** neu auf: lokale Dateien, Inline-Einträge
//! und alle Remote-Listen. Sonst würde der Tausch lokal geladene Domains stillschweigend
//! verlieren.
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
//! `http://` wird nativ über [`TcpStream`] gelesen (HTTP/1.0 + `Connection: close`, damit
//! der Body mit dem Socket endet und kein Chunked-Parser nötig ist). `https://` geht durch
//! einen externen Fetcher (Default: `curl -fsSL`), dessen stdout über denselben
//! Streaming-Parser läuft – TLS-Stacks sind auf ARMv6/256 MB nicht vertretbar, und der
//! Download ist ein periodischer Job, kein Hot Path.

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::fs::File;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdout, Command};
use tokio::time::timeout;
use xxhash_rust::xxh3::xxh3_64;

use super::engine::{backend_for_tier, blocklist_domain_from_line, FilterBackend, FilterEngine};

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

    /// Baut den Bestand ohne Netzwerkzugriff auf: lokale Dateien plus vorhandene Caches.
    ///
    /// Wird vor dem Öffnen des DNS-Sockets aufgerufen, damit ein Neustart sofort filtert.
    pub async fn preload(&self, engine: &FilterEngine) -> Result<UpdateReport, GravityError> {
        let started = Instant::now();
        let mut backend = backend_for_tier(engine.tier());
        let mut totals = Totals::default();
        let mut sources = 0usize;

        for path in &self.config.local_files {
            let label = path.display().to_string();
            let mut source = self.open_local(path).await?;
            let mut sink = CacheSink::disabled();
            self.timed_read(
                source.reader(),
                backend.as_mut(),
                &mut totals,
                &label,
                &mut sink,
            )
            .await?;
            source.finish(&label).await?;
            sources += 1;
        }

        for url in &self.config.urls {
            let file = match self.cache.open_list(url).await {
                CachedList::Fresh(file) => file,
                CachedList::Stale { age } => {
                    eprintln!(
                        "[gravity] {url}: cache ist {} alt und wird nicht geladen",
                        fmt_age(age)
                    );
                    continue;
                }
                CachedList::Missing => {
                    if self.config.verbose {
                        eprintln!("[gravity] {url}: noch kein cache vorhanden");
                    }
                    continue;
                }
            };
            let mut reader = BufReader::new(file);
            let mut sink = CacheSink::disabled();
            self.timed_read(&mut reader, backend.as_mut(), &mut totals, url, &mut sink)
                .await?;
            sources += 1;
        }

        backend.seal();
        let domains = backend.len();
        let heap_bytes = backend.heap_bytes();
        if domains > 0 {
            engine.replace_backend(backend);
        }

        Ok(UpdateReport {
            domains,
            lines: totals.lines,
            skipped: totals.skipped,
            sources,
            duration: started.elapsed(),
            heap_bytes,
        })
    }

    /// Ein Update-Lauf: alle Quellen in ein frisches Backend streamen und tauschen.
    pub async fn update(&self, engine: &FilterEngine) -> Result<UpdateReport, GravityError> {
        if self.config.urls.is_empty() {
            return Err(GravityError::NoSources);
        }

        let started = Instant::now();
        let mut backend = backend_for_tier(engine.tier());
        let mut totals = Totals::default();

        // 1. Lokale Dateien zuerst: sie sind der Ausgangsbestand, den ein fehlgeschlagener
        //    Download nicht beschädigen darf.
        for path in &self.config.local_files {
            let label = path.display().to_string();
            let mut source = self.open_local(path).await?;
            let mut sink = CacheSink::disabled();
            self.timed_read(
                source.reader(),
                backend.as_mut(),
                &mut totals,
                &label,
                &mut sink,
            )
            .await?;
            source.finish(&label).await?;
        }

        // 2. Remote-Quellen: 304 -> Platten-Cache, 2xx -> Body (und Cache erneuern).
        for url in &self.config.urls {
            self.refresh_source(url, backend.as_mut(), &mut totals)
                .await?;
        }

        // 3. Inline-Domains aus der Config überleben den Tausch mit.
        for domain in &self.config.inline {
            backend.add(domain);
        }

        backend.seal();
        let domains = backend.len();
        if domains == 0 {
            return Err(GravityError::Empty);
        }
        let heap_bytes = backend.heap_bytes();
        engine.replace_backend(backend);

        // Erst nach erfolgreichem Tausch aufräumen: bis dahin ist der alte Bestand die
        // Rückfalllinie und seine Dateien dürfen nicht verschwinden.
        match self.prune_cache().await {
            Ok(0) => {}
            Ok(removed) => println!("[gravity] {removed} verwaiste cache-datei(en) entfernt"),
            Err(err) => eprintln!("[gravity] cache-aufräumen fehlgeschlagen: {err}"),
        }

        Ok(UpdateReport {
            domains,
            lines: totals.lines,
            skipped: totals.skipped,
            sources: self.config.urls.len() + self.config.local_files.len(),
            duration: started.elapsed(),
            heap_bytes,
        })
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

    /// Holt eine Remote-Quelle und schreibt sie ins Backend (und in den Cache).
    async fn refresh_source(
        &self,
        url: &str,
        backend: &mut dyn FilterBackend,
        totals: &mut Totals,
    ) -> Result<(), GravityError> {
        if url.starts_with("http://") {
            let cached = self.cache.open_list(url).await;
            // Nur mit einer frischen Cache-Liste revalidieren. Ist sie zu alt, gehen
            // keine Conditional Header raus und der Server liefert den vollen Body.
            let validators = match &cached {
                CachedList::Fresh(_) => self.cache.validators(url).await,
                CachedList::Stale { age } => {
                    eprintln!(
                        "[gravity] {url}: cache ist {} alt, wird vollständig neu geladen",
                        fmt_age(*age)
                    );
                    None
                }
                CachedList::Missing => None,
            };
            let (mut reader, head) = open_http(url, validators.as_ref()).await?;

            if head.status == 304 {
                let CachedList::Fresh(file) = cached else {
                    return Err(GravityError::Http {
                        url: url.to_owned(),
                        reason: "304 ohne frischen cache".to_owned(),
                    });
                };
                let mut cached = BufReader::new(file);
                let mut sink = CacheSink::disabled();
                self.timed_read(&mut cached, backend, totals, url, &mut sink)
                    .await?;

                let fresh = head.validators();
                if !fresh.is_empty() {
                    if let Err(err) = self.cache.store_validators(url, &fresh).await {
                        eprintln!("[gravity] {url}: validatoren nicht speicherbar: {err}");
                    }
                }
                if self.config.verbose {
                    eprintln!("[gravity] {url}: 304 not modified, cache verwendet");
                }
                return Ok(());
            }

            if !(200..300).contains(&head.status) {
                return Err(GravityError::Http {
                    url: url.to_owned(),
                    reason: format!("HTTP status {}", head.status),
                });
            }

            self.stream_into_backend(&mut reader, backend, totals, url)
                .await?;

            let fresh = head.validators();
            if !fresh.is_empty() {
                if let Err(err) = self.cache.store_validators(url, &fresh).await {
                    eprintln!("[gravity] {url}: validatoren nicht speicherbar: {err}");
                }
            }
            return Ok(());
        }

        if url.starts_with("https://") {
            // Bewusst ohne Conditional Request: der Fetcher liefert keine Statuszeile,
            // eine leere 304-Antwort wäre sonst von einer leeren Liste nicht zu trennen.
            let mut source = self.open_via_fetcher(url).await?;
            self.stream_into_backend(source.reader(), backend, totals, url)
                .await?;
            source.finish(url).await?;
            return Ok(());
        }

        Err(GravityError::UnsupportedScheme(url.to_owned()))
    }

    /// Streamt eine geöffnete Quelle ins Backend und spiegelt sie in den Cache.
    ///
    /// Cache-Fehler (voller Datenträger, keine Rechte) sind nicht fatal: die Domainliste
    /// im RAM ist dann trotzdem vollständig, nur die Persistenz fehlt.
    async fn stream_into_backend(
        &self,
        reader: &mut (dyn AsyncBufRead + Unpin + Send),
        backend: &mut dyn FilterBackend,
        totals: &mut Totals,
        url: &str,
    ) -> Result<(), GravityError> {
        match self.cache.create_temp(url).await {
            Ok(Some((mut file, tmp, final_path))) => {
                // Der Sink borgt `file`; Block-Ende gibt die Borrow frei, bevor die Datei
                // geflusht und umbenannt wird.
                let (read, cache_failed) = {
                    let mut sink = CacheSink {
                        file: Some(&mut file),
                        failed: false,
                    };
                    let read = self
                        .timed_read(reader, backend, totals, url, &mut sink)
                        .await;
                    (read, sink.failed)
                };

                if let Err(err) = read {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Err(err);
                }

                let synced = async {
                    file.flush().await?;
                    file.sync_all().await?;
                    Ok::<(), io::Error>(())
                }
                .await;
                drop(file);

                if cache_failed {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Ok(());
                }
                if let Err(err) = synced {
                    eprintln!("[gravity] {url}: cache nicht schreibbar: {err}");
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Ok(());
                }
                if let Err(err) = self.cache.commit(&tmp, &final_path).await {
                    eprintln!("[gravity] {url}: cache nicht austauschbar: {err}");
                    let _ = tokio::fs::remove_file(&tmp).await;
                }
                Ok(())
            }
            Ok(None) => {
                let mut sink = CacheSink::disabled();
                self.timed_read(reader, backend, totals, url, &mut sink)
                    .await
            }
            Err(err) => {
                eprintln!("[gravity] {url}: cache nicht nutzbar ({err}), update läuft ohne cache");
                let mut sink = CacheSink::disabled();
                self.timed_read(reader, backend, totals, url, &mut sink)
                    .await
            }
        }
    }

    /// Liest eine bereits geöffnete Quelle mit Gesamt-Timeout.
    async fn timed_read(
        &self,
        reader: &mut (dyn AsyncBufRead + Unpin + Send),
        backend: &mut dyn FilterBackend,
        totals: &mut Totals,
        label: &str,
        sink: &mut CacheSink<'_>,
    ) -> Result<(), GravityError> {
        match timeout(
            self.config.source_timeout,
            consume_source(reader, backend, totals, sink),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(GravityError::Io(err)),
            Err(_) => Err(GravityError::Timeout(label.to_owned())),
        }
    }

    /// Öffnet eine lokale Blockliste.
    async fn open_local(&self, path: &Path) -> Result<Source, GravityError> {
        let file = File::open(path).await.map_err(|err| GravityError::Local {
            path: path.display().to_string(),
            reason: err.to_string(),
        })?;
        Ok(Source::File(BufReader::new(file)))
    }

    /// HTTPS über einen externen Fetcher; stdout wird gestreamt, nicht gesammelt.
    async fn open_via_fetcher(&self, url: &str) -> Result<Source, GravityError> {
        let template = self.config.fetcher.as_deref().unwrap_or(DEFAULT_FETCHER);
        let mut parts = template.split_whitespace();
        let program = parts.next().ok_or_else(|| GravityError::Http {
            url: url.to_owned(),
            reason: "leerer fetcher-befehl".to_owned(),
        })?;

        let mut command = Command::new(program);
        let mut substituted = false;
        for argument in parts {
            if argument.contains("{url}") {
                command.arg(argument.replace("{url}", url));
                substituted = true;
            } else {
                command.arg(argument);
            }
        }
        if !substituted {
            command.arg(url);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|err| GravityError::Http {
            url: url.to_owned(),
            reason: format!("fetcher `{program}` konnte nicht gestartet werden: {err}"),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| GravityError::Http {
            url: url.to_owned(),
            reason: "fetcher lieferte kein stdout".to_owned(),
        })?;

        Ok(Source::Fetcher {
            reader: BufReader::new(stdout),
            child,
        })
    }
}

/// Offene Quelle: lokale Datei oder Fetcher-Prozess.
///
/// HTTP-Quellen werden direkt in [`GravityUpdater::refresh_source`] gelesen, weil dort
/// Statuscode und Validatoren gebraucht werden.
enum Source {
    File(BufReader<File>),
    Fetcher {
        reader: BufReader<ChildStdout>,
        child: Child,
    },
}

impl Source {
    fn reader(&mut self) -> &mut (dyn AsyncBufRead + Unpin + Send) {
        match self {
            Self::File(reader) => reader,
            Self::Fetcher { reader, .. } => reader,
        }
    }

    /// Schließt die Quelle ab und prüft bei einem Fetcher den Exit-Status.
    async fn finish(self, url: &str) -> Result<(), GravityError> {
        match self {
            Self::File(_) => Ok(()),
            Self::Fetcher { mut child, .. } => {
                let status = child.wait().await.map_err(GravityError::Io)?;
                if !status.success() {
                    return Err(GravityError::Http {
                        url: url.to_owned(),
                        reason: format!("fetcher beendet mit {status}"),
                    });
                }
                Ok(())
            }
        }
    }
}

/// Senke für die Cache-Datei: schreibt akzeptierte Domains mit, ohne den Update-Lauf
/// an einen Schreibfehler zu binden.
struct CacheSink<'a> {
    file: Option<&'a mut File>,
    failed: bool,
}

impl<'a> CacheSink<'a> {
    fn disabled() -> Self {
        Self {
            file: None,
            failed: false,
        }
    }

    async fn write_bytes(&mut self, bytes: &[u8]) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if let Err(err) = file.write_all(bytes).await {
            eprintln!("[gravity] cache-schreibfehler: {err}; update läuft ohne cache weiter");
            self.failed = true;
            self.file = None;
        }
    }
}

#[derive(Debug, Default)]
struct Totals {
    lines: u64,
    accepted: u64,
    skipped: u64,
}

/// Liest den Stream zeilenweise und füttert das Backend.
///
/// Nutzt `fill_buf`/`consume`, damit auch sehr lange Zeilen den Speicher nicht sprengen:
/// Zeilen über [`MAX_LINE_BYTES`] werden verworfen, statt den Puffer zu vergrößern.
async fn consume_source(
    reader: &mut (dyn AsyncBufRead + Unpin + Send),
    backend: &mut dyn FilterBackend,
    totals: &mut Totals,
    sink: &mut CacheSink<'_>,
) -> io::Result<()> {
    let mut line: Vec<u8> = Vec::with_capacity(128);
    // Akzeptierte Domains des aktuellen Chunks; wird gesammelt, damit der Cache in
    // Blockgröße geschrieben wird statt Zeile für Zeile.
    let mut pending: Vec<u8> = Vec::with_capacity(256);
    let mut overflow = false;

    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            break;
        }

        let mut start = 0usize;
        while let Some(offset) = chunk[start..].iter().position(|&byte| byte == b'\n') {
            let end = start + offset;
            let segment = &chunk[start..end];

            if overflow {
                // Rest einer bereits verworfenen Zeile: nur abschließen.
                overflow = false;
                totals.skipped += 1;
            } else if line.len() + segment.len() > MAX_LINE_BYTES {
                line.clear();
                totals.skipped += 1;
            } else {
                line.extend_from_slice(segment);
                emit_line(&mut line, backend, totals, &mut pending);
            }

            start = end + 1;
        }

        if !overflow && start < chunk.len() {
            let rest = &chunk[start..];
            if line.len() + rest.len() > MAX_LINE_BYTES {
                // Zeile ist zu lang: verwerfen, Rest bis zum nächsten \n ignorieren.
                overflow = true;
                line.clear();
            } else {
                line.extend_from_slice(rest);
            }
        }

        let consumed = chunk.len();
        if !pending.is_empty() {
            sink.write_bytes(&pending).await;
            pending.clear();
        }
        Pin::new(&mut *reader).consume(consumed);
    }

    if !overflow && !line.is_empty() {
        emit_line(&mut line, backend, totals, &mut pending);
    }
    if !pending.is_empty() {
        sink.write_bytes(&pending).await;
    }
    Ok(())
}

/// Verarbeitet eine komplette Zeile, hängt akzeptierte Domains an `pending` und leert
/// den Zeilenpuffer.
fn emit_line(
    line: &mut Vec<u8>,
    backend: &mut dyn FilterBackend,
    totals: &mut Totals,
    pending: &mut Vec<u8>,
) {
    totals.lines += 1;
    if line.last() == Some(&b'\r') {
        line.pop();
    }

    match std::str::from_utf8(line) {
        Ok(text) => match blocklist_domain_from_line(text) {
            Some(domain) => {
                backend.add(domain.as_ref());
                totals.accepted += 1;
                pending.extend_from_slice(domain.as_bytes());
                pending.push(b'\n');
            }
            None => totals.skipped += 1,
        },
        Err(_) => totals.skipped += 1,
    }
    line.clear();
}

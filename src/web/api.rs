//! Ultra-schlanker HTTP-Server: Dashboard unter `/`, JSON unter `/api/stats` und
//! `/api/disable`.
//!
//! Kein Framework, kein Routing-Baum: Request-Zeile plus Header lesen (mit Limit und
//! Timeout), Pfad matchen, JSON von Hand schreiben. Es gibt keine Templates, keine
//! Cookies und keine Sessions – die API ist read-only bis auf den Disable-Switch.
//!
//! Das Dashboard ist eine einzige, per `include_str!` eingebettete HTML-Datei: kein
//! Asset-Ordner, kein Build-Schritt, keine externen Abhängigkeiten im Browser.
//!
//! Standardmäßig lauscht der Endpunkt auf `127.0.0.1`. Wer ihn auf `0.0.0.0` bindet,
//! sollte wissen, dass `/api/disable` dann ohne Authentifizierung erreichbar ist.

use std::fmt::Write as _;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::timeout;

use crate::filter::engine::{now_seconds, FilterEngine};
use crate::profiler::Tier;
use crate::stats::ringbuffer::Stats;

/// Maximal akzeptierte Request-Header.
const MAX_HEAD_BYTES: usize = 8 * 1024;
/// Timeout für das Lesen der Request-Zeile und Header.
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);
/// Gleichzeitige HTTP-Verbindungen (Slowloris-Bremse).
const MAX_CONNECTIONS: usize = 16;
/// Obergrenze für `?seconds=`.
const MAX_DISABLE_SECONDS: u64 = 86_400;
/// Standard-Dauer, wenn `seconds` fehlt.
const DEFAULT_DISABLE_SECONDS: u64 = 300;
/// Anzahl der Queries im JSON.
const RECENT_ENTRIES: usize = 20;
/// Das eingebettete Dashboard.
const DASHBOARD: &str = include_str!("static/index.html");

/// Der HTTP-Endpunkt.
pub struct ApiServer {
    listener: TcpListener,
    stats: Arc<Stats>,
    engine: Arc<FilterEngine>,
    tier: Tier,
    cache_entries: usize,
    verbose: bool,
}

impl ApiServer {
    /// Baut den Endpunkt auf einem bereits gebundenen Listener.
    pub fn new(
        listener: TcpListener,
        stats: Arc<Stats>,
        engine: Arc<FilterEngine>,
        tier: Tier,
        cache_entries: usize,
        verbose: bool,
    ) -> Self {
        Self {
            listener,
            stats,
            engine,
            tier,
            cache_entries,
            verbose,
        }
    }

    /// Nimmt Verbindungen an, bis der Listener einen Fehler liefert.
    pub async fn run(self: Arc<Self>) -> io::Result<()> {
        let local = self.listener.local_addr()?;
        println!("[api] http auf http://{local}");
        let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));

        loop {
            let (stream, peer) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(err) => {
                    eprintln!("[api] accept fehlgeschlagen: {err}");
                    continue;
                }
            };
            let permit = match Arc::clone(&connections).acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => break,
            };

            let server = Arc::clone(&self);
            tokio::spawn(async move {
                server.handle(stream, peer).await;
                drop(permit);
            });
        }
        Ok(())
    }

    async fn handle(&self, mut stream: TcpStream, peer: SocketAddr) {
        let head = match timeout(HEAD_TIMEOUT, read_head(&mut stream)).await {
            Ok(Ok(head)) => head,
            Ok(Err(err)) => {
                self.log(format_args!("{peer}: request nicht lesbar: {err}"));
                return;
            }
            Err(_) => {
                self.log(format_args!("{peer}: timeout beim lesen der request"));
                return;
            }
        };

        let Some(request_line) = head.lines().next() else {
            let _ = stream.write_all(BAD_REQUEST.as_bytes()).await;
            return;
        };
        let Some(target) = request_line.split_whitespace().nth(1) else {
            let _ = stream.write_all(BAD_REQUEST.as_bytes()).await;
            return;
        };
        let (path, query) = match target.split_once('?') {
            Some((path, query)) => (path, query),
            None => (target, ""),
        };

        let response = match path {
            "/" | "/index.html" => text_response("200 OK", "text/html; charset=utf-8", DASHBOARD),
            "/api/stats" => self.stats_response(),
            "/api/disable" => self.disable_response(query),
            _ => http_response("404 Not Found", "{\"error\":\"unbekannter endpunkt\"}"),
        };

        if let Err(err) = stream.write_all(response.as_bytes()).await {
            self.log(format_args!("{peer}: antwort nicht sendbar: {err}"));
        }
        let _ = stream.shutdown().await;
    }

    /// Baut das JSON für `/api/stats`.
    fn stats_response(&self) -> String {
        let snapshot = self.stats.snapshot(RECENT_ENTRIES);
        let backend = self.engine.stats();
        let disabled = self
            .engine
            .disabled_remaining_at(now_seconds())
            .map(|remaining| remaining.as_secs())
            .unwrap_or(0);

        let mut body = String::with_capacity(1024);
        let _ = write!(
            body,
            "{{\"uptime_s\":{},\"queries\":{},\"blocked\":{},\"cached\":{},\"forwarded\":{},\"errors\":{},\"domains\":{},\"tier\":\"{}\",\"backend\":\"{}\",\"cache_entries\":{},\"disabled_remaining_s\":{}",
            snapshot.uptime.as_secs(),
            snapshot.queries,
            snapshot.blocked,
            snapshot.cached,
            snapshot.forwarded,
            snapshot.errors,
            backend.domains,
            self.tier.label(),
            backend.name,
            self.cache_entries,
            disabled
        );

        body.push_str(",\"recent\":[");
        for (index, entry) in snapshot.recent.iter().enumerate() {
            if index > 0 {
                body.push(',');
            }
            let _ = write!(body, "{{\"age_s\":{},\"name\":\"", entry.age_secs);
            json_escape(&mut body, &entry.name);
            let _ = write!(
                body,
                "\",\"qtype\":{},\"verdict\":\"{}\"}}",
                entry.qtype,
                entry.verdict.label()
            );
        }
        body.push_str("]}");

        http_response("200 OK", &body)
    }

    /// Schaltet den Filter für `?seconds=` ab (0 = sofort wieder an).
    fn disable_response(&self, query: &str) -> String {
        let seconds = match query_param(query, "seconds") {
            Ok(Some(raw)) => match raw.parse::<u64>() {
                Ok(value) => value.min(MAX_DISABLE_SECONDS),
                Err(_) => {
                    return http_response(
                        "400 Bad Request",
                        "{\"error\":\"seconds muss eine zahl sein\"}",
                    )
                }
            },
            Ok(None) => DEFAULT_DISABLE_SECONDS,
            Err(()) => return http_response("400 Bad Request", "{\"error\":\"kaputte query\"}"),
        };

        if seconds == 0 {
            self.engine.enable();
            println!("[api] filter wieder aktiv");
        } else {
            self.engine.disable_for(seconds, now_seconds());
            println!("[api] filter für {seconds}s deaktiviert");
        }

        let remaining = self
            .engine
            .disabled_remaining_at(now_seconds())
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let body = format!("{{\"ok\":true,\"disabled_remaining_s\":{remaining}}}");
        http_response("200 OK", &body)
    }

    fn log(&self, args: std::fmt::Arguments<'_>) {
        if self.verbose {
            eprintln!("[api] {args}");
        }
    }
}

const BAD_REQUEST: &str = "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 18\r\nConnection: close\r\n\r\n{\"error\":\"bad req\"}";

fn http_response(status: &str, body: &str) -> String {
    text_response(status, "application/json; charset=utf-8", body)
}

fn text_response(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Liest bis zum Ende der Header, maximal [`MAX_HEAD_BYTES`].
async fn read_head(stream: &mut TcpStream) -> io::Result<String> {
    let mut buffer = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];

    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request-header zu groß",
            ));
        }
    }

    String::from_utf8(buffer)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "request ist kein utf-8"))
}

/// Sucht `key=value` in einem Query-String.
fn query_param<'a>(query: &'a str, key: &str) -> Result<Option<&'a str>, ()> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        match pair.split_once('=') {
            Some((name, value)) if name == key => return Ok(Some(value)),
            Some(_) => {}
            None => return Err(()),
        }
    }
    Ok(None)
}

fn json_escape(out: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if (character as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", character as u32);
            }
            character => out.push(character),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::engine::{backend_for_tier, FilterEngine};
    use tokio::io::AsyncReadExt;

    /// Bestand wie in Produktion: Backend bauen, versiegeln, tauschen.
    fn blocked_engine(domain: &str) -> Arc<FilterEngine> {
        let engine = FilterEngine::new(Tier::Nano);
        let mut backend = backend_for_tier(Tier::Nano);
        backend.add(domain);
        backend.seal();
        engine.replace_backend(backend);
        Arc::new(engine)
    }

    async fn start_api() -> (SocketAddr, Arc<FilterEngine>, Arc<Stats>) {
        let engine = blocked_engine("blocked.test");
        let stats = Arc::new(Stats::new(16));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = Arc::new(ApiServer::new(
            listener,
            Arc::clone(&stats),
            Arc::clone(&engine),
            Tier::Nano,
            64,
            false,
        ));
        tokio::spawn(server.run());
        (addr, engine, stats)
    }

    async fn request(addr: SocketAddr, target: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request = format!("GET {target} HTTP/1.1\r\nHost: test\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn stats_endpoint_reports_counters_and_recent_queries() {
        let (addr, _engine, stats) = start_api().await;
        stats.record(
            "example.com",
            1,
            crate::stats::ringbuffer::Verdict::Forwarded,
        );
        stats.record(
            "blocked.test",
            1,
            crate::stats::ringbuffer::Verdict::Blocked,
        );

        let response = request(addr, "/api/stats").await;
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("\"queries\":2"), "{response}");
        assert!(response.contains("\"blocked\":1"), "{response}");
        assert!(response.contains("\"domains\":1"), "{response}");
        assert!(response.contains("\"name\":\"blocked.test\""), "{response}");
        assert!(response.contains("\"verdict\":\"blocked\""), "{response}");
        assert!(
            response.contains("\"tier\":\"Tier 1 (Nano)\""),
            "{response}"
        );
    }

    #[tokio::test]
    async fn disable_endpoint_toggles_the_filter() {
        let (addr, engine, _stats) = start_api().await;
        assert!(engine.is_blocked_at("blocked.test", 0));

        let response = request(addr, "/api/disable?seconds=42").await;
        assert!(
            response.contains("\"disabled_remaining_s\":42"),
            "{response}"
        );
        assert!(
            !engine.is_blocked_at("blocked.test", 0),
            "filter muss aus sein"
        );
        assert!(engine.disabled_remaining_at(now_seconds()).is_some());

        let response = request(addr, "/api/disable?seconds=0").await;
        assert!(
            response.contains("\"disabled_remaining_s\":0"),
            "{response}"
        );
        assert!(
            engine.is_blocked_at("blocked.test", 0),
            "filter muss wieder an sein"
        );
    }

    #[tokio::test]
    async fn disable_without_seconds_defaults_to_five_minutes() {
        let (addr, engine, _stats) = start_api().await;
        request(addr, "/api/disable").await;
        let remaining = engine
            .disabled_remaining_at(now_seconds())
            .expect("filter aus");
        assert!(remaining.as_secs() >= 298 && remaining.as_secs() <= 300);
    }

    #[tokio::test]
    async fn invalid_seconds_is_a_bad_request() {
        let (addr, engine, _stats) = start_api().await;
        let response = request(addr, "/api/disable?seconds=abc").await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert!(!engine.is_disabled_at(now_seconds()));
    }

    #[tokio::test]
    async fn dashboard_is_served_at_root() {
        let (addr, _engine, _stats) = start_api().await;
        let response = request(addr, "/").await;
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(
            response.contains("Content-Type: text/html; charset=utf-8"),
            "{response}"
        );
        assert!(response.contains("<title>pihole-lite – Dashboard</title>"));
        assert!(response.contains("fetch(\"/api/stats\""));
    }

    #[tokio::test]
    async fn unknown_paths_are_not_found() {
        let (addr, _engine, _stats) = start_api().await;
        let response = request(addr, "/admin").await;
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    }
}

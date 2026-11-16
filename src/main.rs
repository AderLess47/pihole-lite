//! pihole-lite – Bootstrapping: Hardware erkennen, Config lesen, Filter füllen,
//! Runtime starten.
//!
//! Der DNS-Core, die Filter-Backends und der Profiler liegen in eigenen Modulen; hier
//! werden sie nur verdrahtet.

mod dns;
mod filter;
mod profiler;
mod stats;
mod web;

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use dns::server::{DnsServer, ServerOptions, SinkholeMode};
use dns::upstream::Upstream;
use filter::engine::FilterEngine;
use filter::gravity::{GravityConfig, GravityUpdater};
use profiler::HardwareProfile;
use stats::ringbuffer::Stats;
use web::api::ApiServer;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_CONFIG_FILE: &str = "config.toml";
const DEFAULT_UPSTREAM_TIMEOUT_MS: u64 = 2_000;
const DEFAULT_SINKHOLE_TTL: u32 = 60;
const DEFAULT_GRAVITY_CACHE_DIR: &str = "./gravity-cache";

fn main() -> ExitCode {
    let args = match Args::parse() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("pihole-lite: {err}\n\n{}", Args::USAGE);
            return ExitCode::from(2);
        }
    };
    if args.help {
        println!("{}", Args::USAGE);
        return ExitCode::SUCCESS;
    }

    let profile = HardwareProfile::detect();

    let mut config = match Config::load(args.config.as_deref()) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("pihole-lite: {err}");
            return ExitCode::from(2);
        }
    };
    config.apply_args(&args);
    if config.max_in_flight == 0 {
        config.max_in_flight = profile.tier.max_in_flight();
    }
    if config.cache_entries == 0 {
        config.cache_entries = profile.tier.cache_entries();
    }

    let engine = Arc::new(FilterEngine::new(profile.tier));
    let gravity = Arc::new(GravityUpdater::new(config.gravity_config()));

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(profile.worker_threads)
        .thread_name("pihole-lite")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("pihole-lite: runtime konnte nicht starten: {err}");
            return ExitCode::from(1);
        }
    };

    runtime.block_on(async move {
        // Bestand vor dem Socket-Aufbau aus lokalen Dateien und Platten-Cache laden:
        // ein Neustart filtert damit sofort, ohne auf Downloads zu warten.
        match gravity.preload(&engine).await {
            Ok(report) => {
                if report.domains > 0 {
                    println!(
                        "[filter] {} domains aus {} lokalen quelle(n)/cache geladen",
                        report.domains, report.sources
                    );
                }
            }
            Err(err) => {
                eprintln!("pihole-lite: blocklisten konnten nicht geladen werden: {err}");
                return ExitCode::from(1);
            }
        }
        for domain in &config.blocklist_entries {
            engine.add(domain);
        }
        engine.seal();

        print_banner(&profile, &config, &engine);
        serve(config, engine, gravity).await
    })
}

/// Startet den UDP-Listener, den Gravity-Updater und wartet auf Ctrl-C.
async fn serve(
    config: Config,
    engine: Arc<FilterEngine>,
    gravity: Arc<GravityUpdater>,
) -> ExitCode {
    let upstream = Arc::new(Upstream::new(
        config.upstreams.clone(),
        config.upstream_timeout,
        config.verbose,
    ));
    let stats = Arc::new(Stats::new(engine.tier().stats_entries()));
    let server = Arc::new(DnsServer::new(
        ServerOptions {
            bind: config.listen,
            mode: config.sinkhole,
            sinkhole_ttl: config.sinkhole_ttl,
            max_in_flight: config.max_in_flight,
            cache_entries: config.cache_entries,
            verbose: config.verbose,
        },
        Arc::clone(&engine),
        upstream,
        Arc::clone(&stats),
    ));

    let handle = tokio::spawn(Arc::clone(&server).run());

    if let Some(addr) = config.api_listen {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                let api = Arc::new(ApiServer::new(
                    listener,
                    Arc::clone(&stats),
                    Arc::clone(&engine),
                    engine.tier(),
                    config.cache_entries,
                    config.verbose,
                ));
                let _api_task = tokio::spawn(api.run());
            }
            // Die API ist Komfort: ohne sie läuft der DNS-Dienst weiter.
            Err(err) => eprintln!("[api] bind auf {addr} fehlgeschlagen: {err} (api aus)"),
        }
    }

    if gravity.is_enabled() {
        // Der Updater läuft neben dem Listener, damit DNS schon vor dem ersten
        // Download antwortet. Er endet mit dem Runtime-Shutdown.
        let _gravity_task = tokio::spawn(Arc::clone(&gravity).run(Arc::clone(&engine)));
    }

    tokio::select! {
        result = handle => match result {
            Ok(Ok(())) => ExitCode::SUCCESS,
            Ok(Err(err)) => {
                eprintln!("[dns] listener beendet: {err}");
                ExitCode::from(1)
            }
            Err(err) => {
                eprintln!("[dns] listener task abgebrochen: {err}");
                ExitCode::from(1)
            }
        },
        signal = tokio::signal::ctrl_c() => {
            if let Err(err) = signal {
                eprintln!("[main] signal-handler fehlgeschlagen: {err}");
            }
            println!("[main] shutdown angefordert");
            ExitCode::SUCCESS
        }
    }
}

fn print_banner(profile: &HardwareProfile, config: &Config, engine: &FilterEngine) {
    println!("pihole-lite {VERSION}");
    println!("  hardware : {}", profile.summary());
    println!(
        "  tier     : {} -> {}",
        profile.tier.label(),
        engine.backend_name()
    );
    println!(
        "  filter   : {} domains, ~{} heap",
        engine.domain_count(),
        fmt_bytes(engine.heap_bytes() as u64)
    );
    println!(
        "  dns      : udp {} (sinkhole {}, ttl {}s, max {} parallel, cache {} antworten)",
        config.listen,
        config.sinkhole.label(),
        config.sinkhole_ttl,
        config.max_in_flight,
        config.cache_entries
    );
    if let Some(addr) = config.api_listen {
        println!("  api      : http://{addr}/api/stats");
    }
    println!(
        "  upstream : {} (timeout {} ms)",
        config
            .upstreams
            .iter()
            .map(SocketAddr::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        config.upstream_timeout.as_millis()
    );
    if !config.blocklists.is_empty() || config.gravity_cache_dir.is_some() {
        println!(
            "  gravity  : {} URL(s), {}, timeout {}s, cache {} (max-alter {})",
            config.blocklists.len(),
            if config.gravity_interval.is_zero() {
                "einmalig".to_owned()
            } else {
                format!("alle {}h", config.gravity_interval.as_secs() / 3600)
            },
            config.gravity_timeout.as_secs(),
            config
                .gravity_cache_dir
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "aus".to_owned()),
            if config.gravity_cache_max_age.is_zero() {
                "unbegrenzt".to_owned()
            } else {
                format!("{}h", config.gravity_cache_max_age.as_secs() / 3600)
            }
        );
    }
}

pub(crate) fn fmt_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value < KIB {
        format!("{bytes} B")
    } else if value < KIB * KIB {
        format!("{:.1} KiB", value / KIB)
    } else {
        format!("{:.1} MiB", value / (KIB * KIB))
    }
}

/// Laufzeitkonfiguration: Defaults, überschrieben von `config.toml` und CLI-Flags.
struct Config {
    listen: SocketAddr,
    upstreams: Vec<SocketAddr>,
    upstream_timeout: Duration,
    sinkhole: SinkholeMode,
    sinkhole_ttl: u32,
    blocklist_file: Option<PathBuf>,
    blocklist_entries: Vec<String>,
    blocklists: Vec<String>,
    gravity_interval: Duration,
    gravity_timeout: Duration,
    fetcher: Option<String>,
    gravity_cache_dir: Option<PathBuf>,
    gravity_cache_max_age: Duration,
    max_in_flight: usize,
    cache_entries: usize,
    api_listen: Option<SocketAddr>,
    verbose: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 53)),
            upstreams: vec![
                SocketAddr::from(([1, 1, 1, 1], 53)),
                SocketAddr::from(([9, 9, 9, 9], 53)),
            ],
            upstream_timeout: Duration::from_millis(DEFAULT_UPSTREAM_TIMEOUT_MS),
            sinkhole: SinkholeMode::ZeroIp,
            sinkhole_ttl: DEFAULT_SINKHOLE_TTL,
            blocklist_file: None,
            blocklist_entries: Vec::new(),
            blocklists: Vec::new(),
            // 24h ist der übliche Rhythmus für Blocklisten.
            gravity_interval: Duration::from_secs(24 * 3600),
            gravity_timeout: Duration::from_secs(300),
            fetcher: None,
            gravity_cache_dir: Some(PathBuf::from(DEFAULT_GRAVITY_CACHE_DIR)),
            // 7 Tage: ältere Listen sind eher falsch als nützlich. 0 = unbegrenzt.
            gravity_cache_max_age: Duration::from_secs(7 * 24 * 3600),
            // 0 = aus dem Hardware-Tier ableiten.
            max_in_flight: 0,
            cache_entries: 0,
            // Nur lokal erreichbar: der Disable-Switch hat keine Authentifizierung.
            api_listen: Some(SocketAddr::from(([127, 0, 0, 1], 8080))),
            verbose: false,
        }
    }
}

impl Config {
    fn load(path: Option<&Path>) -> Result<Self, String> {
        let mut config = Self::default();
        match path {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|err| format!("kann {} nicht lesen: {err}", path.display()))?;
                config
                    .apply_toml(&text)
                    .map_err(|err| format!("{}: {err}", path.display()))?;
            }
            None => {
                // Ohne explizites --config ist eine fehlende config.toml in Ordnung.
                if let Ok(text) = std::fs::read_to_string(DEFAULT_CONFIG_FILE) {
                    config
                        .apply_toml(&text)
                        .map_err(|err| format!("{DEFAULT_CONFIG_FILE}: {err}"))?;
                }
            }
        }
        Ok(config)
    }

    /// Minimaler `key = value`-Parser (Strings, Zahlen, Booleans, String-Arrays).
    fn apply_toml(&mut self, text: &str) -> Result<(), String> {
        for (index, raw_line) in text.lines().enumerate() {
            let line = strip_comment(raw_line).trim();
            if line.is_empty() || line.starts_with('[') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("zeile {}: erwartet `key = value`", index + 1));
            };
            let value = Value::parse(value)
                .ok_or_else(|| format!("zeile {}: wert nicht unterstützt", index + 1))?;
            self.set(key.trim(), value)
                .map_err(|err| format!("zeile {}: {err}", index + 1))?;
        }
        Ok(())
    }

    fn set(&mut self, key: &str, value: Value) -> Result<(), String> {
        match key {
            "listen" => self.listen = parse_addr(&value.into_str()?)?,
            "upstream" | "upstreams" => {
                let mut upstreams = Vec::new();
                for entry in value.into_list()? {
                    for part in entry.split(',') {
                        upstreams.push(parse_addr(part)?);
                    }
                }
                if upstreams.is_empty() {
                    return Err("`upstream` darf nicht leer sein".into());
                }
                self.upstreams = upstreams;
            }
            "upstream_timeout_ms" => {
                self.upstream_timeout = Duration::from_millis(value.into_int(10, 30_000)? as u64);
            }
            "sinkhole" => {
                self.sinkhole = match value.into_str()?.to_ascii_lowercase().as_str() {
                    "zero" | "zero-ip" | "0.0.0.0" => SinkholeMode::ZeroIp,
                    "nxdomain" | "nx" => SinkholeMode::NxDomain,
                    other => {
                        return Err(format!(
                            "unbekannter sinkhole-modus `{other}` (erlaubt: zero, nxdomain)"
                        ))
                    }
                };
            }
            "sinkhole_ttl" => self.sinkhole_ttl = value.into_int(0, 86_400)? as u32,
            "blocklist_file" => self.blocklist_file = Some(PathBuf::from(value.into_str()?)),
            "block" | "blocked_domains" => self.blocklist_entries = value.into_list()?,
            "blocklist" | "blocklists" => self.blocklists = value.into_list()?,
            "gravity_interval_hours" => {
                self.gravity_interval =
                    Duration::from_secs(value.into_int(0, 24 * 365)? as u64 * 3600);
            }
            "gravity_timeout_seconds" => {
                self.gravity_timeout = Duration::from_secs(value.into_int(10, 86_400)? as u64);
            }
            "fetcher" => self.fetcher = Some(value.into_str()?),
            "gravity_cache_dir" => {
                let dir = value.into_str()?;
                self.gravity_cache_dir = if dir.trim().is_empty() {
                    None
                } else {
                    Some(PathBuf::from(dir))
                };
            }
            "gravity_cache_max_age_hours" => {
                self.gravity_cache_max_age =
                    Duration::from_secs(value.into_int(0, 24 * 365)? as u64 * 3600);
            }
            "max_in_flight" => self.max_in_flight = value.into_int(1, 4096)? as usize,
            "cache_entries" => self.cache_entries = value.into_int(0, 1_000_000)? as usize,
            "api_listen" => {
                let raw = value.into_str()?;
                self.api_listen = if raw.trim().is_empty() {
                    None
                } else {
                    Some(parse_addr(&raw)?)
                };
            }
            "verbose" => self.verbose = value.into_bool()?,
            other => return Err(format!("unbekannter key `{other}`")),
        }
        Ok(())
    }

    fn apply_args(&mut self, args: &Args) {
        if let Some(listen) = args.listen {
            self.listen = listen;
        }
        if !args.upstreams.is_empty() {
            self.upstreams = args.upstreams.clone();
        }
        if let Some(path) = args.blocklist_file.clone() {
            self.blocklist_file = Some(path);
        }
        if args.verbose {
            self.verbose = true;
        }
    }

    fn gravity_config(&self) -> GravityConfig {
        GravityConfig {
            urls: self.blocklists.clone(),
            // Ein Update baut den Bestand komplett neu auf: lokale Quellen müssen mit.
            local_files: self.blocklist_file.iter().cloned().collect(),
            inline: self.blocklist_entries.clone(),
            fetcher: self.fetcher.clone(),
            cache_dir: self.gravity_cache_dir.clone(),
            cache_max_age: if self.gravity_cache_max_age.is_zero() {
                None
            } else {
                Some(self.gravity_cache_max_age)
            },
            interval: self.gravity_interval,
            source_timeout: self.gravity_timeout,
            verbose: self.verbose,
        }
    }
}

/// Entfernt `#`-Kommentare, ohne Anführungszeichen anzufassen.
fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (index, character) in line.char_indices() {
        match character {
            '"' | '\'' => in_quotes = !in_quotes,
            '#' if !in_quotes => return &line[..index],
            _ => {}
        }
    }
    line
}

/// Werttypen des Mini-TOML-Parsers.
enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    List(Vec<String>),
}

impl Value {
    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if let Some(inner) = raw.strip_prefix('[') {
            let inner = inner.strip_suffix(']')?;
            let mut items = Vec::new();
            for part in inner.split(',') {
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }
                items.push(unquote(part)?.to_owned());
            }
            return Some(Self::List(items));
        }
        if raw.starts_with('"') || raw.starts_with('\'') {
            return Some(Self::Str(unquote(raw)?.to_owned()));
        }
        match raw {
            "true" => Some(Self::Bool(true)),
            "false" => Some(Self::Bool(false)),
            _ => raw.parse::<i64>().ok().map(Self::Int),
        }
    }

    fn into_str(self) -> Result<String, String> {
        match self {
            Self::Str(value) => Ok(value),
            _ => Err("string erwartet".into()),
        }
    }

    fn into_bool(self) -> Result<bool, String> {
        match self {
            Self::Bool(value) => Ok(value),
            _ => Err("boolean erwartet".into()),
        }
    }

    fn into_int(self, min: i64, max: i64) -> Result<i64, String> {
        match self {
            Self::Int(value) if (min..=max).contains(&value) => Ok(value),
            Self::Int(value) => Err(format!("wert {value} liegt außerhalb {min}..={max}")),
            _ => Err("zahl erwartet".into()),
        }
    }

    fn into_list(self) -> Result<Vec<String>, String> {
        match self {
            Self::List(items) => Ok(items),
            Self::Str(item) => Ok(vec![item]),
            _ => Err("liste oder string erwartet".into()),
        }
    }
}

fn unquote(raw: &str) -> Option<&str> {
    let raw = raw.trim();
    let inner = if let Some(inner) = raw.strip_prefix('"') {
        inner.strip_suffix('"')?
    } else {
        let inner = raw.strip_prefix('\'')?;
        inner.strip_suffix('\'')?
    };
    Some(inner)
}

/// Parst `1.2.3.4:53`, `1.2.3.4` (Port 53) und `[::1]:53`.
fn parse_addr(raw: &str) -> Result<SocketAddr, String> {
    let raw = raw.trim();
    if let Ok(addr) = raw.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = raw.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, 53));
    }
    Err(format!(
        "ungültige adresse `{raw}` (erwartet z.B. 1.1.1.1:53)"
    ))
}

/// Kommandozeilen-Flags; überschreiben `config.toml`.
#[derive(Default)]
struct Args {
    config: Option<PathBuf>,
    listen: Option<SocketAddr>,
    upstreams: Vec<SocketAddr>,
    blocklist_file: Option<PathBuf>,
    verbose: bool,
    help: bool,
}

impl Args {
    const USAGE: &'static str = "\
usage: pihole-lite [OPTIONS]

  -c, --config <PATH>          config.toml (default: ./config.toml, falls vorhanden)
  -l, --listen <ADDR>          UDP-Listener, z.B. 0.0.0.0:53
  -u, --upstream <ADDR[,ADDR]> Upstream-Resolver, mehrfach oder komma-getrennt
  -b, --blocklist-file <PATH>  lokale Blockliste (Hosts-Format oder 1 Domain/Zeile)
  -v, --verbose                pro-query-logging auf stderr
  -h, --help                   diese hilfe
";

    fn parse() -> Result<Self, String> {
        let mut args = Self::default();
        let mut iterator = std::env::args().skip(1);
        while let Some(flag) = iterator.next() {
            match flag.as_str() {
                "--config" | "-c" => {
                    args.config = Some(PathBuf::from(next_value(&mut iterator, &flag)?))
                }
                "--listen" | "-l" => {
                    args.listen = Some(parse_addr(&next_value(&mut iterator, &flag)?)?)
                }
                "--upstream" | "-u" => {
                    for part in next_value(&mut iterator, &flag)?.split(',') {
                        args.upstreams.push(parse_addr(part)?);
                    }
                }
                "--blocklist-file" | "-b" => {
                    args.blocklist_file = Some(PathBuf::from(next_value(&mut iterator, &flag)?))
                }
                "--verbose" | "-v" => args.verbose = true,
                "--help" | "-h" => args.help = true,
                other => return Err(format!("unbekanntes argument `{other}`")),
            }
        }
        Ok(args)
    }
}

fn next_value(iterator: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    iterator
        .next()
        .ok_or_else(|| format!("`{flag}` erwartet einen wert"))
}

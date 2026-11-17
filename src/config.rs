//! Konfiguration: Defaults, `config.toml`, CLI-Flags und ihre Präzedenz.
//!
//! Reihenfolge beim Start: [`Config::default`] → `config.toml` ([`Config::load`]) →
//! CLI-Flags ([`Config::apply_args`]). Werte, die Grenzen kennen (`cache_entries`,
//! `sinkhole_ttl`, `max_in_flight`, Intervalle), werden hier geprüft; der Rest des
//! Programms bekommt nur noch gültige Werte zu sehen.
//!
//! Die `0`-Werte bei `max_in_flight` und `cache_entries` bedeuten „aus dem Tier
//! ableiten" — die Ableitung selbst passiert in `main`, weil sie das Hardware-Profil
//! braucht.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::dns::server::SinkholeMode;
use crate::filter::gravity::GravityConfig;

const DEFAULT_CONFIG_FILE: &str = "config.toml";
const DEFAULT_UPSTREAM_TIMEOUT_MS: u64 = 2_000;
const DEFAULT_SINKHOLE_TTL: u32 = 60;
const DEFAULT_GRAVITY_CACHE_DIR: &str = "./gravity-cache";

/// Laufzeitkonfiguration: Defaults, überschrieben von `config.toml` und CLI-Flags.
pub(crate) struct Config {
    pub(crate) listen: SocketAddr,
    pub(crate) upstreams: Vec<SocketAddr>,
    pub(crate) upstream_timeout: Duration,
    pub(crate) sinkhole: SinkholeMode,
    pub(crate) sinkhole_ttl: u32,
    pub(crate) blocklist_file: Option<PathBuf>,
    pub(crate) blocklist_entries: Vec<String>,
    pub(crate) blocklists: Vec<String>,
    pub(crate) gravity_interval: Duration,
    pub(crate) gravity_timeout: Duration,
    pub(crate) fetcher: Option<String>,
    pub(crate) gravity_cache_dir: Option<PathBuf>,
    pub(crate) gravity_cache_max_age: Duration,
    pub(crate) max_in_flight: usize,
    pub(crate) cache_entries: usize,
    pub(crate) api_listen: Option<SocketAddr>,
    pub(crate) verbose: bool,
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
    /// Lädt `path`, oder — ohne expliziten Pfad — `./config.toml`, falls vorhanden.
    ///
    /// Eine fehlende `config.toml` ist ohne `--config` kein Fehler: dann gelten die
    /// Defaults.
    pub(crate) fn load(path: Option<&Path>) -> Result<Self, String> {
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
    ///
    /// Sektionen (`[foo]`) werden ignoriert: die Datei ist flach, ein Tippfehler in der
    /// Sektion soll keine Werte stillschweigend verschlucken.
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

    /// Setzt einen einzelnen Key; unbekannte Keys sind ein Fehler, kein stiller Ignor.
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

    /// Überschreibt die Datei-Werte mit den CLI-Flags, die gesetzt wurden.
    pub(crate) fn apply_args(&mut self, args: &Args) {
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

    /// Übersetzt die Konfiguration in die Sicht des Gravity-Updaters.
    pub(crate) fn gravity_config(&self) -> GravityConfig {
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
pub(crate) struct Args {
    pub(crate) config: Option<PathBuf>,
    pub(crate) listen: Option<SocketAddr>,
    pub(crate) upstreams: Vec<SocketAddr>,
    pub(crate) blocklist_file: Option<PathBuf>,
    pub(crate) verbose: bool,
    pub(crate) help: bool,
}

impl Args {
    pub(crate) const USAGE: &'static str = "\
usage: pihole-lite [OPTIONS]

  -c, --config <PATH>          config.toml (default: ./config.toml, falls vorhanden)
  -l, --listen <ADDR>          UDP-Listener, z.B. 0.0.0.0:53
  -u, --upstream <ADDR[,ADDR]> Upstream-Resolver, mehrfach oder komma-getrennt
  -b, --blocklist-file <PATH>  lokale Blockliste (Hosts-Format oder 1 Domain/Zeile)
  -v, --verbose                pro-query-logging auf stderr
  -h, --help                   diese hilfe
";

    /// Liest die Prozessargumente.
    pub(crate) fn parse() -> Result<Self, String> {
        Self::parse_from(std::env::args().skip(1))
    }

    /// Liest eine beliebige Argumentliste — damit ist die CLI ohne Prozessstart testbar.
    fn parse_from(iterator: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut args = Self::default();
        let mut iterator = iterator;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, String> {
        let mut config = Config::default();
        config.apply_toml(text)?;
        Ok(config)
    }

    #[test]
    fn toml_overrides_defaults() {
        let config = parse(
            "listen = \"127.0.0.1:5353\"\n\
             upstream = [\"1.1.1.1:53\", \"9.9.9.9:53\"]\n\
             sinkhole = \"nxdomain\"\n\
             cache_entries = 2048\n\
             api_listen = \"\"\n",
        )
        .expect("gültige config");

        assert_eq!("127.0.0.1:5353", config.listen.to_string());
        assert_eq!(2, config.upstreams.len());
        assert_eq!(SinkholeMode::NxDomain, config.sinkhole);
        assert_eq!(2048, config.cache_entries);
        assert_eq!(None, config.api_listen);
    }

    #[test]
    fn unknown_keys_are_errors() {
        let err = parse("cache_entry = 2048\n")
            .err()
            .expect("tippfehler muss auffallen");
        assert!(err.contains("unbekannter key `cache_entry`"), "{err}");
    }

    #[test]
    fn out_of_range_values_name_their_bounds() {
        let err = parse("cache_entries = -1\n")
            .err()
            .expect("negativ ist ungültig");
        assert!(err.contains("außerhalb 0..=1000000"), "{err}");
    }

    #[test]
    fn comma_separated_upstreams_expand() {
        // Die Komma-Form kommt als ein gequoteter String: der Listen-Parser trennt
        // Einträge selbst an Kommas, in einer Liste wäre der zweite Teil kein Eintrag.
        let config = parse("upstream = \"1.1.1.1:53,9.9.9.9:53\"\n").expect("gültig");
        assert_eq!(2, config.upstreams.len());
        assert_eq!("9.9.9.9:53", config.upstreams[1].to_string());
    }

    #[test]
    fn hashes_inside_quotes_are_not_comments() {
        let config = parse("block = [\"a#b.test\"] # echter kommentar\n").expect("gültig");
        assert_eq!(vec!["a#b.test".to_string()], config.blocklist_entries);
    }

    #[test]
    fn args_reject_a_flag_without_value() {
        let err = Args::parse_from(["--listen".to_string()].into_iter())
            .err()
            .expect("wert fehlt");
        assert!(err.contains("`--listen` erwartet einen wert"), "{err}");
    }

    #[test]
    fn args_default_to_no_overrides_and_missing_file_is_an_error() {
        let args = Args::parse_from(Vec::new().into_iter()).expect("leere args");
        assert!(!args.help && !args.verbose);
        assert!(args.listen.is_none() && args.upstreams.is_empty());

        let err = Config::load(Some(Path::new("/gibt/es/nicht.toml")))
            .err()
            .expect("fehlende datei mit --config ist ein fehler");
        assert!(err.contains("kann /gibt/es/nicht.toml nicht lesen"), "{err}");
    }
}

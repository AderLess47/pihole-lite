//! pihole-lite – Bootstrapping: Hardware erkennen, Config lesen, Filter füllen,
//! Runtime starten.
//!
//! Der DNS-Core, die Filter-Backends und der Profiler liegen in eigenen Modulen; hier
//! werden sie nur verdrahtet. Defaults, `config.toml`, CLI-Flags und ihre Präzedenz
//! stehen in [`config`].

mod config;
mod dns;
mod filter;
mod profiler;
mod stats;
mod web;

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use config::{Args, Config};
use dns::server::{DnsServer, ServerOptions};
use dns::upstream::Upstream;
use filter::engine::FilterEngine;
use filter::gravity::GravityUpdater;
use profiler::HardwareProfile;
use stats::ringbuffer::Stats;
use web::api::ApiServer;

const VERSION: &str = env!("CARGO_PKG_VERSION");

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
                // Inline-Einträge zählen als Domain, aber nicht als Quelle: ohne Datei oder
                // Cache gibt es hier nichts zu melden, das Banner zeigt die Zahl trotzdem.
                if report.sources > 0 {
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
    let backend = engine.stats();
    println!("pihole-lite {VERSION}");
    println!("  hardware : {}", profile.summary());
    println!("  tier     : {} -> {}", profile.tier.label(), backend.name);
    println!(
        "  filter   : {} domains, ~{} heap",
        backend.domains,
        fmt_bytes(backend.heap_bytes as u64)
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

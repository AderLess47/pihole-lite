//! Update-Wege: veralteter Cache, Revalidierung, Preload ohne Quellen.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::super::cache::Validators;
use super::super::{GravityConfig, GravityUpdater};
use super::helpers::{age_file, temp_dir};
use crate::filter::engine::FilterEngine;
use crate::profiler::Tier;

#[tokio::test]
async fn stale_cache_is_ignored_at_startup_until_limit_is_off() {
    let dir = temp_dir("stale");
    let url = "http://example.org/hosts";
    let seed = GravityUpdater::new(GravityConfig {
        urls: vec![url.to_owned()],
        local_files: Vec::new(),
        inline: Vec::new(),
        fetcher: None,
        cache_dir: Some(dir.clone()),
        cache_max_age: None,
        interval: Duration::ZERO,
        source_timeout: Duration::from_secs(5),
        verbose: false,
    });
    let (list_path, _) = seed.cache.paths(url).unwrap();
    std::fs::write(&list_path, "ads.example.com\n").unwrap();
    age_file(&list_path, Duration::from_secs(30 * 24 * 3600));

    let engine = FilterEngine::new(Tier::Nano);
    let stale = GravityUpdater::new(GravityConfig {
        urls: vec![url.to_owned()],
        local_files: Vec::new(),
        inline: Vec::new(),
        fetcher: None,
        cache_dir: Some(dir.clone()),
        cache_max_age: Some(Duration::from_secs(3600)),
        interval: Duration::ZERO,
        source_timeout: Duration::from_secs(5),
        verbose: false,
    });
    let report = stale.preload(&engine).await.unwrap();
    assert_eq!(0, report.domains);
    assert!(!engine.is_blocked_at("ads.example.com", 0));

    // Ohne Limit wird dieselbe Datei geladen.
    let unlimited = GravityUpdater::new(GravityConfig {
        urls: vec![url.to_owned()],
        local_files: Vec::new(),
        inline: Vec::new(),
        fetcher: None,
        cache_dir: Some(dir.clone()),
        cache_max_age: None,
        interval: Duration::ZERO,
        source_timeout: Duration::from_secs(5),
        verbose: false,
    });
    let engine = FilterEngine::new(Tier::Nano);
    let report = unlimited.preload(&engine).await.unwrap();
    assert_eq!(1, report.domains);
    assert!(engine.is_blocked_at("ads.example.com", 0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn stale_cache_is_not_revalidated_conditionally() {
    let dir = temp_dir("stale-conditional");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/hosts");

    let seed = GravityUpdater::new(GravityConfig {
        urls: vec![url.clone()],
        local_files: Vec::new(),
        inline: Vec::new(),
        fetcher: None,
        cache_dir: Some(dir.clone()),
        cache_max_age: None,
        interval: Duration::ZERO,
        source_timeout: Duration::from_secs(5),
        verbose: false,
    });
    let (list_path, _) = seed.cache.paths(&url).unwrap();
    std::fs::write(&list_path, "old.test\n").unwrap();
    seed.cache
        .store_validators(
            &url,
            &Validators {
                etag: Some("\"v1\"".to_owned()),
                last_modified: None,
            },
        )
        .await
        .unwrap();
    age_file(&list_path, Duration::from_secs(30 * 24 * 3600));

    let seen = Arc::new(Mutex::new(String::new()));
    let seen_by_server = Arc::clone(&seen);
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0u8; 2048];
        let read = socket.read(&mut buffer).await.unwrap();
        *seen_by_server.lock().unwrap() = String::from_utf8_lossy(&buffer[..read]).to_string();
        let body = "new.test\n";
        socket
            .write_all(
                format!(
                    "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });

    let engine = FilterEngine::new(Tier::Nano);
    let report = GravityUpdater::new(GravityConfig {
        urls: vec![url.clone()],
        local_files: Vec::new(),
        inline: Vec::new(),
        fetcher: None,
        cache_dir: Some(dir.clone()),
        cache_max_age: Some(Duration::from_secs(3600)),
        interval: Duration::ZERO,
        source_timeout: Duration::from_secs(5),
        verbose: false,
    })
    .update(&engine)
    .await
    .expect("update should succeed");

    assert_eq!(1, report.domains);
    assert!(engine.is_blocked_at("new.test", 0));
    assert!(!seen.lock().unwrap().contains("If-None-Match"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn preload_applies_inline_domains_without_any_source() {
    let engine = FilterEngine::new(Tier::Nano);
    let updater = GravityUpdater::new(GravityConfig {
        urls: Vec::new(),
        local_files: Vec::new(),
        inline: vec!["inline.test".to_owned(), "Sub.Inline.TEST.".to_owned()],
        fetcher: None,
        cache_dir: None,
        cache_max_age: None,
        interval: Duration::ZERO,
        source_timeout: Duration::from_secs(5),
        verbose: false,
    });

    let report = updater.preload(&engine).await.unwrap();

    assert_eq!(2, report.domains);
    assert_eq!(0, report.sources);
    assert!(engine.is_blocked_at("inline.test", 0));
    assert!(engine.is_blocked_at("sub.inline.test", 0));
}

#[tokio::test]
async fn preload_reads_local_files_and_skips_missing_caches() {
    let path = std::env::temp_dir().join(format!(
        "pihole-lite-gravity-{}-preload.txt",
        std::process::id()
    ));
    std::fs::write(&path, "0.0.0.0 local.test\n").unwrap();

    let engine = FilterEngine::new(Tier::Nano);
    let updater = GravityUpdater::new(GravityConfig {
        // Kein Cache: der Start darf dafür keinen einzigen Netzwerkzugriff machen.
        urls: vec!["http://127.0.0.1:1/hosts".to_owned()],
        local_files: vec![path.clone()],
        inline: vec!["inline.test".to_owned()],
        fetcher: None,
        cache_dir: None,
        cache_max_age: None,
        interval: Duration::ZERO,
        source_timeout: Duration::from_secs(5),
        verbose: false,
    });

    let report = updater.preload(&engine).await.unwrap();
    let _ = std::fs::remove_file(&path);

    assert_eq!(2, report.domains);
    assert_eq!(1, report.sources);
    assert!(engine.is_blocked_at("local.test", 0));
    assert!(engine.is_blocked_at("inline.test", 0));
}

//! Helfer: Updater bauen, temporäre Verzeichnisse, Ein-Wegwerf-HTTP-Server.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::super::{GravityConfig, GravityUpdater};

pub(super) fn updater(urls: Vec<String>, timeout: Duration) -> GravityUpdater {
    GravityUpdater::new(GravityConfig {
        urls,
        local_files: Vec::new(),
        inline: Vec::new(),
        fetcher: None,
        cache_dir: None,
        cache_max_age: None,
        interval: Duration::ZERO,
        source_timeout: timeout,
        verbose: false,
    })
}

pub(super) fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pihole-lite-gravity-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Startet einen Wegwerf-HTTP-Server, der `response` einmal ausliefert.
pub(super) async fn serve_once(response: String) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 1024];
        let _ = socket.read(&mut request).await;
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    addr
}

/// Setzt die mtime einer Datei in die Vergangenheit (für Alterstests).
pub(super) fn age_file(path: &Path, age: Duration) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    let modified = SystemTime::now().checked_sub(age).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
}

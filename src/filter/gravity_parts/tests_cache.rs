    #[tokio::test]
    async fn conditional_get_revalidates_and_reuses_the_cache() {
        let dir = temp_dir("conditional");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = Arc::clone(&requests);

        tokio::spawn(async move {
            for round in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = vec![0u8; 2048];
                let read = socket.read(&mut buffer).await.unwrap();
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buffer[..read]).to_string());

                let response = if round == 0 {
                    "HTTP/1.0 200 OK\r\nETag: \"v1\"\r\nLast-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\n\r\nads.example.com\n"
                } else {
                    "HTTP/1.0 304 Not Modified\r\nETag: \"v1\"\r\n\r\n"
                };
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let engine = FilterEngine::new(Tier::Nano);
        let config = GravityConfig {
            urls: vec![format!("http://{addr}/hosts")],
            local_files: Vec::new(),
            inline: Vec::new(),
            fetcher: None,
            cache_dir: Some(dir.clone()),
            cache_max_age: None,
            interval: Duration::ZERO,
            source_timeout: Duration::from_secs(5),
            verbose: false,
        };

        let first = GravityUpdater::new(config.clone())
            .update(&engine)
            .await
            .expect("first update");
        assert_eq!(1, first.domains);
        assert!(engine.is_blocked("ads.example.com"));

        let second = GravityUpdater::new(config)
            .update(&engine)
            .await
            .expect("second update");
        assert_eq!(1, second.domains);
        assert!(engine.is_blocked("ads.example.com"));

        let requests = requests.lock().unwrap();
        assert!(requests[0].contains("GET /hosts HTTP/1.0"));
        assert!(!requests[0].contains("If-None-Match"));
        assert!(requests[1].contains("If-None-Match: \"v1\""));
        assert!(requests[1].contains("If-Modified-Since: Wed, 21 Oct 2015"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn preload_restores_the_cache_without_network() {
        let dir = temp_dir("preload");
        let body = "ads.example.com\n";
        let addr = serve_once(format!(
            "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ))
        .await;

        let config = GravityConfig {
            urls: vec![format!("http://{addr}/hosts")],
            local_files: Vec::new(),
            inline: Vec::new(),
            fetcher: None,
            cache_dir: Some(dir.clone()),
            cache_max_age: None,
            interval: Duration::ZERO,
            source_timeout: Duration::from_secs(5),
            verbose: false,
        };

        let engine = FilterEngine::new(Tier::Nano);
        GravityUpdater::new(config.clone())
            .update(&engine)
            .await
            .expect("update should succeed");

        // "Neustart": neue Engine, die Quelle ist nicht mehr erreichbar.
        let fresh = FilterEngine::new(Tier::Nano);
        let report = GravityUpdater::new(config)
            .preload(&fresh)
            .await
            .expect("preload should succeed");

        assert_eq!(1, report.domains);
        assert!(fresh.is_blocked("sub.ads.example.com"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Setzt die mtime einer Datei in die Vergangenheit (für Alterstests).
    fn age_file(path: &Path, age: Duration) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        let modified = SystemTime::now().checked_sub(age).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
    }

    #[tokio::test]
    async fn prune_removes_orphans_and_temporary_files_only() {
        let dir = temp_dir("prune");
        let updater = GravityUpdater::new(GravityConfig {
            urls: vec!["http://example.org/keep".to_owned()],
            local_files: Vec::new(),
            inline: Vec::new(),
            fetcher: None,
            cache_dir: Some(dir.clone()),
            cache_max_age: None,
            interval: Duration::ZERO,
            source_timeout: Duration::from_secs(5),
            verbose: false,
        });
        let cache = &updater.cache;

        let (keep_list, _) = cache.paths("http://example.org/keep").unwrap();
        let (gone_list, gone_meta) = cache.paths("http://example.org/gone").unwrap();
        tokio::fs::write(&keep_list, "keep.test\n").await.unwrap();
        tokio::fs::write(&gone_list, "gone.test\n").await.unwrap();
        tokio::fs::write(&gone_meta, "url\t00\n").await.unwrap();
        let leftover = keep_list.with_extension("list.tmp");
        tokio::fs::write(&leftover, "partial\n").await.unwrap();
        let foreign = dir.join("notes.txt");
        tokio::fs::write(&foreign, "keep me").await.unwrap();

        let removed = updater.prune_cache().await.unwrap();

        assert_eq!(3, removed);
        assert!(keep_list.exists(), "liste der aktiven url muss bleiben");
        assert!(!gone_list.exists());
        assert!(!gone_meta.exists());
        assert!(!leftover.exists());
        assert!(
            foreign.exists(),
            "fremde dateien dürfen nicht gelöscht werden"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

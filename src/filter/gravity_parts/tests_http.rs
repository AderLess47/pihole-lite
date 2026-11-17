    #[test]
    fn http_urls_are_split_into_host_port_and_path() {
        let url = HttpUrl::parse("http://example.org/lists/hosts.txt").unwrap();
        assert_eq!("example.org", url.host);
        assert_eq!(80, url.port);
        assert_eq!("/lists/hosts.txt", url.path);
        assert_eq!("example.org", url.authority);

        let url = HttpUrl::parse("http://127.0.0.1:8080/a?b=c").unwrap();
        assert_eq!("127.0.0.1", url.host);
        assert_eq!(8080, url.port);
        assert_eq!("/a?b=c", url.path);

        assert!(matches!(
            HttpUrl::parse("https://example.org/x"),
            Err(GravityError::UnsupportedScheme(_))
        ));
    }

    #[tokio::test]
    async fn http_body_is_streamed_line_by_line_and_swapped_in() {
        let body = "0.0.0.0 ads.example.com\n# kommentar\n\ntracker.test\n";
        let addr = serve_once(format!(
            "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ))
        .await;

        let engine = FilterEngine::new(Tier::Nano);
        assert!(!engine.is_blocked_at("ads.example.com", 0));

        let report = updater(vec![format!("http://{addr}/hosts")], Duration::from_secs(5))
            .update(&engine)
            .await
            .expect("update should succeed");

        assert_eq!(2, report.domains);
        assert_eq!(4, report.lines);
        assert!(engine.is_blocked_at("sub.ads.example.com", 0));
        assert!(engine.is_blocked_at("tracker.test", 0));
        assert!(!engine.is_blocked_at("example.com", 0));
        assert_eq!(2, engine.stats().domains);
    }

    #[tokio::test]
    async fn chunked_responses_are_rejected_instead_of_corrupting_the_list() {
        let addr = serve_once(
            "HTTP/1.0 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd\r\n0\r\n\r\n"
                .to_owned(),
        )
        .await;

        let engine = FilterEngine::new(Tier::Nano);
        let result = updater(vec![format!("http://{addr}/hosts")], Duration::from_secs(5))
            .update(&engine)
            .await;

        assert!(matches!(result, Err(GravityError::Http { .. })));
        assert_eq!(0, engine.stats().domains);
    }

    #[tokio::test]
    async fn failed_update_keeps_the_previous_blocklist() {
        let engine = FilterEngine::new(Tier::Nano);
        let mut backend = crate::filter::engine::backend_for_tier(Tier::Nano);
        backend.add("keep.test");
        backend.seal();
        engine.replace_backend(backend);
        assert!(engine.is_blocked_at("keep.test", 0));

        let result = updater(
            vec!["http://127.0.0.1:1/hosts".to_owned()],
            Duration::from_secs(5),
        )
        .update(&engine)
        .await;

        assert!(result.is_err());
        assert!(engine.is_blocked_at("keep.test", 0));
    }

    #[tokio::test]
    async fn local_files_and_inline_entries_survive_a_remote_update() {
        let path = std::env::temp_dir().join(format!(
            "pihole-lite-gravity-{}-local.txt",
            std::process::id()
        ));
        std::fs::write(&path, "0.0.0.0 local.test\n").unwrap();

        let body = "remote.test\n";
        let addr = serve_once(format!(
            "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ))
        .await;

        let engine = FilterEngine::new(Tier::Nano);
        let updater = GravityUpdater::new(GravityConfig {
            urls: vec![format!("http://{addr}/hosts")],
            local_files: vec![path.clone()],
            inline: vec!["inline.test".to_owned()],
            fetcher: None,
            cache_dir: None,
            cache_max_age: None,
            interval: Duration::ZERO,
            source_timeout: Duration::from_secs(5),
            verbose: false,
        });

        updater
            .update(&engine)
            .await
            .expect("update should succeed");
        let _ = std::fs::remove_file(&path);

        assert!(engine.is_blocked_at("local.test", 0));
        assert!(engine.is_blocked_at("inline.test", 0));
        assert!(engine.is_blocked_at("remote.test", 0));
        assert_eq!(3, engine.stats().domains);
    }

    #[tokio::test]
    async fn oversized_lines_are_dropped_without_growing_the_buffer() {
        let long = format!("{}\nvalid.test\n", "a".repeat(4096));
        let addr = serve_once(format!(
            "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{long}",
            long.len()
        ))
        .await;

        let engine = FilterEngine::new(Tier::Nano);
        let report = updater(vec![format!("http://{addr}/hosts")], Duration::from_secs(5))
            .update(&engine)
            .await
            .expect("update should succeed");

        assert_eq!(1, report.domains);
        assert!(engine.is_blocked_at("valid.test", 0));
    }

    #[tokio::test]
    async fn validators_are_persisted_per_url() {
        let dir = temp_dir("validators");
        let cache = GravityCache::new(Some(dir.clone()), None);
        let validators = Validators {
            etag: Some("\"v1\"".to_owned()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_owned()),
        };

        cache
            .store_validators("http://example.org/a", &validators)
            .await
            .unwrap();

        assert_eq!(
            Some(validators),
            cache.validators("http://example.org/a").await
        );
        assert_eq!(None, cache.validators("http://example.org/b").await);
        let _ = std::fs::remove_dir_all(&dir);
    }


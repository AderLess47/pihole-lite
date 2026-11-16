impl GravityUpdater {
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

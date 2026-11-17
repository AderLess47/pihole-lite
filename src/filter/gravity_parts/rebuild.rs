/// Wie ein Neuaufbau den Remote-Teil behandelt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Start: lokale Dateien und vorhandene Caches, kein Netzwerkzugriff.
    Offline,
    /// Update: lokale Dateien, dann Remote-Quellen mit Cache-Revalidierung.
    Refresh,
}

/// Ein kompletter Neuaufbau des Bestands.
///
/// `preload` und `update` unterscheiden sich nur darin, woher der Remote-Teil kommt und
/// was ein leerer Bestand bedeutet. Alles andere – lokale Dateien, Inline-Domains,
/// `seal`, Zählung, Tausch – läuft durch denselben Weg. Vorher baute nur `update` die
/// Inline-Domains ein, `preload` nicht, und der Aufrufer musste die Lücke nach dem Start
/// von Hand flicken.
struct Rebuild<'a> {
    updater: &'a GravityUpdater,
    backend: Box<dyn FilterBackend>,
    totals: Totals,
    sources: usize,
}

impl<'a> Rebuild<'a> {
    fn new(updater: &'a GravityUpdater, tier: Tier) -> Self {
        Self {
            updater,
            backend: backend_for_tier(tier),
            totals: Totals::default(),
            sources: 0,
        }
    }

    /// Lokale Dateien: der Ausgangsbestand, den ein fehlgeschlagener Download nicht
    /// beschädigen darf.
    async fn local_files(&mut self) -> Result<(), GravityError> {
        let updater = self.updater;
        for path in &updater.config.local_files {
            let label = path.display().to_string();
            let mut source = updater.open_local(path).await?;
            let mut sink = CacheSink::disabled();
            updater
                .timed_read(
                    source.reader(),
                    self.backend.as_mut(),
                    &mut self.totals,
                    &label,
                    &mut sink,
                )
                .await?;
            source.finish(&label).await?;
            self.sources += 1;
        }
        Ok(())
    }

    /// Vorhandene Cache-Listen, ohne Netzwerk. Fehlende oder zu alte Caches werden
    /// übersprungen – dafür gibt es das spätere Update.
    async fn cached_lists(&mut self) -> Result<(), GravityError> {
        let updater = self.updater;
        for url in &updater.config.urls {
            let file = match updater.cache.open_list(url).await {
                CachedList::Fresh(file) => file,
                CachedList::Stale { age } => {
                    eprintln!(
                        "[gravity] {url}: cache ist {} alt und wird nicht geladen",
                        fmt_age(age)
                    );
                    continue;
                }
                CachedList::Missing => {
                    if updater.config.verbose {
                        eprintln!("[gravity] {url}: noch kein cache vorhanden");
                    }
                    continue;
                }
            };
            let mut reader = BufReader::new(file);
            let mut sink = CacheSink::disabled();
            updater
                .timed_read(
                    &mut reader,
                    self.backend.as_mut(),
                    &mut self.totals,
                    url,
                    &mut sink,
                )
                .await?;
            self.sources += 1;
        }
        Ok(())
    }

    /// Remote-Quellen mit Netzwerk: `304` nutzt die Cache-Liste, `2xx` streamt und
    /// erneuert sie.
    async fn remote(&mut self) -> Result<(), GravityError> {
        let updater = self.updater;
        for url in &updater.config.urls {
            updater
                .refresh_source(url, self.backend.as_mut(), &mut self.totals)
                .await?;
            self.sources += 1;
        }
        Ok(())
    }

    /// Inline-Domains aus der Config. Sie gehören in jeden Aufbau, sonst verliert der
    /// Tausch sie.
    fn inline_domains(&mut self) {
        for domain in &self.updater.config.inline {
            self.backend.add(domain);
        }
    }

    /// `seal` plus die Kennzahlen des Laufs; ob getauscht wird, entscheidet der Aufrufer.
    fn finish(mut self, started: Instant) -> (Box<dyn FilterBackend>, UpdateReport) {
        self.backend.seal();
        let report = UpdateReport {
            domains: self.backend.len(),
            lines: self.totals.lines,
            skipped: self.totals.skipped,
            sources: self.sources,
            duration: started.elapsed(),
            heap_bytes: self.backend.heap_bytes(),
        };
        (self.backend, report)
    }
}

impl GravityUpdater {
    /// Baut den Bestand neu auf und tauscht ihn erst nach Erfolg in die Engine.
    async fn rebuild(
        &self,
        engine: &FilterEngine,
        mode: Mode,
    ) -> Result<UpdateReport, GravityError> {
        let started = Instant::now();
        let mut build = Rebuild::new(self, engine.tier());
        build.local_files().await?;
        match mode {
            Mode::Offline => build.cached_lists().await?,
            Mode::Refresh => build.remote().await?,
        }
        build.inline_domains();
        let (backend, report) = build.finish(started);

        if report.domains == 0 {
            // Ein leerer Bestand ist beim Update ein Fehler: der alte ist besser als
            // keiner. Beim Start heißt leer nur "nichts da", die Engine bleibt, wie sie ist.
            return match mode {
                Mode::Refresh => Err(GravityError::Empty),
                Mode::Offline => Ok(report),
            };
        }
        engine.replace_backend(backend);

        // Erst nach erfolgreichem Tausch aufräumen: bis dahin ist der alte Bestand die
        // Rückfalllinie und seine Dateien dürfen nicht verschwinden.
        if mode == Mode::Refresh {
            match self.prune_cache().await {
                Ok(0) => {}
                Ok(removed) => println!("[gravity] {removed} verwaiste cache-datei(en) entfernt"),
                Err(err) => eprintln!("[gravity] cache-aufräumen fehlgeschlagen: {err}"),
            }
        }
        Ok(report)
    }
}

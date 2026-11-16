
/// Offene Quelle: lokale Datei oder Fetcher-Prozess.
///
/// HTTP-Quellen werden direkt in [`GravityUpdater::refresh_source`] gelesen, weil dort
/// Statuscode und Validatoren gebraucht werden.
enum Source {
    File(BufReader<File>),
    Fetcher {
        reader: BufReader<ChildStdout>,
        child: Child,
    },
}

impl Source {
    fn reader(&mut self) -> &mut (dyn AsyncBufRead + Unpin + Send) {
        match self {
            Self::File(reader) => reader,
            Self::Fetcher { reader, .. } => reader,
        }
    }

    /// Schließt die Quelle ab und prüft bei einem Fetcher den Exit-Status.
    async fn finish(self, url: &str) -> Result<(), GravityError> {
        match self {
            Self::File(_) => Ok(()),
            Self::Fetcher { mut child, .. } => {
                let status = child.wait().await.map_err(GravityError::Io)?;
                if !status.success() {
                    return Err(GravityError::Http {
                        url: url.to_owned(),
                        reason: format!("fetcher beendet mit {status}"),
                    });
                }
                Ok(())
            }
        }
    }
}

/// Senke für die Cache-Datei: schreibt akzeptierte Domains mit, ohne den Update-Lauf
/// an einen Schreibfehler zu binden.
struct CacheSink<'a> {
    file: Option<&'a mut File>,
    failed: bool,
}

impl<'a> CacheSink<'a> {
    fn disabled() -> Self {
        Self {
            file: None,
            failed: false,
        }
    }

    async fn write_bytes(&mut self, bytes: &[u8]) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if let Err(err) = file.write_all(bytes).await {
            eprintln!("[gravity] cache-schreibfehler: {err}; update läuft ohne cache weiter");
            self.failed = true;
            self.file = None;
        }
    }
}

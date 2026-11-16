# Betrieb, Build und Releases

## Betrieb

- **Neustart filtert sofort.** Der Bestand wird vor dem Socket-Aufbau aus
  `blocklist_file` und dem Platten-Cache geladen; ein fehlgeschlagener Download lässt den
  vorherigen Bestand unangetastet und wird beim nächsten Intervall erneut versucht.
- **Ein Cache-Verzeichnis gehört genau einer Instanz.** Zwei Prozesse auf demselben
  `gravity_cache_dir` überschreiben sich gegenseitig.
- **SD-Karte.** Der Cache wird nur beim Update geschrieben (einmal pro Intervall). Eine
  einzelne, große Quelle ist auf einem Pi 1 die richtige Wahl; jede weitere Quelle kostet
  Downloadzeit und RSS beim Einlesen.
- **Update des Binaries:** neue Version herunterladen, Prüfsumme verifizieren, nach
  `/usr/local/bin/pihole-lite` kopieren, `sudo systemctl restart pihole-lite`.

## Selbst bauen

Auf dem Pi selbst (dauert dort lange, LTO braucht RAM):

```sh
cargo build --release
```

Cross-Build für ARMv6 von einem x86-Rechner mit `gcc-arm-linux-gnueabihf`:

```sh
rustup target add arm-unknown-linux-gnueabihf
export CARGO_TARGET_ARM_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc
cargo build --release --target arm-unknown-linux-gnueabihf
```

Das Release-Profil (`opt-level = "s"`, LTO, `panic = "abort"`, `strip`) ist auf Größe
optimiert. Für den Pi zählt außerdem die glibc des Build-Systems, denn dagegen wird
gelinkt: ein Build auf einer aktuellen Distribution verlangt deren glibc und startet auf
älteren Pi-Images nicht. Die CI baut deshalb in einem `debian:bookworm`-Container
(glibc 2.36) und bricht ab, wenn das Binary ein neueres Symbol als `GLIBC_2.36` braucht.

## Checks und Releases

Die CI (`.github/workflows/ci.yml`) läuft bei jedem Push auf `main` und bei Pull Requests:

- `fmt, clippy, test` – `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`
- `ARMv6 cross build` – Clippy und Release-Build gegen `arm-unknown-linux-gnueabihf`,
  `file`-Prüfung, glibc-Guard, Paketierung mit `sha256sum`, Upload als Workflow-Artefakt

Ein Tag `v*` baut dasselbe Binary und hängt es samt Prüfsumme dauerhaft an das Release:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

Der Release-Job (`softprops/action-gh-release`) veröffentlicht nur das geprüfte Artefakt
des ARMv6-Jobs – kein zweiter Build, keine abweichenden Bytes.

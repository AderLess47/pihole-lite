# Changelog

Alle nennenswerten Änderungen an diesem Projekt. Aufbau nach
[Keep a Changelog](https://keepachangelog.com/de/1.1.0/), Versionen nach
[Semantic Versioning](https://semver.org/lang/de/).

Der Abschnitt einer Version wird beim Tag-Push als Release-Beschreibung
veröffentlicht; die Überschrift muss deshalb genau `## [<version>] - <datum>`
lauten.

## [Unreleased]

### Changed

- CLI- und TOML-Auswertung liegt in `config.rs`: Defaults, Präzedenz und
  Bereichsprüfungen an einer Stelle, `main.rs` verdrahtet nur noch.
- Preload und Update teilen sich einen Aufbau (`Rebuild`), damit lokale Dateien
  und Inline-Domains in beiden Wegen gleich behandelt werden; `main` muss nach
  `preload` nichts mehr nachtragen.
- Die Query-Entscheidung liegt in `dns/resolve.rs`: `resolve` liefert ein `Reply`
  mit Bytes, Verdict und dem Namen im Puffer, der Socket-Loop sendet und bucht.
- Filter und Abschaltung bekommen die Zeit als Parameter (`now` aus
  `now_seconds()`) statt sie im Entscheid zu lesen; damit ist der Ablauf einer
  Abschaltung ohne Warten testbar.
- Der Gravity-Updater ist in echte Module unter `filter/gravity/` geschnitten
  (`update`, `rebuild`, `stream`, `source`, `cache`, `http`) statt in per
  `include!` eingebundene Fragmente.

## [0.1.0] - 2026-11-16

### Added

- DNS-Sinkhole über UDP mit den Sinkhole-Modi `zero` und `nxdomain` und
  konfigurierbarer Sinkhole-TTL.
- Hardware-Tiers aus `/proc/meminfo`: Nano (≤ 300 MiB, ein Worker,
  512 Cache-Einträge, sortierter xxh3-Hash), Balanced (≤ 1 GiB, zwei Worker,
  4096 Einträge, Radix-Trie), Pro (> 1 GiB, bis vier Worker, 16384 Einträge).
- Antwort-Cache als LRU mit TTL-Kappung und Alterung; negative Antworten werden
  60 s gehalten, Fehler-, Truncation- und 0-TTL-Antworten nicht.
- Blocklisten-Filter mit Streaming-Parser (Hosts-Format oder eine Domain pro
  Zeile); ein Treffer blockt die Domain und alle Subdomains.
- Gravity-Updater: `http://` nativ, `https://` über einen externen Fetcher
  (Default `curl`), Tausch erst nach erfolgreichem `seal()`. Pro Quelle eine
  Cache-Datei mit ETag/Last-Modified-Revalidierung, Prune und Maximalalter.
- Mini-HTTP-API mit `/api/stats` und `/api/disable?seconds=N`, dazu ein
  Dashboard als eine eingebettete HTML-Datei ohne Build-Schritt.
- systemd-Unit unter `deploy/pihole-lite.service`: eigener Benutzer, nur
  `CAP_NET_BIND_SERVICE`, `ProtectSystem=strict` und weitere Härtung.
- ARMv6-Cross-Build (`arm-unknown-linux-gnueabihf`) mit Clippy, `file`-Prüfung
  und Release-Artefakt samt `sha256sum`.

### Fixed

- Das Release-Binary wird gegen glibc 2.36 (Debian Bookworm) gebaut. Der erste
  Build auf `ubuntu-latest` verlangte `GLIBC_2.39` und wäre auf Raspberry Pi OS
  nicht gestartet; ein CI-Guard bricht jetzt ab, sobald ein Symbol neuer als die
  Baseline ist.

[Unreleased]: https://github.com/AderLess47/pihole-lite/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/AderLess47/pihole-lite/releases/tag/v0.1.0

# AGENTS.md – Architektur-Regeln für pihole-lite

Ziel: ein DNS-Sinkhole, das auf einem Raspberry Pi 1 (ARMv6, 256 MB RAM) unter **15 MB RSS**
läuft und auf größeren Systemen automatisch skaliert. Jede Änderung wird an diesem Ziel
gemessen, nicht an Bequemlichkeit.

## Harte Regeln

1. **Tier-Entscheidungen ausschließlich in `src/profiler.rs`.** Kein anderer Code liest
   `/proc/meminfo` oder schaltet anhand von RAM-Grenzen um. Grenzen:
   Tier 1 Nano `<= 300 MiB`, Tier 2 Balanced `<= 1 GiB`, Tier 3 Pro `> 1 GiB`.
2. **Kein Heap pro Query** (einzige Ausnahme: der Request-Buffer, den `tokio::spawn`
   besitzt). Query-Namen werden in einem Stack-Buffer normalisiert, nie per `to_string()`.
3. **Filter nur über das `FilterBackend`-Trait.** Neue Backends (z. B. Regex-Layer für
   Tier 3) implementieren das Trait, statt `FilterEngine` zu verzweigen.
4. **Blocklisten werden niemals komplett in den RAM geladen.** Zeilenweise streamen
   (`BufRead::read_line` / `tokio::io::AsyncBufReadExt`), sofort hashen bzw. einfügen,
   Zeilenpuffer wiederverwenden.
5. **Keine Panics im Query-Pfad.** Fehlerhafte Pakete werden geloggt und verworfen oder mit
   FORMERR/SERVFAIL beantwortet. `unwrap`/`expect` nur in `main`, bei bewiesenen
   Invarianten oder in Tests.
6. **Dependencies:** `tokio` (nur die Features aus `Cargo.toml`), `simple-dns`,
   `xxhash-rust`. Alles Weitere braucht Begründung **und** eine RSS-Messung. In Tier 1
   ohne Messung tabu: `regex`, `serde`, `hyper`/`axum`, `tracing`.
7. **Konfiguration** kommt aus `config.toml` bzw. CLI-Flags, nicht aus verstreuten
   Konstanten.
8. **Modulstruktur bleibt:** `dns/` (server, upstream, cache), `filter/` (engine, gravity),
   `stats/` (ringbuffer), `web/` (api, static).

## Statistik & API

`stats/ringbuffer.rs` zählt Queries, Blocks, Cache-Treffer, Forwardings und Fehler in
Atomics und hält die letzten N Queries in einem Ring fester Größe (Seqlock pro Slot:
Schreiber blockieren nie, Leser verwerfen tearing-anfällige Slots). Namen werden auf
96 Byte gekürzt; nur ein Snapshot allokiert Strings. `web/api.rs` ist ein handgebauter
HTTP/1.1-Server ohne Framework, mit Header-Cap, Read-Timeout und Verbindungslimit:
`/api/stats` liefert JSON, `/api/disable?seconds=N` schaltet den Filter über
`FilterEngine::disable_for` ab (`seconds=0` wieder an). Das Dashboard unter `/` ist eine
einzige, per `include_str!` eingebettete HTML-Datei mit Inline-CSS/-JS: kein Asset-Ordner,
kein Build-Schritt, keine CDN-Abhängigkeit. Der Endpunkt bindet standardmäßig auf
`127.0.0.1`, weil der Disable-Switch keine Authentifizierung hat.

## Antwort-Cache

`dns/cache.rs` hält rohe Antworten im LRU; die Kapazität kommt aus dem Tier (512 / 4096 /
16384), `cache_entries` kann sie überschreiben. Lookup und Insert sind O(1) über ein
Slot-Slab plus HashMap auf `(xxh3(name), qtype)`; der Name wird im Slot verglichen, eine
Hash-Kollision ist ein Miss. Pro Lookup entsteht nur die Kopie, die ohnehin gesendet wird.
TTLs werden im Wire-Format gekappt und gealtert – OPT-Records werden übersprungen, weil
ihr TTL-Feld EDNS-Flags trägt. Gecacht wird nur, was `cache_ttl` erlaubt: Fehler-RCODE,
Truncation, 0-TTL und Zone-Transfers bleiben draußen, negative Antworten bekommen 60 s.
Einfügen passiert erst nach dem Senden an den Client.

## Gravity-Transport

`http://` wird nativ gelesen (HTTP/1.0 + `Connection: close`, Body endet mit dem Socket,
kein Chunked-Framing). `https://` läuft über einen externen Fetcher (`curl -fsSL`,
konfigurierbar) und streamt dessen stdout. Grund: ein TLS-Stack ist auf ARMv6 mit 256 MB
die falsche Investition, und der Download ist ein periodischer Job – der Parser bleibt für
beide Wege derselbe (`fill_buf`/`consume`, harter Zeilen-Cap, kein Volltext im RAM).
Updates tauschen das Backend erst nach erfolgreichem `seal()` aus; ein fehlgeschlagener
Lauf lässt den vorherigen Bestand unangetastet.

Persistenz: pro Quelle liegen `<hash>.list` und `<hash>.meta` unter `gravity_cache_dir`.
`preload` baut den Bestand daraus **ohne Netzwerk** auf, bevor der DNS-Socket aufgeht.
`http://`-Quellen werden mit `If-None-Match` / `If-Modified-Since` revalidiert; ein `304`
liest die Cache-Datei statt erneut zu laden. Der Fetcher bekommt bewusst keine Conditional
Header – ohne Statuszeile wäre eine leere `304`-Antwort nicht von einer leeren Liste zu
unterscheiden. Cache-Schreibfehler sind nie fatal, nur die Persistenz fehlt dann.

Wartung: Nach einem erfolgreichen Tausch entfernt `GravityCache::prune` alle
`<hash>.{list,meta}` ohne passende URL sowie liegengebliebene `.tmp`-Reste. Dateien, deren
Name nicht auf unser Schema passt, werden nie angefasst. Listen älter als
`gravity_cache_max_age_hours` (Default 168) werden beim Start verworfen und nie
revalidiert – sonst würde ein `304` eine beliebig alte Liste zementieren. `0` schaltet
das Alterlimit ab. Ein Cache-Verzeichnis gehört genau einer Instanz.

## Checks vor jedem Commit

```sh
cargo fmt
cargo clippy -- -D warnings
cargo test
cargo build --release
```

Für den Pi 1: `cargo build --release --target arm-unknown-linux-gnueabihf`
(ARMv6 + softfloat, passendes Linker-Image vorausgesetzt).

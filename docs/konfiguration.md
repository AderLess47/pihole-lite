# Konfiguration

`pihole-lite` liest `./config.toml` (überschreibbar mit `--config`); fehlt die Datei,
gelten die Defaults. Jeder Key ist optional, die kommentierte Vorlage ist
[`../config.toml`](../config.toml).

| Key | Default | Bedeutung |
|---|---|---|
| `listen` | `0.0.0.0:53` | UDP-Listener |
| `upstream` | `["1.1.1.1:53", "9.9.9.9:53"]` | Resolver in Reihenfolge, der erste gültige gewinnt |
| `sinkhole` | `"zero"` | `zero` (A→0.0.0.0, AAAA→::) oder `nxdomain` |
| `sinkhole_ttl` | `60` | TTL der Sinkhole-Antwort |
| `upstream_timeout_ms` | `2000` | Timeout pro Upstream-Versuch |
| `cache_entries` | Tier | Antwort-Cache-Einträge, `0` = aus dem Tier |
| `max_in_flight` | Tier | gleichzeitige Queries, `0` = aus dem Tier |
| `api_listen` | `127.0.0.1:8080` | HTTP-API, `""` = aus |
| `blocklist_file` | – | lokale Liste (Hosts-Format oder eine Domain/Zeile), wird gestreamt |
| `block` | – | Inline-Domains |
| `blocklist` | – | Remote-Listen für den Gravity-Updater |
| `gravity_interval_hours` | `24` | Update-Intervall, `0` = nur beim Start |
| `gravity_timeout_seconds` | `300` | Gesamt-Timeout pro Quelle |
| `gravity_cache_dir` | `./gravity-cache` | Platten-Cache pro Quelle, `""` = aus |
| `gravity_cache_max_age_hours` | `168` | ältere Listen werden verworfen, `0` = unbegrenzt |
| `fetcher` | `curl -fsSL --max-time 120` | externer Fetcher für `https://` |
| `verbose` | `false` | Pro-Query-Logging – auf dem Pi 1 aus lassen |

CLI-Flags (`--listen`, `--upstream`, `--blocklist-file`, `--verbose`, `--config`) haben
Vorrang vor der Datei. `--help` zeigt die Usage.

Ein brauchbarer Start für den Pi 1:

```toml
listen = "0.0.0.0:53"
upstream = ["1.1.1.1:53", "9.9.9.9:53"]
api_listen = "127.0.0.1:8080"
gravity_cache_dir = "/var/lib/pihole-lite/gravity-cache"
gravity_interval_hours = 24
blocklist = ["https://raw.githubusercontent.com/StevenBlack/hosts/master/hosts"]
verbose = false
```

Nach einer Änderung: `sudo systemctl restart pihole-lite`.

Betriebshinweise, Selbstbau und der Release-Ablauf stehen in
[`betrieb.md`](betrieb.md).

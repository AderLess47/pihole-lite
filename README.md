# pihole-lite

DNS-Sinkhole in Rust, gebaut für einen **Raspberry Pi 1 (ARMv6, 256 MB RAM)** – unter
15 MB RSS. Auf größeren Hosts skaliert es selbstständig über drei Hardware-Tiers.

Enthalten: UDP-Resolver mit Antwort-Cache, Blocklisten-Filter, Gravity-Updater mit
Platten-Cache und Conditional GET, Mini-HTTP-API mit Dashboard. Keine Datenbank, kein
Web-Framework, kein TLS-Stack, keine Runtime-Abhängigkeiten außer `libc`.

| Tier | RAM | Worker | Antwort-Cache | Stats-Ring |
|---|---|---|---|---|
| Nano | ≤ 300 MiB | 1 | 512 | klein |
| Balanced | ≤ 1 GiB | 2 | 4096 | mittel |
| Pro | > 1 GiB | alle Kerne (max. 4) | 16384 | groß |

Das Tier wählt `src/profiler.rs` beim Start anhand von `/proc/meminfo` – die einzige
Stelle im Code, die RAM-Grenzen kennt.

## Installation auf dem Pi (v0.1.0)

Voraussetzungen: Raspberry Pi OS / Debian **armhf** (hard-float), `curl` für
`https://`-Blocklisten, Port 53 frei.

### 1. Binary und Prüfsumme herunterladen

```sh
mkdir -p /tmp/pihole-lite && cd /tmp/pihole-lite
wget https://github.com/AderLess47/pihole-lite/releases/download/v0.1.0/pihole-lite-armv6
wget https://github.com/AderLess47/pihole-lite/releases/download/v0.1.0/pihole-lite-armv6.sha256
sha256sum -c pihole-lite-armv6.sha256
```

Erwartet: `pihole-lite-armv6: OK`. Die Prüfsumme des Release-Assets ist
`8df6b6c192b84707446b83143b5027b2b4a4e4264e4de147dc6f8ba968388637` (818.440 Bytes,
`ELF 32-bit LSB pie executable, ARM, EABI5`, hard-float, gestrippt). Das Binary ist eine
einzelne Datei, dynamisch gegen glibc gelinkt; das neueste benötigte Symbol ist
`GLIBC_2.34`. Es läuft damit auf Raspberry Pi OS Bookworm (auch dem 32-Bit-Image für
Pi 1/Zero) und allem Neueren — unter Bullseye braucht es einen eigenen Build. Benötigt
werden nur `libc`, `libm` und `libgcc_s`, die auf jedem Pi OS vorhanden sind.

### 2. Dateien ablegen

```sh
sudo install -m 0755 pihole-lite-armv6 /usr/local/bin/pihole-lite

sudo install -d /etc/pihole-lite
sudo install -m 0644 /pfad/zum/repo/config.toml /etc/pihole-lite/config.toml

sudo useradd --system --home-dir /var/lib/pihole-lite --shell /usr/sbin/nologin pihole-lite
sudo install -d -o pihole-lite -g pihole-lite /var/lib/pihole-lite

sudo install -m 0644 /pfad/zum/repo/deploy/pihole-lite.service /etc/systemd/system/pihole-lite.service
```

Die mitgelieferte Unit [`deploy/pihole-lite.service`](deploy/pihole-lite.service) läuft als
eigener Benutzer `pihole-lite`, bekommt nur `CAP_NET_BIND_SERVICE` (Port 53 ohne root),
arbeitet in `/var/lib/pihole-lite` und ist mit `ProtectSystem=strict`, `NoNewPrivileges`
und `PrivateTmp` gehärtet. Dort liegt auch der Gravity-Cache (`./gravity-cache`); für einen
anderen Pfad in `/etc/pihole-lite/config.toml` einen absoluten Wert setzen:

```toml
gravity_cache_dir = "/var/lib/pihole-lite/gravity-cache"
```

### 3. Dienst starten

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now pihole-lite
systemctl status pihole-lite --no-pager
journalctl -u pihole-lite -f
```

Im Journal steht der Startbanner mit Tier, Domain-Anzahl, Listener und Upstreams:

```
pihole-lite 0.1.0
  hardware : 246 MiB RAM (180 MiB frei), 1 Kern(e), 1 Worker
  tier     : Tier 1 (Nano) -> nanohash (sorted xxh3-64)
  filter   : 61234 domains, ~512 KiB heap
  dns      : udp 0.0.0.0:53 (sinkhole zero, ttl 60s, max 16 parallel, cache 512 antworten)
  api      : http://127.0.0.1:8080/api/stats
  upstream : 1.1.1.1:53, 9.9.9.9:53 (timeout 2000 ms)
```

### 4. Pi als DNS-Server eintragen

Im Router (oder pro Gerät) die IPv4-Adresse des Pi als DNS-Server setzen, dann prüfen:

```sh
dig @192.168.1.2 ads.example.com +short
dig @192.168.1.2 heise.de +short
```

Ist Port 53 belegt (`sudo ss -lunp | grep ':53'`), vorher den störenden Dienst abschalten
– bei `systemd-resolved` z. B. `DNSStubListener=no` in `/etc/systemd/resolved.conf`.

## Dashboard und API

Die API bindet nur auf `127.0.0.1:8080`, weil `/api/disable` keine Authentifizierung hat.

| Route | Zweck |
|---|---|
| `GET /` | Dashboard (eine eingebettete HTML-Datei, kein CDN) |
| `GET /api/stats` | Zähler, Cache-Trefferquote, letzte Queries als JSON |
| `GET /api/disable?seconds=N` | Filter für N Sekunden aus, `seconds=0` wieder an |

Für das Dashboard im Browser einen SSH-Tunnel legen und `http://127.0.0.1:8080/` öffnen:

```sh
ssh -L 8080:127.0.0.1:8080 pi@192.168.1.2
```

## Weiter

Konfigurationsschlüssel, Betriebshinweise, Selbstbau und Release-Ablauf stehen in
[`docs/konfiguration.md`](docs/konfiguration.md); die kommentierte Vorlage ist
[`config.toml`](config.toml).

## Lizenz

MIT (siehe `license` in `Cargo.toml`).

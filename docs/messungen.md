# Messungen

Die Zusage des Projekts — unter 15 MB RSS auf einem Pi 1 mit 256 MB — ist nur so
viel wert vie die Zahl, die sie belegt. Dieses Protokoll erzeugt diese Zahl
reproduzierbar: ein Lauf ohne Blockliste, ein Lauf mit 100.000 Domains, gleiche
Konfiguration, gleiches Binary.

## Ausführen

Auf dem Pi im Repo-Wurzelverzeichnis:

```sh
bash bench/messung.sh                     # Release-Build, 10 s Last pro Lauf
bash bench/messung.sh --seconds 30 --threads 8
```

Das Skript baut bei Bedarf `target/release/pihole-lite`, kompiliert den
Lastgenerator `bench/loadgen.rs` (nur `std`, keine Ab��P8digkeiten), erzeugt beide
Listen, startet den Server auf `127.0.0.1:5353` mit API auf `127.0.0.1:18080`,
misst und beendet ihn wieder. Ergebnisse landen in `bench/out/ergebnis.md`, die
Rohdaten daneben.

Voraussetzungen: `curl` (für die Bereitschaftsprüfung), `rustc` zum Bauen des
Lastgenerators, freie Ports 5353/udp und 18080/tcp.

## Was gemessen wird

| Wert | Herkunft | Bedeutung |
|---|---|---|
| RSS idle | `VmRSS` in `/proc/<pid>/status` | residenter Speicher nach dem Laden der Liste, ohne Last |
| Peak idle | `VmHWM` | höchster Stand während des Ladens |
| RSS Last | `VmRSS` nach dem Lauf | residenter Speicher unter Last |
| Peak Last | `VmHWM` | Speicherspitze über die gesamte Laufzeit |
| QPS | `bench/loadgen` | beantwortete Queries pro Sekunde im Filter-Pfad |

Der Lastgenerator fragt immer dieselben 4096 Namen ab. Beim 100k-Lauf sind sie
geblockt, die Antwort kommt also lokal aus dem Filter; beim Leerlisten-Lauf geht
dieselbe Anfrage zum Upstream. Die beiden QPS-Zahlen messen deshalb verschiedene
Dinge — lokale Antwort gegen Netzlaufzeit — und sind nur innerhalb desselben
Laufs über Versionen hinweg vergleichbar. Verglichen wird zwischen den Läufen der
RSS, nicht die QPS.

## Ergebnistabelle

Auf dem Pi ausfüllen und hier ablegen:

| Lauf | Domains | RSS idle (MiB) | Peak (MiB) | RSS Last (MiB) | QPS |
|---|---|---|---|---|---|
| Weiterleitung (leere Liste) | 0 | — | — | — | — |
| 100k Domains (Sinkhole) | 100.000 | — | — | — | — |

Dazu gehören immer: Pi-Modell und RAM, `uname -srm`, glibc-Version, Kernel,
`gpu_mem`-Einstellung und ob der Lauf gedrosselt wurde (`vcgencmd get_throttled`).
Das Skript schreibt diese Angaben in den Kopf von `ergebnis.md`.

## Reproduzierbarkeit

- Drei Läufe, Median nehmen; der erste Lauf nach dem Booten ist regelmäßig
  schlechter (Page-Cache kalt, SD-Karte langsam).
- Nur Release-Builds messen. Ein Debug-Build sagt nichts über RSS oder QPS.
- `gpu_mem=16` in `/boot/firmware/config.txt`, kein Desktop — sonst fehlt dem
  Server der RAM, den die Grafik belegt.
- Temperatur im Blick behalten: ab etwa 80 °C drosselt der Pi 1 und die QPS fallen,
  ohne dass der Code schlechter geworden ist.
- Gleiche Liste, gleiche Domain-Anzahl, gleicher Modus. Ein Vergleich „100k gegen
  80k" ist kein Vergleich.

## Wenn die Zahl nicht passt

- **RSS über 15 MB:** zuerst `tier` im Startbanner prüfen. Auf einem Pi mit 256 MB
  muss dort `Tier 1 (Nano)` stehen; alles andere heißt, dass `/proc/meminfo` nicht
  gelesen werden konnte und der Fallback greift.
- **RSS wächst mit der Liste, aber nicht linear:** das ist erwartet (acht Byte pro
  Domain im Nano-Backend), ein Sprung deutet auf doppelte Einträge hin.
- **QPS nahe null:** Blockliste prüfen (`filter: N domains` im Banner) und ob der
  Name im Test wirklich geblockt wird.

## Grenzen

Die QPS-Zahl entsteht über das Loopback-Interface mit einem Lastgenerator auf
demselben Gerät; beide teilen sich CPU und Netzwerk-Stack. Für Kapazitätsplanung
ist sie eine brauchbare Untergrenze, für einen Vergleich zwischen zwei Builds auf
derselben Hardware ist sie belastbar.

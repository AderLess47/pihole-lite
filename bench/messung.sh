#!/usr/bin/env bash
# Messprotokoll für RSS und QPS (Details: docs/messungen.md).
#
# Vergleicht zwei Läufe mit identischer Konfiguration: ohne Blockliste und mit
# einer generierten Liste aus 100k Domains. Schreibt Tabelle und Umgebung nach
# bench/out/.
#
#   bash bench/messung.sh                      # Standard: Release-Build, 10 s Last
#   bash bench/messung.sh --seconds 5 --threads 4
#   bash bench/messung.sh --binary /usr/local/bin/pihole-lite --out /tmp/messung
set -euo pipefail

cd "$(dirname "$0")/.."

BINARY="target/release/pihole-lite"
OUT="bench/out"
RUN_SECONDS=10
THREADS=8
DOMAINS=100000

while [ $# -gt 0 ]; do
    case "$1" in
        --binary) BINARY="$2"; shift 2 ;;
        --out) OUT="$2"; shift 2 ;;
        --seconds) RUN_SECONDS="$2"; shift 2 ;;
        --threads) THREADS="$2"; shift 2 ;;
        --domains) DOMAINS="$2"; shift 2 ;;
        -h|--help) sed -n '2,11p' "$0"; exit 0 ;;
        *) echo "unbekanntes argument: $1" >&2; exit 2 ;;
    esac
done

mkdir -p "$OUT"

# rustup legt die Werkzeuge in ~/.cargo/bin ab, das in nicht-interaktiven Shells
# oft nicht im PATH steht.
CARGO=$(command -v cargo || true)
[ -n "$CARGO" ] || CARGO="$HOME/.cargo/bin/cargo"
RUSTC=$(command -v rustc || true)
[ -n "$RUSTC" ] || RUSTC="$HOME/.cargo/bin/rustc"

if [ ! -x "$BINARY" ]; then
    echo "[messung] $BINARY fehlt, baue Release (nur Release-Zahlen zählen)"
    "$CARGO" build --release
fi
[ -x "$OUT/loadgen" ] || "$RUSTC" -O --edition 2021 bench/loadgen.rs -o "$OUT/loadgen"

EMPTY_LIST="$OUT/blocklist-empty.txt"
FULL_LIST="$OUT/blocklist-$DOMAINS.txt"
: > "$EMPTY_LIST"
awk -v n="$DOMAINS" 'BEGIN { for (i = 0; i < n; i++) printf "blocked-%d.example.com\n", i }' \
    > "$FULL_LIST"

write_config() {
    cat > "$OUT/config.toml" <<EOF
listen = "127.0.0.1:5353"
upstream = ["1.1.1.1:53"]
api_listen = "127.0.0.1:18080"
sinkhole = "zero"
cache_entries = 0
gravity_cache_dir = ""
blocklist_file = "$1"
verbose = false
EOF
}

status_kb() { awk -v key="$2" '$1 == key { print $2 }' "/proc/$1/status"; }
to_mib() { awk -v kb="${1:-0}" 'BEGIN { printf "%.1f", kb / 1024 }'; }

wait_ready() {
    local pid="$1" tries=0
    until curl -fsS -o /dev/null "http://127.0.0.1:18080/api/stats" 2>/dev/null; do
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "[messung] server ist beim start gestorben" >&2
            return 1
        fi
        tries=$((tries + 1))
        if [ "$tries" -gt 240 ]; then
            echo "[messung] server wurde nicht rechtzeitig bereit" >&2
            return 1
        fi
        sleep 0.25
    done
}

measure() {
    local label="$1" list="$2" pid rss_idle hwm_idle rss_load hwm_load tier qps
    write_config "$list"

    "$BINARY" --config "$OUT/config.toml" > "$OUT/$label-server.log" 2>&1 &
    pid=$!
    if ! wait_ready "$pid"; then
        kill "$pid" 2>/dev/null || true
        return 1
    fi

    tier=$(grep -m1 '^  tier' "$OUT/$label-server.log" | sed 's/.*: //' || true)
    rss_idle=$(to_mib "$(status_kb "$pid" 'VmRSS:')")
    hwm_idle=$(to_mib "$(status_kb "$pid" 'VmHWM:')")

    "$OUT/loadgen" --server 127.0.0.1:5353 --seconds "$RUN_SECONDS" \
        --threads "$THREADS" --mode blocked > "$OUT/$label-load.txt"
    qps=$(awk -F= '/^qps=/ { print $2 }' "$OUT/$label-load.txt")
    rss_load=$(to_mib "$(status_kb "$pid" 'VmRSS:')")
    hwm_load=$(to_mib "$(status_kb "$pid" 'VmHWM:')")

    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true

    printf '| %s | %s | %s | %s | %s | %s | %s |\n' \
        "$label" "$(wc -l < "$list")" "$rss_idle" "$hwm_idle" "$rss_load" "$hwm_load" "$qps" \
        >> "$OUT/rows.md"
    printf '| %s | %s | %s |\n' "$label" "$tier" "$qps" >> "$OUT/tiers.md"
}

: > "$OUT/rows.md"
: > "$OUT/tiers.md"

measure "Weiterleitung (leere Liste)" "$EMPTY_LIST"
measure "100k Domains (Sinkhole)" "$FULL_LIST"

{
    echo "# Messergebnis"
    echo
    echo "- Zeitpunkt: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "- Host: $(uname -srm)"
    if [ -r /proc/device-tree/model ]; then
        echo "- Modell: $(tr -d '\0' < /proc/device-tree/model)"
    fi
    echo "- RAM: $(awk '/^MemTotal:/ { printf "%.1f MiB", $2 / 1024 }' /proc/meminfo)"
    glibc=$(ldd --version 2>/dev/null | head -1 || true)
    echo "- glibc: ${glibc:-unbekannt}"
    echo "- Binary: \`$(sha256sum "$BINARY" | cut -d' ' -f1)\`"
    echo "- Last: ${THREADS} Threads, ${RUN_SECONDS} s pro Lauf, Modus \`blocked\`, Liste mit ${DOMAINS} Domains"
    if command -v vcgencmd >/dev/null 2>&1; then
        echo "- Temperatur: $(vcgencmd measure_temp 2>/dev/null || echo unbekannt)"
        echo "- Drosselung: $(vcgencmd get_throttled 2>/dev/null || echo unbekannt)"
    fi
    echo
    echo "| Lauf | Domains | RSS idle (MiB) | Peak idle (MiB) | RSS Last (MiB) | Peak Last (MiB) | QPS |"
    echo "|---|---|---|---|---|---|---|"
    cat "$OUT/rows.md"
    echo
    echo "Tier pro Lauf:"
    echo
    echo "| Lauf | Tier | QPS |"
    echo "|---|---|---|"
    cat "$OUT/tiers.md"
} | tee "$OUT/ergebnis.md"

echo
echo "[messung] Rohergebnisse: $OUT/"

#!/usr/bin/env bash
# Setzt die Messwerte aus bench/out/ergebnis.md in die Ergebnistabelle von
# docs/messungen.md ein. Aufruf: bash bench/eintragen.sh [ergebnis.md]
set -euo pipefail

cd "$(dirname "$0")/.."

SOURCE="${1:-bench/out/ergebnis.md}"
TARGET="docs/messungen.md"
START='<!-- messwerte:start -->'
END='<!-- messwerte:end -->'

[ -f "$SOURCE" ] || {
    echo "fehlt: $SOURCE — erst \`bash bench/messung.sh\` auf dem Pi laufen lassen" >&2
    exit 1
}
[ -f "$TARGET" ] || {
    echo "fehlt: $TARGET" >&2
    exit 1
}
if ! grep -qxF "$START" "$TARGET" || ! grep -qxF "$END" "$TARGET"; then
    echo "marker \`$START\` / \`$END\` fehlen in $TARGET" >&2
    exit 1
fi

# Nur die Zeilen der Messtabelle: sieben Spalten, ohne Kopf und Trennzeile.
# Die Tier-Tabelle darunter hat drei Spalten und fällt dadurch heraus.
rows=$(awk -F'|' 'NF >= 9 && $2 !~ /Lauf/ && $2 !~ /^-+$/ { print }' "$SOURCE")

if [ -z "$rows" ]; then
    echo "keine messzeilen in $SOURCE gefunden" >&2
    exit 1
fi

awk -v start="$START" -v end="$END" -v rows="$rows" '
    $0 == start {
        print
        print "| Lauf | Domains | RSS idle (MiB) | Peak idle (MiB) | RSS Last (MiB) | Peak Last (MiB) | QPS |"
        print "|---|---|---|---|---|---|---|"
        print rows
        skip = 1
        next
    }
    $0 == end { skip = 0 }
    !skip { print }
' "$TARGET" > "$TARGET.neu"
mv "$TARGET.neu" "$TARGET"

echo "[eintragen] $TARGET aktualisiert aus $SOURCE:"
sed -n "/$START/,/$END/p" "$TARGET"

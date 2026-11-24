#!/usr/bin/env python3
"""Prüft die im README zugesagte Eigenständigkeit.

Der Kern von pihole-lite darf nicht an ein VPN-/HubGuard-Backend gekoppelt sein.
Dieses Skript schlägt fehl, sobald im getrackten Code oder in den Cargo-Dateien
entsprechende Begriffe auftauchen oder eine Abhängigkeit auf eine fremde
Git-/Pfad-Quelle zeigt.

Nicht geprüft werden README, CHANGELOG und docs/: Dort steht, wie die
Eigenständigkeit gemeint ist, und genau diese Erklärung nennt die Begriffe.

Aufruf: python3 scripts/check_independence.py
"""
import re
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCAN_FILES = ("build.rs", "Cargo.toml", "Cargo.lock", "config.toml")
SCAN_PREFIXES = ("src/", "bench/", "deploy/")

TERMS = [
    ("VPN", re.compile(r"\bvpn\b", re.IGNORECASE)),
    ("HubGuard", re.compile(r"hubguard", re.IGNORECASE)),
    ("DACH-VPN", re.compile(r"dach[\s_-]*vpn", re.IGNORECASE)),
    ("Abo", re.compile(r"\babo\b", re.IGNORECASE)),
    ("Abonnement", re.compile(r"abonn\w*", re.IGNORECASE)),
    ("Abrechnung", re.compile(r"abrechnung", re.IGNORECASE)),
    ("Billing", re.compile(r"billing", re.IGNORECASE)),
    ("Stripe", re.compile(r"stripe", re.IGNORECASE)),
    ("Supabase", re.compile(r"supabase", re.IGNORECASE)),
    ("Subscription", re.compile(r"subscription", re.IGNORECASE)),
    ("Entitlement", re.compile(r"entitlement", re.IGNORECASE)),
]

problems: list[str] = []


def tracked_files() -> list[str]:
    result = subprocess.run(
        ["git", "ls-files", "-z"], cwd=ROOT, capture_output=True, check=True
    )
    return [entry for entry in result.stdout.decode("utf-8").split("\0") if entry]


def in_scope(path: str) -> bool:
    return path in SCAN_FILES or path.startswith(SCAN_PREFIXES)


def scan_code() -> int:
    scanned = 0
    for relative in tracked_files():
        if not in_scope(relative):
            continue
        path = ROOT / relative
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        scanned += 1
        for number, line in enumerate(text.splitlines(), 1):
            for label, pattern in TERMS:
                if pattern.search(line):
                    problems.append(f"{relative}:{number}: Begriff „{label}“")
    return scanned


def dependency_tables(node: object):
    if isinstance(node, dict):
        for key, value in node.items():
            if key in ("dependencies", "dev-dependencies", "build-dependencies") and isinstance(
                value, dict
            ):
                yield key, value
            else:
                yield from dependency_tables(value)


def scan_cargo() -> tuple[int, int]:
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    dependencies = 0
    for table, entries in dependency_tables(manifest):
        for name, spec in entries.items():
            dependencies += 1
            for label, pattern in TERMS:
                if pattern.search(name):
                    problems.append(f"Cargo.toml [{table}]: Abhängigkeit „{name}“ ({label})")
                    break
            if isinstance(spec, dict):
                if "git" in spec:
                    problems.append(
                        f"Cargo.toml [{table}] {name}: Git-Abhängigkeit ({spec['git']})"
                    )
                if "path" in spec:
                    target = (ROOT / spec["path"]).resolve()
                    if target != ROOT and ROOT not in target.parents:
                        problems.append(
                            f"Cargo.toml [{table}] {name}: Pfad außerhalb des Repos ({spec['path']})"
                        )

    lock = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))
    packages = 0
    for package in lock.get("package", []):
        packages += 1
        name = package.get("name", "")
        source = package.get("source", "")
        for label, pattern in TERMS:
            if pattern.search(name):
                problems.append(f"Cargo.lock: Paket „{name}“ ({label})")
            if source and pattern.search(source):
                problems.append(f"Cargo.lock: Quelle von „{name}“ nennt {label} ({source})")
        if source.startswith("git+"):
            problems.append(f"Cargo.lock: „{name}“ kommt aus einer Git-Quelle ({source})")

    return dependencies, packages


print("→ Eigenständigkeit: Code und Cargo-Abhängigkeiten")
scanned = scan_code()
dependencies, packages = scan_cargo()
problems = sorted(set(problems))

if problems:
    for problem in problems:
        print(f"FAIL {problem}")
    print(
        f"\n{len(problems)} Kopplungs-Befund(e). pihole-lite bleibt eigenständig: "
        "keine VPN-/HubGuard-/Abo-/Billing-Logik, keine fremden Git-/Pfad-Quellen."
    )
    sys.exit(1)

print(
    f"PASS {scanned} Code-/Konfigurationsdatei(en) ohne Kopplungs-Begriffe, "
    f"{dependencies} Abhängigkeit(en), {packages} Lock-Pakete ohne fremde Quelle."
)

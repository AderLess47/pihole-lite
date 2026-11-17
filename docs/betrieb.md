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

Cross-Build für ARMv6 von einem x86-Rechner (Debian/Ubuntu mit `binutils`, `curl`, `xz`,
`dpkg-deb` und `gcc-arm-linux-gnueabihf`):

`gcc-arm-linux-gnueabihf` allein reicht nicht. Seine crt-Objekte und libgcc sind gegen
ARMv7 + VFPv3 gebaut und tragen das als ELF-Attribut ins Binary; der Pi 1 ist ein
ARM1176 (ARMv6K, VFPv2, kein Thumb-2). Deshalb erst einen Raspbian-Sysroot entpacken,
dann über einen Wrapper linken, der `--sysroot`, `-B` und `-L` **vor** rustcs Argumente
setzt: ein `-L` aus `RUSTFLAGS` steht zu spät in der Linkerzeile und greift für `-lgcc_s`
nicht.

```sh
rustup target add arm-unknown-linux-gnueabihf

sysroot="$PWD/.armv6/sysroot"
mkdir -p "$sysroot"
curl -fsSL -o /tmp/Packages.xz \
  http://archive.raspbian.org/raspbian/dists/bookworm/main/binary-armhf/Packages.xz
xz -dc /tmp/Packages.xz > /tmp/Packages
for pkg in libc6 libc6-dev libgcc-s1 libgcc-12-dev gcc-12; do
  file=$(sed -n "/^Package: $pkg\$/,/^\$/s/^Filename: //p" /tmp/Packages)
  curl -fsSL -o "/tmp/$pkg.deb" "http://archive.raspbian.org/raspbian/$file"
  dpkg-deb -x "/tmp/$pkg.deb" "$sysroot"
done

cat > armv6-link.sh <<'EOF'
#!/bin/sh
set -e
: "${ARM_SYSROOT:?ARM_SYSROOT fehlt}"
gccdir=$(ls -d "$ARM_SYSROOT"/usr/lib/gcc/arm-linux-gnueabihf/*/ | head -n 1)
exec arm-linux-gnueabihf-gcc --sysroot="$ARM_SYSROOT" \
  -B"$ARM_SYSROOT"/usr/lib/arm-linux-gnueabihf -L"$gccdir" "$@"
EOF
chmod +x armv6-link.sh

export ARM_SYSROOT="$sysroot"
export CARGO_TARGET_ARM_UNKNOWN_LINUX_GNUEABIHF_LINKER="$PWD/armv6-link.sh"
export CARGO_TARGET_ARM_UNKNOWN_LINUX_GNUEABIHF_RUSTFLAGS="-C target-cpu=arm1176jzf-s"
export RUSTUP_TOOLCHAIN=stable
cargo build --release --target arm-unknown-linux-gnueabihf
```

Die Variable heißt bewusst `ARM_SYSROOT` und nicht `SYSROOT`: steht `SYSROOT` in der
Umgebung, bevor die Toolchain installiert ist, findet rustc für das eigene Crate kein
`std` mehr (E0463), obwohl `rust-std` installiert ist. `RUSTUP_TOOLCHAIN` pinnt die
Toolchain, in die `rust-std` für ARM ging.

Danach prüfen – beide Zeilen müssen stimmen, sonst ist der Sysroot verrutscht:

```sh
readelf -A target/arm-unknown-linux-gnueabihf/release/pihole-lite \
  | grep -E 'Tag_CPU_arch|Tag_FP_arch'
#   Tag_CPU_arch: v6
#   Tag_FP_arch: VFPv2
```

Dieselben Schritte macht der Job `ARMv6 cross build` in `.github/workflows/ci.yml`
(inklusive Cache für den Sysroot). Wer direkt auf dem Pi baut, braucht nichts davon:
dort kommen crt und libgcc ohnehin von Raspberry Pi OS.

Das Release-Profil (`opt-level = "s"`, LTO, `panic = "abort"`, `strip`) ist auf Größe
optimiert. Für den Pi zählt außerdem die glibc des Build-Systems, denn dagegen wird
gelinkt: ein Build auf einer aktuellen Distribution verlangt deren glibc und startet auf
älteren Pi-Images nicht. Der Sysroot bringt deshalb die glibc 2.36 aus Raspberry Pi OS
Bookworm mit, und die CI bricht ab, wenn das Binary ein neueres Symbol als `GLIBC_2.36`
braucht.

Die glibc allein reicht aber nicht: Debian-armhf hat **ARMv7 + VFPv3** als Baseline, und
seine crt-Objekte tragen das als ELF-Attribut ins fertige Binary — `Tag_CPU_arch: v7`,
`Thumb-2`, `VFPv3-D16`. Der Pi 1 ist ein ARM1176: ARMv6K, VFPv2, kein Thumb-2, keine
Hardware-Division. Die CI linkt deshalb gegen einen **Raspberry-Pi-OS-Sysroot**
(`libc6`, `libc6-dev`, `libgcc-s1`, `libgcc-12-dev`, `gcc-12` aus Raspbian bookworm,
nach `$RUNNER_TEMP` entpackt) und kompiliert mit `-C target-cpu=arm1176jzf-s`. Damit
stehen im Binary `Tag_CPU_arch: v6` und `Tag_FP_arch: VFPv2`, und `readelf -A` sagt die
Wahrheit über den Code, statt die Toolchain-Defaults zu wiederholen. Der Build prüft
das selbst: fehlt eines der beiden Attribute, bricht der Job ab.

## Checks und Releases

Die CI (`.github/workflows/ci.yml`) läuft bei jedem Push auf `main` und bei Pull Requests:

- `fmt, clippy, test` – `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`
- `ARMv6 cross build` – Clippy und Release-Build gegen `arm-unknown-linux-gnueabihf`
  gegen einen ARMv6-Sysroot, `file`-Prüfung, ELF-Attribute (`Tag_CPU_arch: v6`,
  `Tag_FP_arch: VFPv2`), glibc-Guard, Paketierung mit `sha256sum`, Upload als
  Workflow-Artefakt

Ein Tag `v*` baut dasselbe Binary und hängt es samt Prüfsumme dauerhaft an das Release:

```sh
git tag v0.2.1 && git push origin v0.2.1
```

Der Release-Job (`softprops/action-gh-release`) veröffentlicht nur das geprüfte Artefakt
des ARMv6-Jobs – kein zweiter Build, keine abweichenden Bytes.

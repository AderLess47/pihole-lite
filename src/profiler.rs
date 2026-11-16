//! Hardware-Profiler: liest System-RAM und CPU-Kerne und leitet daraus das Tier ab.
//!
//! Es ist die einzige Stelle im Projekt, die RAM-Grenzen kennt. Alles andere fragt
//! [`HardwareProfile`] bzw. [`Tier`].

use std::fs;
use std::io;

/// RAM-Obergrenze für Tier 1 (Nano), in MiB.
pub const NANO_MAX_MIB: u64 = 300;
/// RAM-Obergrenze für Tier 2 (Balanced), in MiB.
pub const BALANCED_MAX_MIB: u64 = 1024;
/// Annahme, wenn `/proc/meminfo` nicht lesbar ist (kein Linux).
const FALLBACK_TOTAL_MIB: u64 = 512;

/// Skalierungsstufe, abgeleitet aus dem Gesamt-RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Bis 300 MiB: ARMv6/Pi 1. Ein Worker, Hash-Backend, kleiner Cache.
    Nano,
    /// Bis 1 GiB: Radix-Trie, mehrere Worker.
    Balanced,
    /// Über 1 GiB: Radix-Trie, alle Kerne (später zusätzlich Regex-Layer).
    Pro,
}

impl Tier {
    /// Wählt das Tier anhand des Gesamt-RAM in MiB.
    pub fn from_total_mib(total_mib: u64) -> Self {
        if total_mib <= NANO_MAX_MIB {
            Self::Nano
        } else if total_mib <= BALANCED_MAX_MIB {
            Self::Balanced
        } else {
            Self::Pro
        }
    }

    /// Anzeigename für Logs und das Dashboard.
    pub fn label(self) -> &'static str {
        match self {
            Self::Nano => "Tier 1 (Nano)",
            Self::Balanced => "Tier 2 (Balanced)",
            Self::Pro => "Tier 3 (Pro)",
        }
    }

    /// Obergrenze für Tokio-Worker-Threads.
    fn max_worker_threads(self) -> usize {
        match self {
            Self::Nano => 1,
            Self::Balanced => 2,
            Self::Pro => 4,
        }
    }

    /// Anzahl Einträge im Query-Log (Ringbuffer).
    pub fn stats_entries(self) -> usize {
        match self {
            Self::Nano => 64,
            Self::Balanced => 128,
            Self::Pro => 256,
        }
    }

    /// Anzahl DNS-Antworten im LRU-Cache.
    pub fn cache_entries(self) -> usize {
        match self {
            Self::Nano => 512,
            Self::Balanced => 4096,
            Self::Pro => 16_384,
        }
    }

    /// Anzahl gleichzeitig laufender DNS-Queries, die das Tier verkraftet.
    pub fn max_in_flight(self) -> usize {
        match self {
            Self::Nano => 16,
            Self::Balanced => 64,
            Self::Pro => 256,
        }
    }

    /// Tatsächliche Worker-Anzahl: nie mehr Worker als Kerne, nie mehr als das Tier erlaubt.
    pub fn worker_threads(self, cores: usize) -> usize {
        cores.clamp(1, self.max_worker_threads())
    }
}

/// Ergebnis der Hardware-Erkennung beim Start.
#[derive(Debug, Clone, Copy)]
pub struct HardwareProfile {
    /// Abgeleitetes Tier.
    pub tier: Tier,
    /// Gesamt-RAM in MiB.
    pub total_mib: u64,
    /// Verfügbarer RAM in MiB (zum Startzeitpunkt).
    pub available_mib: u64,
    /// Verfügbare CPU-Kerne.
    pub cores: usize,
    /// Anzahl der Tokio-Worker-Threads.
    pub worker_threads: usize,
    /// `false`, wenn `/proc/meminfo` fehlte und Defaults angenommen wurden.
    pub meminfo_available: bool,
}

impl HardwareProfile {
    /// Erkennt RAM und Kerne und leitet das Tier ab.
    pub fn detect() -> Self {
        let (total_mib, available_mib, meminfo_available) = match read_meminfo() {
            Ok(info) => {
                let total = info.total_kib / 1024;
                let available = (info.available_kib / 1024).min(total);
                (total, available, true)
            }
            Err(err) => {
                eprintln!(
                    "[profiler] /proc/meminfo nicht lesbar ({err}); nehme {FALLBACK_TOTAL_MIB} MiB an"
                );
                (FALLBACK_TOTAL_MIB, FALLBACK_TOTAL_MIB / 2, false)
            }
        };

        let tier = if meminfo_available {
            Tier::from_total_mib(total_mib)
        } else {
            // Ohne Messung nicht in den Nano-Pfad rutschen: dort fehlt dem Trie-Handling
            // nichts, aber ein unbekanntes System soll den komfortableren Pfad bekommen.
            Tier::Balanced
        };
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);

        Self {
            tier,
            total_mib,
            available_mib,
            cores,
            worker_threads: tier.worker_threads(cores),
            meminfo_available,
        }
    }

    /// Ein-Zeilen-Zusammenfassung für das Startbanner.
    pub fn summary(&self) -> String {
        let fallback = if self.meminfo_available {
            ""
        } else {
            " [Fallback: /proc/meminfo fehlt]"
        };
        format!(
            "{} MiB RAM ({} MiB frei), {} Kern(e), {} Worker{fallback}",
            self.total_mib, self.available_mib, self.cores, self.worker_threads
        )
    }
}

/// Relevanter Ausschnitt aus `/proc/meminfo` (Werte in KiB).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MemInfo {
    total_kib: u64,
    available_kib: u64,
}

fn read_meminfo() -> io::Result<MemInfo> {
    let text = fs::read_to_string("/proc/meminfo")?;
    parse_meminfo(&text).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "MemTotal fehlt in /proc/meminfo",
        )
    })
}

/// Parst `MemTotal` und `MemAvailable` (Fallback: `MemFree` + `Buffers` + `Cached`).
fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let mut found_total = false;
    let mut total_kib = 0u64;
    let mut available_kib = 0u64;
    let mut free_kib = 0u64;
    let mut buffers_kib = 0u64;
    let mut cached_kib = 0u64;

    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let value = rest
            .split_whitespace()
            .next()
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(0);
        match key.trim() {
            "MemTotal" => {
                total_kib = value;
                found_total = true;
            }
            "MemAvailable" => available_kib = value,
            "MemFree" => free_kib = value,
            "Buffers" => buffers_kib = value,
            "Cached" => cached_kib = value,
            _ => {}
        }
    }

    if !found_total {
        return None;
    }
    if available_kib == 0 {
        // Ältere Kernel (< 3.14) kennen MemAvailable nicht.
        available_kib = free_kib + buffers_kib + cached_kib;
    }

    Some(MemInfo {
        total_kib,
        available_kib,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_follow_ram_size() {
        assert_eq!(Tier::Nano, Tier::from_total_mib(256));
        assert_eq!(Tier::Nano, Tier::from_total_mib(NANO_MAX_MIB));
        assert_eq!(Tier::Balanced, Tier::from_total_mib(NANO_MAX_MIB + 1));
        assert_eq!(Tier::Balanced, Tier::from_total_mib(BALANCED_MAX_MIB));
        assert_eq!(Tier::Pro, Tier::from_total_mib(BALANCED_MAX_MIB + 1));
        assert_eq!(Tier::Pro, Tier::from_total_mib(16384));
    }

    #[test]
    fn worker_threads_are_bounded_by_cores_and_tier() {
        assert_eq!(1, Tier::Nano.worker_threads(8));
        assert_eq!(2, Tier::Balanced.worker_threads(8));
        assert_eq!(1, Tier::Pro.worker_threads(1));
    }

    #[test]
    fn meminfo_parses_available_and_falls_back_to_free_plus_cached() {
        let with_available =
            "MemTotal:         256000 kB\nMemFree:  10000 kB\nMemAvailable: 200000 kB\n";
        assert_eq!(
            Some(MemInfo {
                total_kib: 256_000,
                available_kib: 200_000
            }),
            parse_meminfo(with_available)
        );

        let without_available =
            "MemTotal: 262144 kB\nMemFree: 4096 kB\nBuffers: 2048 kB\nCached: 8192 kB\n";
        assert_eq!(
            Some(MemInfo {
                total_kib: 262_144,
                available_kib: 14_336
            }),
            parse_meminfo(without_available)
        );

        assert_eq!(None, parse_meminfo("garbage without fields\n"));
    }
}

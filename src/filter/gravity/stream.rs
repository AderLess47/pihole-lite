//! Zeilenweise ins Backend: `fill_buf`/`consume` statt `read_line`, damit auch eine
//! riesige Zeile den Puffer nicht wachsen lässt.

use std::io;
use std::pin::Pin;

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use super::source::CacheSink;
use super::MAX_LINE_BYTES;
use crate::filter::engine::{blocklist_domain_from_line, FilterBackend};

#[derive(Debug, Default)]
pub(super) struct Totals {
    pub(super) lines: u64,
    pub(super) accepted: u64,
    pub(super) skipped: u64,
}

/// Liest den Stream zeilenweise und füttert das Backend.
///
/// Nutzt `fill_buf`/`consume`, damit auch sehr lange Zeilen den Speicher nicht sprengen:
/// Zeilen über [`MAX_LINE_BYTES`] werden verworfen, statt den Puffer zu vergrößern.
pub(super) async fn consume_source(
    reader: &mut (dyn AsyncBufRead + Unpin + Send),
    backend: &mut dyn FilterBackend,
    totals: &mut Totals,
    sink: &mut CacheSink<'_>,
) -> io::Result<()> {
    let mut line: Vec<u8> = Vec::with_capacity(128);
    // Akzeptierte Domains des aktuellen Chunks; wird gesammelt, damit der Cache in
    // Blockgröße geschrieben wird statt Zeile für Zeile.
    let mut pending: Vec<u8> = Vec::with_capacity(256);
    let mut overflow = false;

    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            break;
        }

        let mut start = 0usize;
        while let Some(offset) = chunk[start..].iter().position(|&byte| byte == b'\n') {
            let end = start + offset;
            let segment = &chunk[start..end];

            if overflow {
                // Rest einer bereits verworfenen Zeile: nur abschließen.
                overflow = false;
                totals.skipped += 1;
            } else if line.len() + segment.len() > MAX_LINE_BYTES {
                line.clear();
                totals.skipped += 1;
            } else {
                line.extend_from_slice(segment);
                emit_line(&mut line, backend, totals, &mut pending);
            }

            start = end + 1;
        }

        if !overflow && start < chunk.len() {
            let rest = &chunk[start..];
            if line.len() + rest.len() > MAX_LINE_BYTES {
                // Zeile ist zu lang: verwerfen, Rest bis zum nächsten \n ignorieren.
                overflow = true;
                line.clear();
            } else {
                line.extend_from_slice(rest);
            }
        }

        let consumed = chunk.len();
        if !pending.is_empty() {
            sink.write_bytes(&pending).await;
            pending.clear();
        }
        Pin::new(&mut *reader).consume(consumed);
    }

    if !overflow && !line.is_empty() {
        emit_line(&mut line, backend, totals, &mut pending);
    }
    if !pending.is_empty() {
        sink.write_bytes(&pending).await;
    }
    Ok(())
}

/// Verarbeitet eine komplette Zeile, hängt akzeptierte Domains an `pending` und leert
/// den Zeilenpuffer.
fn emit_line(
    line: &mut Vec<u8>,
    backend: &mut dyn FilterBackend,
    totals: &mut Totals,
    pending: &mut Vec<u8>,
) {
    totals.lines += 1;
    if line.last() == Some(&b'\r') {
        line.pop();
    }

    match std::str::from_utf8(line) {
        Ok(text) => match blocklist_domain_from_line(text) {
            Some(domain) => {
                backend.add(domain.as_ref());
                totals.accepted += 1;
                pending.extend_from_slice(domain.as_bytes());
                pending.push(b'\n');
            }
            None => totals.skipped += 1,
        },
        Err(_) => totals.skipped += 1,
    }
    line.clear();
}

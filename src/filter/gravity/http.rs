//! `http://`-Transport: Request schreiben, Statuszeile und Header lesen.
//!
//! HTTP/1.0 plus `Connection: close` heißt: der Body endet mit dem Socket, es gibt kein
//! Chunked-Framing. Chunked-Antworten werden deshalb abgelehnt statt falsch geparst.

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::cache::Validators;
use super::{GravityError, CONNECT_TIMEOUT, MAX_HEADER_BYTES};

/// Gelesene Response-Header.
pub(super) struct ResponseHead {
    pub(super) status: u16,
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
}

impl ResponseHead {
    pub(super) fn validators(&self) -> Validators {
        Validators {
            etag: self.etag.clone(),
            last_modified: self.last_modified.clone(),
        }
    }
}

/// Öffnet eine `http://`-URL und liest die Response-Header weg.
///
/// Der Body bleibt im `BufReader` gepuffert und wird von dort gestreamt. HTTP/1.0 +
/// `Connection: close` heißt: der Body endet mit dem Socket, kein Chunked-Framing.
pub(super) async fn open_http(
    raw: &str,
    validators: Option<&Validators>,
) -> Result<(BufReader<TcpStream>, ResponseHead), GravityError> {
    let url = HttpUrl::parse(raw)?;
    let stream = timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((url.host.as_str(), url.port)),
    )
    .await
    .map_err(|_| GravityError::Timeout(raw.to_owned()))?
    .map_err(GravityError::Io)?;
    stream.set_nodelay(true).map_err(GravityError::Io)?;
    let mut reader = BufReader::new(stream);

    let mut request = String::with_capacity(320);
    request.push_str("GET ");
    request.push_str(&url.path);
    request.push_str(" HTTP/1.0\r\nHost: ");
    request.push_str(&url.authority);
    request.push_str("\r\nUser-Agent: pihole-lite/");
    request.push_str(env!("CARGO_PKG_VERSION"));
    request.push_str("\r\nAccept: text/plain, */*\r\nConnection: close\r\n");

    if let Some(validators) = validators {
        if let Some(etag) = validators
            .etag
            .as_deref()
            .filter(|value| is_header_safe(value))
        {
            request.push_str("If-None-Match: ");
            request.push_str(etag);
            request.push_str("\r\n");
        }
        if let Some(last_modified) = validators
            .last_modified
            .as_deref()
            .filter(|value| is_header_safe(value))
        {
            request.push_str("If-Modified-Since: ");
            request.push_str(last_modified);
            request.push_str("\r\n");
        }
    }
    request.push_str("\r\n");

    reader
        .get_mut()
        .write_all(request.as_bytes())
        .await
        .map_err(GravityError::Io)?;

    let head = read_response_headers(&mut reader, raw).await?;
    Ok((reader, head))
}

/// Nur druckbare ASCII-Werte gehen als Header auf die Reise (kein CR/LF-Injection).
fn is_header_safe(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
}

/// Liest Statuszeile und Header; bricht bei Chunked-Encoding bewusst ab, statt später
/// Chunk-Größen als Domains zu interpretieren.
async fn read_response_headers<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    url: &str,
) -> Result<ResponseHead, GravityError> {
    let mut line = String::with_capacity(128);
    let mut header_bytes = 0usize;

    reader
        .read_line(&mut line)
        .await
        .map_err(GravityError::Io)?;
    let status = parse_status_line(&line).ok_or_else(|| GravityError::Http {
        url: url.to_owned(),
        reason: format!("ungültige Statuszeile: {}", line.trim()),
    })?;
    header_bytes += line.len();

    let mut head = ResponseHead {
        status,
        etag: None,
        last_modified: None,
    };

    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(GravityError::Io)?;
        if read == 0 {
            return Err(GravityError::Http {
                url: url.to_owned(),
                reason: "Verbindung endete innerhalb der Header".to_owned(),
            });
        }
        header_bytes += read;
        if header_bytes > MAX_HEADER_BYTES {
            return Err(GravityError::Http {
                url: url.to_owned(),
                reason: "Response-Header zu groß".to_owned(),
            });
        }

        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            return Ok(head);
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();

        if name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            return Err(GravityError::Http {
                url: url.to_owned(),
                reason: "chunked transfer-encoding; nutze den fetcher oder eine quelle mit content-length".to_owned(),
            });
        }
        if name.eq_ignore_ascii_case("etag") {
            head.etag = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("last-modified") {
            head.last_modified = Some(value.to_owned());
        }
    }
}

fn parse_status_line(line: &str) -> Option<u16> {
    let mut parts = line.split_whitespace();
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse().ok()
}

/// Zerlegte `http://`-URL.
pub(super) struct HttpUrl {
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) path: String,
    pub(super) authority: String,
}

impl HttpUrl {
    pub(super) fn parse(raw: &str) -> Result<Self, GravityError> {
        let rest = raw
            .strip_prefix("http://")
            .ok_or_else(|| GravityError::UnsupportedScheme(raw.to_owned()))?;
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err(GravityError::InvalidUrl(raw.to_owned()));
        }

        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => (
                host,
                port.parse::<u16>()
                    .map_err(|_| GravityError::InvalidUrl(raw.to_owned()))?,
            ),
            _ => (authority, 80u16),
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if host.is_empty() || host.contains('@') {
            return Err(GravityError::InvalidUrl(raw.to_owned()));
        }

        Ok(Self {
            host: host.to_owned(),
            port,
            path: path.to_owned(),
            authority: authority.to_owned(),
        })
    }
}

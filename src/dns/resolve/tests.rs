//! Tests der Entscheidung: jede Antwortart ohne Socket, ohne Listener.

use super::*;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;

use crate::dns::server::ServerOptions;
use crate::dns::upstream::Upstream;
use crate::filter::engine::{backend_for_tier, FilterEngine};
use crate::profiler::Tier;

const QTYPE_A: u16 = 1;
/// Fester Zeitpunkt für alle Tests: die Uhr ist ein Parameter, also kein Warten nötig.
const NOW: u64 = 1_700_000_000;

/// Query im Wire-Format; bewusst handgebaut, damit der Test keine Encoder-Kette braucht.
fn query_bytes(name: &str, qtype: u16, id: u16) -> Vec<u8> {
    let mut out = vec![0u8; HEADER_BYTES];
    out[0..2].copy_from_slice(&id.to_be_bytes());
    out[2] = 0x01; // RD
    out[4..6].copy_from_slice(&1u16.to_be_bytes());
    for label in name.split('.').filter(|label| !label.is_empty()) {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // Klasse IN
    out
}

fn name_of(reply: &Reply) -> &str {
    std::str::from_utf8(&reply.name[..reply.name_len]).expect("name im reply")
}

fn a_record_count(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[6], bytes[7]])
}

fn rcode(bytes: &[u8]) -> u8 {
    bytes[3] & 0x0f
}

fn peer() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 5555))
}

/// Upstream ohne Resolver: jede Weiterleitung schlägt sofort mit `NoServers` fehl.
fn no_upstream() -> Arc<Upstream> {
    Arc::new(Upstream::new(Vec::new(), Duration::from_millis(100), false))
}

fn server_with(
    engine: Arc<FilterEngine>,
    upstream: Arc<Upstream>,
    cache_entries: usize,
) -> DnsServer {
    DnsServer::new(
        ServerOptions {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            mode: SinkholeMode::ZeroIp,
            sinkhole_ttl: 60,
            max_in_flight: 16,
            cache_entries,
            verbose: false,
        },
        engine,
        upstream,
        Arc::new(Stats::new(8)),
    )
}

fn plain_engine() -> Arc<FilterEngine> {
    Arc::new(FilterEngine::new(Tier::Nano))
}

/// Bestand wie in Produktion: Backend bauen, versiegeln, tauschen.
fn blocking_engine(domain: &str) -> Arc<FilterEngine> {
    let engine = FilterEngine::new(Tier::Nano);
    let mut backend = backend_for_tier(Tier::Nano);
    backend.add(domain);
    backend.seal();
    engine.replace_backend(backend);
    Arc::new(engine)
}

/// Antwortet einmal auf ein UDP-Paket: QR-Bit setzen, keine Records, gleiche ID.
async fn udp_responder_once() -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 512];
        let (len, peer) = socket.recv_from(&mut buf).await.unwrap();
        let mut answer = buf[..len].to_vec();
        answer[2] |= 0x80;
        // Keine Records in allen drei Abschnitten; die Frage bleibt stehen.
        answer[6..12].fill(0);
        let _ = socket.send_to(&answer, peer).await;
    });
    addr
}

#[tokio::test]
async fn blocked_query_yields_a_sinkhole_and_a_blocked_verdict() {
    let server = server_with(blocking_engine("ads.example.com"), no_upstream(), 16);
    let reply = server
        .resolve(
            &query_bytes("sub.ads.example.com", QTYPE_A, 0x1234),
            peer(),
            NOW,
        )
        .await;

    assert_eq!(Some(Verdict::Blocked), reply.verdict);
    assert_eq!("sub.ads.example.com", name_of(&reply));
    let bytes = reply.bytes.as_ref().expect("sinkhole-antwort");
    assert_eq!([0x12, 0x34], [bytes[0], bytes[1]]);
    assert_eq!(0, rcode(bytes));
    assert_eq!(1, a_record_count(bytes));

    reply.record(&server.stats);
    assert_eq!(1, server.stats.snapshot(0).blocked);
    assert_eq!(1, server.stats.snapshot(0).queries);
}

#[tokio::test]
async fn cached_answer_is_served_without_the_upstream() {
    let server = server_with(plain_engine(), no_upstream(), 16);
    let mut stored = query_bytes("cache.test", QTYPE_A, 0x9999);
    stored[2] |= 0x80;
    server
        .cache
        .insert("cache.test", QTYPE_A, stored, Duration::from_secs(60));

    let reply = server
        .resolve(&query_bytes("cache.test", QTYPE_A, 0x0042), peer(), NOW)
        .await;

    assert_eq!(Some(Verdict::Cached), reply.verdict);
    let bytes = reply.bytes.as_ref().expect("cache-antwort");
    assert_eq!([0x00, 0x42], [bytes[0], bytes[1]]);
}

#[tokio::test]
async fn forwarded_query_returns_the_upstream_answer() {
    let upstream = Arc::new(Upstream::new(
        vec![udp_responder_once().await],
        Duration::from_secs(5),
        false,
    ));
    let server = server_with(plain_engine(), upstream, 0);

    let reply = server
        .resolve(&query_bytes("forward.test", QTYPE_A, 0x0007), peer(), NOW)
        .await;

    assert_eq!(Some(Verdict::Forwarded), reply.verdict);
    let bytes = reply.bytes.as_ref().expect("antwort");
    assert_eq!([0x00, 0x07], [bytes[0], bytes[1]]);
    assert_eq!(0x80, bytes[2] & 0x80);
}

#[tokio::test]
async fn upstream_failure_answers_with_servfail() {
    let server = server_with(plain_engine(), no_upstream(), 0);
    let reply = server
        .resolve(&query_bytes("down.test", QTYPE_A, 0x0011), peer(), NOW)
        .await;

    assert_eq!(Some(Verdict::Error), reply.verdict);
    assert_eq!(2, rcode(reply.bytes.as_ref().expect("servfail")));
}

#[tokio::test]
async fn an_answer_is_not_taken_for_a_query() {
    let server = server_with(plain_engine(), no_upstream(), 16);
    let mut request = query_bytes("loop.test", QTYPE_A, 0x0101);
    request[2] |= 0x80;

    let reply = server.resolve(&request, peer(), NOW).await;

    assert_eq!(None, reply.verdict);
    assert!(reply.bytes.is_none());
}

#[tokio::test]
async fn short_packets_are_dropped_silently() {
    let server = server_with(plain_engine(), no_upstream(), 16);
    let reply = server
        .resolve(&[0x12, 0x34, 0x01, 0x00, 0x00], peer(), NOW)
        .await;

    assert_eq!(None, reply.verdict);
    assert!(reply.bytes.is_none());
}

#[tokio::test]
async fn broken_queries_get_formerr() {
    let server = server_with(plain_engine(), no_upstream(), 16);
    let mut request = query_bytes("broken.test", QTYPE_A, 0x0022);
    request.truncate(HEADER_BYTES + 3);

    let reply = server.resolve(&request, peer(), NOW).await;

    assert_eq!(None, reply.verdict);
    assert_eq!(1, rcode(reply.bytes.as_ref().expect("formerr")));
}

#[tokio::test]
async fn a_disabled_filter_lets_blocked_names_through_until_the_deadline() {
    let upstream = Arc::new(Upstream::new(
        vec![udp_responder_once().await],
        Duration::from_secs(5),
        false,
    ));
    let server = server_with(blocking_engine("ads.example.com"), upstream, 0);
    server.engine.disable_for(60, NOW);

    let reply = server
        .resolve(
            &query_bytes("ads.example.com", QTYPE_A, 0x0033),
            peer(),
            NOW + 30,
        )
        .await;
    assert_eq!(Some(Verdict::Forwarded), reply.verdict);

    // Nach der Deadline blockt derselbe Bestand wieder – nur die Zahl ändert sich.
    let reply = server
        .resolve(
            &query_bytes("ads.example.com", QTYPE_A, 0x0034),
            peer(),
            NOW + 61,
        )
        .await;
    assert_eq!(Some(Verdict::Blocked), reply.verdict);
    assert_eq!(0, rcode(reply.bytes.as_ref().expect("sinkhole")));
}

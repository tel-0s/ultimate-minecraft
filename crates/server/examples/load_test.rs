//! Phase 6e: headless Minecraft 1.21.11 client swarm for load testing.
//!
//! Each client speaks the real protocol — handshake, offline login, the
//! Known Packs configuration exchange, play — answers keep-alives, and
//! optionally wanders (movement packets that drive chunk streaming) and
//! breaks blocks (physics submissions). Reports join latency percentiles
//! and inbound traffic.
//!
//! Usage:
//!   cargo run --release --example load_test -- <players> <duration_secs> [--wander] [--break] [addr]
//!
//! Run a release server first (`cargo run --release`).

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use azalea_core::position::Vec3;
use azalea_protocol::common::movements::MoveFlags;
use azalea_protocol::packets::config::{
    ClientboundConfigPacket, ServerboundFinishConfiguration, ServerboundSelectKnownPacks,
};
use azalea_protocol::packets::game::{
    ClientboundGamePacket, ServerboundAcceptTeleportation, ServerboundGamePacket,
    ServerboundKeepAlive, ServerboundMovePlayerPos,
};
use azalea_protocol::packets::handshake::{ServerboundHandshakePacket, ServerboundIntention};
use azalea_protocol::packets::login::{
    ClientboundLoginPacket, ServerboundHello, ServerboundLoginAcknowledged, ServerboundLoginPacket,
};
use azalea_protocol::packets::{ClientIntention, Packet};
use azalea_protocol::read::read_packet;
use azalea_protocol::write::write_packet;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::net::TcpStream;
use uuid::Uuid;

const PROTOCOL: i32 = 774; // MC 1.21.11

#[derive(Default)]
struct Stats {
    connected: AtomicU64,
    joined: AtomicU64,
    chunks: AtomicU64,
    bytes: AtomicU64,
    keepalives: AtomicU64,
    block_updates: AtomicU64,
    errors: AtomicU64,
    join_ms: Mutex<Vec<f64>>,
}

/// AsyncRead wrapper that counts inbound bytes.
struct CountingReader<R> {
    inner: R,
    bytes: Arc<AtomicU64>,
}

impl<R: AsyncRead + Unpin> AsyncRead for CountingReader<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &poll {
            let n = buf.filled().len() - before;
            self.bytes.fetch_add(n as u64, Relaxed);
        }
        poll
    }
}

async fn run_client(
    index: usize,
    addr: String,
    stats: Arc<Stats>,
    deadline: Instant,
    wander: bool,
    do_break: bool,
    spread: bool,
) -> Result<()> {
    let t_start = Instant::now();
    let stream = TcpStream::connect(&addr).await?;
    stream.set_nodelay(true)?;
    let (read_half, mut write) = stream.into_split();
    let byte_counter = Arc::new(AtomicU64::new(0));
    let mut read = CountingReader { inner: read_half, bytes: Arc::clone(&byte_counter) };
    let mut buf = Cursor::new(Vec::new());
    let mut enc = None;
    let mut dec = None;
    let compression: Option<u32> = None;

    // ── Handshake + login ───────────────────────────────────────────────
    let intent: ServerboundHandshakePacket = ServerboundIntention {
        protocol_version: PROTOCOL,
        hostname: "loadtest".into(),
        port: 25565,
        intention: ClientIntention::Login,
    }
    .into_variant();
    write_packet(&intent, &mut write, compression, &mut enc).await?;

    let hello: ServerboundLoginPacket = ServerboundHello {
        name: format!("load_{index:04}"),
        profile_id: Uuid::nil(),
    }
    .into_variant();
    write_packet(&hello, &mut write, compression, &mut enc).await?;

    loop {
        let pkt = read_packet::<ClientboundLoginPacket, _>(&mut read, &mut buf, compression, &mut dec)
            .await
            .map_err(|e| anyhow!("login read: {e}"))?;
        match pkt {
            ClientboundLoginPacket::LoginFinished(_) => break,
            ClientboundLoginPacket::LoginDisconnect(d) => {
                return Err(anyhow!("disconnected during login: {:?}", d.reason));
            }
            _ => {}
        }
    }
    let ack: ServerboundLoginPacket = ServerboundLoginAcknowledged.into_variant();
    write_packet(&ack, &mut write, compression, &mut enc).await?;

    // ── Configuration ───────────────────────────────────────────────────
    loop {
        let pkt =
            read_packet::<ClientboundConfigPacket, _>(&mut read, &mut buf, compression, &mut dec)
                .await
                .map_err(|e| anyhow!("config read: {e}"))?;
        match pkt {
            ClientboundConfigPacket::SelectKnownPacks(p) => {
                let resp = ServerboundSelectKnownPacks { known_packs: p.known_packs.clone() };
                write_packet(&resp.into_variant(), &mut write, compression, &mut enc).await?;
            }
            ClientboundConfigPacket::FinishConfiguration(_) => {
                write_packet(
                    &ServerboundFinishConfiguration.into_variant(),
                    &mut write,
                    compression,
                    &mut enc,
                )
                .await?;
                break;
            }
            _ => {}
        }
    }
    stats.connected.fetch_add(1, Relaxed);

    // ── Play: reader drives, writer half replies ────────────────────────
    let (reply_tx, mut reply_rx) = tokio::sync::mpsc::unbounded_channel::<ServerboundGamePacket>();

    // Writer task: keep-alive replies + wander movement + block breaks.
    let writer_stats = Arc::clone(&stats);
    let writer = tokio::spawn(async move {
        // Deterministic per-client heading (golden angle).
        let angle = (index as f64) * 2.399963;
        // --spread: clients fan out over a ~1200-block-wide ring before
        // wandering, so crowds are SPARSE — the spatial pub/sub case.
        // Default: everyone stays near spawn (dense crowd).
        let mut pos = if spread {
            let r = 80.0 + (index % 120) as f64 * 5.0;
            Vec3 { x: 8.5 + angle.cos() * r, y: 80.0, z: 8.5 + angle.sin() * r }
        } else {
            Vec3 { x: 8.5, y: 80.0, z: 8.5 }
        };
        let (dx, dz) = (angle.cos() * 0.8, angle.sin() * 0.8);
        let mut move_tick = tokio::time::interval(Duration::from_millis(200));
        let mut break_tick = tokio::time::interval(Duration::from_millis(2000));
        loop {
            tokio::select! {
                reply = reply_rx.recv() => {
                    match reply {
                        Some(pkt) => {
                            if write_packet(&pkt, &mut write, compression, &mut enc).await.is_err() {
                                return;
                            }
                            let _ = &writer_stats; // (keep-alive count happens reader-side)
                        }
                        None => return, // reader gone
                    }
                }
                _ = move_tick.tick(), if wander => {
                    pos.x += dx;
                    pos.z += dz;
                    let mv: ServerboundGamePacket = ServerboundMovePlayerPos {
                        pos,
                        flags: MoveFlags { on_ground: true, horizontal_collision: false },
                    }.into_variant();
                    if write_packet(&mv, &mut write, compression, &mut enc).await.is_err() {
                        return;
                    }
                }
                _ = break_tick.tick(), if do_break => {
                    use azalea_protocol::packets::game::s_player_action::{self, Action};
                    let pkt: ServerboundGamePacket = s_player_action::ServerboundPlayerAction {
                        action: Action::StartDestroyBlock,
                        pos: azalea_core::position::BlockPos::new(
                            pos.x as i32, 70, pos.z as i32,
                        ),
                        direction: azalea_core::direction::Direction::Up,
                        seq: 1,
                    }.into_variant();
                    if write_packet(&pkt, &mut write, compression, &mut enc).await.is_err() {
                        return;
                    }
                }
            }
        }
    });

    // Reader loop: count traffic, echo keep-alives, note the join.
    let mut joined = false;
    let result: Result<()> = async {
        loop {
            if Instant::now() >= deadline {
                return Ok(());
            }
            let pkt = tokio::time::timeout(
                Duration::from_secs(30),
                read_packet::<ClientboundGamePacket, _>(&mut read, &mut buf, compression, &mut dec),
            )
            .await
            .map_err(|_| anyhow!("read stalled >30s"))?
            .map_err(|e| anyhow!("play read: {e}"))?;

            match pkt {
                ClientboundGamePacket::Login(_) => {
                    if !joined {
                        joined = true;
                        stats.joined.fetch_add(1, Relaxed);
                        let ms = t_start.elapsed().as_secs_f64() * 1e3;
                        stats.join_ms.lock().unwrap().push(ms);
                    }
                }
                ClientboundGamePacket::KeepAlive(ka) => {
                    stats.keepalives.fetch_add(1, Relaxed);
                    let _ = reply_tx.send(ServerboundKeepAlive { id: ka.id }.into_variant());
                }
                ClientboundGamePacket::PlayerPosition(p) => {
                    // The server blocks on this confirmation before
                    // streaming chunks (vanilla behaviour).
                    let _ = reply_tx
                        .send(ServerboundAcceptTeleportation { id: p.id }.into_variant());
                }
                ClientboundGamePacket::LevelChunkWithLight(_) => {
                    stats.chunks.fetch_add(1, Relaxed);
                }
                ClientboundGamePacket::BlockUpdate(_) => {
                    stats.block_updates.fetch_add(1, Relaxed);
                }
                ClientboundGamePacket::Disconnect(d) => {
                    return Err(anyhow!("kicked: {:?}", d.reason));
                }
                _ => {}
            }
        }
    }
    .await;

    stats.bytes.fetch_add(byte_counter.load(Relaxed), Relaxed);
    writer.abort();
    result
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let players: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(100);
    let duration: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(30);
    let wander = args.iter().any(|a| a == "--wander");
    let do_break = args.iter().any(|a| a == "--break");
    let spread = args.iter().any(|a| a == "--spread");
    let addr = args
        .iter()
        .skip(3)
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:25565".to_string());

    println!(
        "load_test: {players} players, {duration}s, wander={wander}, break={do_break}, spread={spread}, addr={addr}"
    );

    let stats = Arc::new(Stats::default());
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(duration);

    let mut handles = Vec::with_capacity(players);
    for i in 0..players {
        let stats = Arc::clone(&stats);
        let addr = addr.clone();
        handles.push(tokio::spawn(async move {
            // Ramp: ~200 joins/sec.
            tokio::time::sleep(Duration::from_millis((i as u64) * 5)).await;
            if let Err(e) = run_client(i, addr, stats.clone(), deadline, wander, do_break, spread).await {
                let n = stats.errors.fetch_add(1, Relaxed);
                if n < 5 {
                    eprintln!("client {i}: {e:#}");
                }
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }

    let elapsed = t0.elapsed().as_secs_f64();
    let mut joins = stats.join_ms.lock().unwrap().clone();
    joins.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| -> f64 {
        if joins.is_empty() {
            return f64::NAN;
        }
        joins[((joins.len() as f64 - 1.0) * p) as usize]
    };
    let gb = stats.bytes.load(Relaxed) as f64 / 1e9;

    println!();
    println!("=== load_test results ({:.1}s) ===", elapsed);
    println!(
        "  joined: {}/{} (errors {})",
        stats.joined.load(Relaxed),
        players,
        stats.errors.load(Relaxed)
    );
    println!("  join latency: p50 {:.0} ms | p99 {:.0} ms | max {:.0} ms", pct(0.5), pct(0.99), pct(1.0));
    println!(
        "  chunks received: {} ({:.0}/s) | inbound {:.2} GB ({:.1} MB/s)",
        stats.chunks.load(Relaxed),
        stats.chunks.load(Relaxed) as f64 / elapsed,
        gb,
        gb * 1e3 / elapsed,
    );
    println!(
        "  keep-alive replies: {} | block updates seen: {}",
        stats.keepalives.load(Relaxed),
        stats.block_updates.load(Relaxed)
    );
}

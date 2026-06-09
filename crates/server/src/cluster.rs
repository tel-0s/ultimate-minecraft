//! Phase 6f: the partition boundary over a socket.
//!
//! 6b-2 made the boundary between two physics workers a *message*:
//! serialize an event, send it to the owner of its position, with
//! happens-before riding the transport because consequents are forwarded
//! only after their cause executed. This module runs the same protocol
//! over TCP between two **nodes** (processes/machines).
//!
//! What makes cross-node physics cheap here:
//! - **Worldgen is deterministic** (preset + seed): every node generates
//!   identical baseline terrain locally — no chunk transfer, ever. A
//!   node's copy of foreign regions is a *replica* kept fresh by
//!   [`WriteSync`] frames (each node mirrors its write log to peers).
//! - **Rules are confluent and self-stabilizing** (6b-2): rule
//!   evaluations that read replica state across a node border are the
//!   same race class as cross-partition reads inside one process, and
//!   converge the same way.
//!
//! ## Wire frames (u32-LE length, then u8 kind, then body)
//! - `Forward`  — cross-node consequents `(event, priority)*`, inserted
//!   as roots in the receiving node's graphs.
//! - `Action`   — a player block action whose position the peer owns.
//! - `WriteSync`— the sender's executed write payloads; the receiver
//!   applies them to its replica world and republishes on its bus so
//!   *its* connected clients see physics computed on the other node.
//! - `Ping`/`Pong` — two-node quiescence: globally quiet iff both local
//!   `pending == 0` AND each side has received exactly what the other
//!   sent (`Forward`/`Action`/`WriteSync` frames are counted; pings are
//!   not). Receive-side ordering makes the check sound: work is injected
//!   (raising local pending) *before* `received` is incremented.
//!
//! Prototype scope: exactly two nodes (one peer link). The full mesh,
//! region migration, and gateway/physics node separation come later.

use std::io::{Read as IoRead, Write as IoWrite};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use ultimate_engine::causal::event::{Event, EventPayload, LightCell, LightType};
use ultimate_engine::world::block::BlockId;
use ultimate_engine::world::position::{BlockPos, ChunkPos};
use ultimate_engine::world::World;

use crate::event_bus::{self, ChangeSource, SpatialBus};
use crate::physics::{BlockAction, PhysicsHandle};

/// Which node owns a region. Salted differently from the intra-node
/// worker hash so node and worker assignment decorrelate.
pub fn owner_node(chunk: ChunkPos, total_nodes: u32) -> u32 {
    if total_nodes <= 1 {
        return 0;
    }
    let rx = (chunk.x >> 2) as i64 as u64 ^ 0xA5A5_5A5A_DEAD_BEEF;
    let rz = (chunk.z >> 2) as i64 as u64;
    let mut h = (rx << 32) ^ (rz & 0xFFFF_FFFF) ^ (rx >> 32);
    h = h.wrapping_add(0x9E3779B97F4A7C15);
    h = (h ^ (h >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94D049BB133111EB);
    h ^= h >> 31;
    (h % total_nodes as u64) as u32
}

// ── Payload codec ───────────────────────────────────────────────────────────

fn put_i64(buf: &mut Vec<u8>, v: i64) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn put_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn put_pos(buf: &mut Vec<u8>, p: BlockPos) {
    put_i64(buf, p.x);
    put_i64(buf, p.y);
    put_i64(buf, p.z);
}

struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, at: 0 }
    }
    fn u8(&mut self) -> Result<u8> {
        let v = *self.buf.get(self.at).ok_or_else(|| anyhow!("truncated frame"))?;
        self.at += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16> {
        let s = self.buf.get(self.at..self.at + 2).ok_or_else(|| anyhow!("truncated frame"))?;
        self.at += 2;
        Ok(u16::from_le_bytes(s.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        let s = self.buf.get(self.at..self.at + 4).ok_or_else(|| anyhow!("truncated frame"))?;
        self.at += 4;
        Ok(u32::from_le_bytes(s.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        let s = self.buf.get(self.at..self.at + 8).ok_or_else(|| anyhow!("truncated frame"))?;
        self.at += 8;
        Ok(u64::from_le_bytes(s.try_into().unwrap()))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }
    fn pos(&mut self) -> Result<BlockPos> {
        Ok(BlockPos::new(self.i64()?, self.i64()?, self.i64()?))
    }
}

fn light_type_to_u8(t: LightType) -> u8 {
    match t {
        LightType::Sky => 0,
        LightType::Block => 1,
    }
}
fn light_type_from_u8(v: u8) -> Result<LightType> {
    match v {
        0 => Ok(LightType::Sky),
        1 => Ok(LightType::Block),
        other => Err(anyhow!("bad light type {other}")),
    }
}

fn encode_payload(buf: &mut Vec<u8>, p: &EventPayload) {
    match p {
        EventPayload::BlockSet { pos, old, new } => {
            buf.push(0);
            put_pos(buf, *pos);
            put_u16(buf, old.0);
            put_u16(buf, new.0);
        }
        EventPayload::BlockNotify { pos } => {
            buf.push(1);
            put_pos(buf, *pos);
        }
        EventPayload::LightSet { pos, light_type, old, new } => {
            buf.push(2);
            put_pos(buf, *pos);
            buf.push(light_type_to_u8(*light_type));
            buf.push(*old);
            buf.push(*new);
        }
        EventPayload::LightNotify { pos } => {
            buf.push(3);
            put_pos(buf, *pos);
        }
        EventPayload::LightBatch { changes } => {
            buf.push(4);
            buf.extend_from_slice(&(changes.len() as u32).to_le_bytes());
            for c in changes.iter() {
                put_pos(buf, c.pos);
                buf.push(light_type_to_u8(c.light_type));
                buf.push(c.old);
                buf.push(c.new);
            }
        }
    }
}

fn decode_payload(r: &mut Reader) -> Result<EventPayload> {
    Ok(match r.u8()? {
        0 => EventPayload::BlockSet {
            pos: r.pos()?,
            old: BlockId(r.u16()?),
            new: BlockId(r.u16()?),
        },
        1 => EventPayload::BlockNotify { pos: r.pos()? },
        2 => EventPayload::LightSet {
            pos: r.pos()?,
            light_type: light_type_from_u8(r.u8()?)?,
            old: r.u8()?,
            new: r.u8()?,
        },
        3 => EventPayload::LightNotify { pos: r.pos()? },
        4 => {
            let n = r.u32()? as usize;
            let mut cells = Vec::with_capacity(n);
            for _ in 0..n {
                cells.push(LightCell {
                    pos: r.pos()?,
                    light_type: light_type_from_u8(r.u8()?)?,
                    old: r.u8()?,
                    new: r.u8()?,
                });
            }
            EventPayload::LightBatch { changes: cells.into() }
        }
        other => return Err(anyhow!("bad payload tag {other}")),
    })
}

// ── Frames ──────────────────────────────────────────────────────────────────

const KIND_FORWARD: u8 = 0;
const KIND_ACTION: u8 = 1;
const KIND_WRITE_SYNC: u8 = 2;
const KIND_PING: u8 = 3;
const KIND_PONG: u8 = 4;
const KIND_HELLO: u8 = 5;
const KIND_TRANSFER: u8 = 6;

enum OutFrame {
    Forward(Vec<(Event, u8)>),
    Action(BlockAction),
    WriteSync(Vec<EventPayload>),
    Ping(u64),
    /// Quiescence report for the WHOLE responding node: its local
    /// `pending` plus its sent/received totals summed across ALL links —
    /// so a coordinator can detect in-flight traffic on links it can't
    /// see (peer↔peer).
    Pong { token: u64, pending: i64, sent: u64, received: u64 },
    /// Dialer identifies itself immediately after connecting.
    Hello { node_id: u32 },
    /// Region ownership flip (migration). Counted, so quiescence waits
    /// for routing convergence.
    Transfer { region: (i32, i32), new_owner: u32 },
}

fn encode_frame(frame: &OutFrame) -> Vec<u8> {
    let mut body = Vec::with_capacity(64);
    match frame {
        OutFrame::Forward(items) => {
            body.push(KIND_FORWARD);
            body.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for (event, prio) in items {
                body.push(*prio);
                encode_payload(&mut body, &event.payload);
            }
        }
        OutFrame::Action(a) => {
            body.push(KIND_ACTION);
            put_pos(&mut body, a.pos);
            put_u16(&mut body, a.old.0);
            put_u16(&mut body, a.new.0);
            body.push(a.update_stairs as u8);
        }
        OutFrame::WriteSync(payloads) => {
            body.push(KIND_WRITE_SYNC);
            body.extend_from_slice(&(payloads.len() as u32).to_le_bytes());
            for p in payloads {
                encode_payload(&mut body, p);
            }
        }
        OutFrame::Ping(token) => {
            body.push(KIND_PING);
            body.extend_from_slice(&token.to_le_bytes());
        }
        OutFrame::Pong { token, pending, sent, received } => {
            body.push(KIND_PONG);
            body.extend_from_slice(&token.to_le_bytes());
            body.extend_from_slice(&pending.to_le_bytes());
            body.extend_from_slice(&sent.to_le_bytes());
            body.extend_from_slice(&received.to_le_bytes());
        }
        OutFrame::Hello { node_id } => {
            body.push(KIND_HELLO);
            body.extend_from_slice(&node_id.to_le_bytes());
        }
        OutFrame::Transfer { region, new_owner } => {
            body.push(KIND_TRANSFER);
            body.extend_from_slice(&region.0.to_le_bytes());
            body.extend_from_slice(&region.1.to_le_bytes());
            body.extend_from_slice(&new_owner.to_le_bytes());
        }
    }
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

#[derive(Clone, Copy, Debug)]
struct PongData {
    token: u64,
    pending: i64,
    sent: u64,
    received: u64,
}

// ── The link ────────────────────────────────────────────────────────────────

/// One TCP link to the peer node. Created before the physics service
/// ([`connect`](ClusterLink::connect) / [`accept`](ClusterLink::accept)),
/// then [`attach`](ClusterLink::attach)ed once the service exists — early
/// inbound frames simply wait in the socket buffer.
pub struct ClusterLink {
    out_tx: mpsc::Sender<OutFrame>,
    /// Counted frames sent to the peer (Forward/Action/WriteSync only).
    sent: AtomicU64,
    /// Counted frames fully processed from the peer.
    received: AtomicU64,
    /// Socket for the deferred reader thread.
    inbound: Mutex<Option<TcpStream>>,
    /// Latest pong, slotted by the reader thread.
    pong: Mutex<Option<PongData>>,
    pong_cv: Condvar,
}

impl ClusterLink {
    /// Dial a peer, identifying ourselves with a `Hello` frame (FIFO
    /// guarantees it's the first frame the acceptor sees).
    pub fn connect(addr: &str, my_node_id: u32) -> Result<Arc<Self>> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let stream = loop {
            match TcpStream::connect(addr) {
                Ok(s) => break s,
                Err(e) if Instant::now() < deadline => {
                    tracing::debug!("cluster connect retry: {e}");
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(e) => return Err(anyhow!("cluster connect {addr}: {e}")),
            }
        };
        let link = Self::from_stream(stream)?;
        let _ = link.out_tx.send(OutFrame::Hello { node_id: my_node_id });
        Ok(link)
    }

    /// Accept one peer and read its `Hello` to learn who it is.
    pub fn accept_identified(listener: &TcpListener) -> Result<(u32, Arc<Self>)> {
        let (mut stream, peer) = listener.accept()?;
        // Read exactly one frame (the Hello) inline, before the reader
        // thread exists.
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf)?;
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body)?;
        let mut r = Reader::new(&body);
        if r.u8()? != KIND_HELLO {
            return Err(anyhow!("peer {peer} did not start with Hello"));
        }
        let node_id = r.u32()?;
        tracing::info!("cluster: node {node_id} connected from {peer}");
        Ok((node_id, Self::from_stream(stream)?))
    }

    fn from_stream(stream: TcpStream) -> Result<Arc<Self>> {
        stream.set_nodelay(true)?;
        let write_half = stream.try_clone()?;
        let (out_tx, out_rx) = mpsc::channel::<OutFrame>();

        let link = Arc::new(Self {
            out_tx,
            sent: AtomicU64::new(0),
            received: AtomicU64::new(0),
            inbound: Mutex::new(Some(stream)),
            pong: Mutex::new(None),
            pong_cv: Condvar::new(),
        });

        // Writer thread: drains the outbox onto the socket.
        let writer_link = Arc::clone(&link);
        std::thread::Builder::new()
            .name("cluster-writer".into())
            .spawn(move || {
                let mut write = write_half;
                while let Ok(frame) = out_rx.recv() {
                    let counted = matches!(
                        frame,
                        OutFrame::Forward(_)
                            | OutFrame::Action(_)
                            | OutFrame::WriteSync(_)
                            | OutFrame::Transfer { .. }
                    );
                    let bytes = encode_frame(&frame);
                    // Count BEFORE the peer can possibly observe the frame.
                    if counted {
                        writer_link.sent.fetch_add(1, Ordering::SeqCst);
                    }
                    if write.write_all(&bytes).is_err() {
                        tracing::error!("cluster writer: peer link lost");
                        return;
                    }
                }
            })
            .expect("cluster writer thread");

        Ok(link)
    }

    /// Start this link's reader thread, wiring inbound frames into the
    /// node's physics service, replica world, client bus, and mesh
    /// routing table. Called by [`ClusterMesh::attach`].
    fn start_reader(
        self: &Arc<Self>,
        mesh: std::sync::Weak<ClusterMesh>,
        world: Arc<World>,
        bus: Arc<SpatialBus>,
        physics: PhysicsHandle,
    ) {
        let stream = self
            .inbound
            .lock()
            .expect("cluster inbound lock")
            .take()
            .expect("attach called twice");
        let link = Arc::clone(self);

        std::thread::Builder::new()
            .name("cluster-reader".into())
            .spawn(move || {
                let mut read = stream;
                let mut len_buf = [0u8; 4];
                loop {
                    if read.read_exact(&mut len_buf).is_err() {
                        tracing::info!("cluster reader: peer link closed");
                        return;
                    }
                    let len = u32::from_le_bytes(len_buf) as usize;
                    let mut body = vec![0u8; len];
                    if read.read_exact(&mut body).is_err() {
                        tracing::error!("cluster reader: truncated frame");
                        return;
                    }
                    let Some(mesh) = mesh.upgrade() else {
                        return; // mesh dropped — shutting down
                    };
                    if let Err(e) = link.dispatch(&mesh, &body, &world, &bus, &physics) {
                        tracing::error!("cluster reader: {e:#}");
                        return;
                    }
                }
            })
            .expect("cluster reader thread");
    }

    fn dispatch(
        &self,
        mesh: &ClusterMesh,
        body: &[u8],
        world: &World,
        bus: &SpatialBus,
        physics: &PhysicsHandle,
    ) -> Result<()> {
        let mut r = Reader::new(body);
        match r.u8()? {
            KIND_FORWARD => {
                let n = r.u32()? as usize;
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    let prio = r.u8()?;
                    items.push((Event { payload: decode_payload(&mut r)? }, prio));
                }
                // Inject (raising local pending) BEFORE counting received,
                // so a Pong can never show this frame as received while
                // its work is invisible to `pending`.
                physics.submit_forwards(items);
                self.received.fetch_add(1, Ordering::SeqCst);
            }
            KIND_ACTION => {
                let action = BlockAction {
                    pos: r.pos()?,
                    old: BlockId(r.u16()?),
                    new: BlockId(r.u16()?),
                    update_stairs: r.u8()? != 0,
                };
                physics.submit_action_local(action);
                self.received.fetch_add(1, Ordering::SeqCst);
            }
            KIND_WRITE_SYNC => {
                let n = r.u32()? as usize;
                let mut payloads = Vec::with_capacity(n);
                for _ in 0..n {
                    payloads.push(decode_payload(&mut r)?);
                }
                apply_replica_writes(world, &payloads);
                // Republish spatially so THIS node's clients see physics
                // computed on the peer.
                let changes = event_bus::collect_block_changes(&payloads);
                let light_changes = event_bus::collect_light_changes(&payloads);
                bus.publish_world(ChangeSource::Physics, changes, light_changes);
                self.received.fetch_add(1, Ordering::SeqCst);
            }
            KIND_PING => {
                let token = r.u64()?;
                // Report NODE totals (all links), not just this link —
                // the asker may not be able to see our other links'
                // in-flight traffic otherwise.
                let (sent, received) = mesh.totals();
                let _ = self.out_tx.send(OutFrame::Pong {
                    token,
                    pending: physics.pending(),
                    sent,
                    received,
                });
            }
            KIND_PONG => {
                let data = PongData {
                    token: r.u64()?,
                    pending: r.i64()?,
                    sent: r.u64()?,
                    received: r.u64()?,
                };
                *self.pong.lock().expect("pong lock") = Some(data);
                self.pong_cv.notify_all();
            }
            KIND_TRANSFER => {
                let region = (r.u32()? as i32, r.u32()? as i32);
                let new_owner = r.u32()?;
                mesh.apply_transfer(region, new_owner);
                self.received.fetch_add(1, Ordering::SeqCst);
            }
            KIND_HELLO => {
                return Err(anyhow!("unexpected Hello after handshake"));
            }
            other => return Err(anyhow!("bad frame kind {other}")),
        }
        Ok(())
    }

    // ── Senders (called from physics) ───────────────────────────────────

    pub(crate) fn send_forward(&self, items: Vec<(Event, u8)>) {
        if !items.is_empty() {
            let _ = self.out_tx.send(OutFrame::Forward(items));
        }
    }

    pub(crate) fn send_action(&self, action: BlockAction) {
        let _ = self.out_tx.send(OutFrame::Action(action));
    }

    pub(crate) fn send_write_sync(&self, payloads: Vec<EventPayload>) {
        if !payloads.is_empty() {
            let _ = self.out_tx.send(OutFrame::WriteSync(payloads));
        }
    }

    /// Send a Ping and wait for the matching Pong (node-level totals).
    fn ping_pong(&self, token: u64, deadline: Instant) -> Option<PongData> {
        let _ = self.out_tx.send(OutFrame::Ping(token));
        let mut slot = self.pong.lock().expect("pong lock");
        loop {
            if let Some(p) = *slot {
                if p.token == token {
                    return Some(p);
                }
            }
            let timeout = deadline.saturating_duration_since(Instant::now());
            if timeout.is_zero() {
                return None;
            }
            let (s, _) = self.pong_cv.wait_timeout(slot, timeout).expect("pong wait");
            slot = s;
        }
    }
}

// ── The mesh ────────────────────────────────────────────────────────────────

/// Full mesh of N nodes: one [`ClusterLink`] per peer plus the shared
/// region-ownership override table (migrations). All routing questions go
/// through [`owner`](ClusterMesh::owner).
pub struct ClusterMesh {
    pub node_id: u32,
    pub total_nodes: u32,
    /// Region ownership maps onto nodes `0..physics_nodes` only. Nodes
    /// with `node_id >= physics_nodes` are **gateways**: they join the
    /// mesh (replica world via WriteSync, action submission, player
    /// serving) but own no regions and execute no physics.
    pub physics_nodes: u32,
    links: Vec<Option<Arc<ClusterLink>>>,
    /// Migration overrides: region → owning node. Missing = hash default.
    overrides: Mutex<std::collections::HashMap<(i32, i32), u32>>,
}

impl ClusterMesh {
    pub fn new(node_id: u32, total_nodes: u32, links: Vec<Option<Arc<ClusterLink>>>) -> Arc<Self> {
        Self::new_with_physics(node_id, total_nodes, total_nodes, links)
    }

    pub fn new_with_physics(
        node_id: u32,
        total_nodes: u32,
        physics_nodes: u32,
        links: Vec<Option<Arc<ClusterLink>>>,
    ) -> Arc<Self> {
        assert_eq!(links.len(), total_nodes as usize);
        assert!(links[node_id as usize].is_none(), "no link to self");
        assert!(physics_nodes >= 1 && physics_nodes <= total_nodes);
        Arc::new(Self {
            node_id,
            total_nodes,
            physics_nodes,
            links,
            overrides: Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Form a full mesh: dial every lower-id node (their listen
    /// addresses given in `peer_addrs[id]`), accept every higher-id node
    /// on `listener`. Symmetric across all nodes ⇒ no connect storms.
    pub fn form(
        node_id: u32,
        total_nodes: u32,
        listener: &TcpListener,
        peer_addrs: &[String],
    ) -> Result<Arc<Self>> {
        Self::form_with_physics(node_id, total_nodes, total_nodes, listener, peer_addrs)
    }

    /// Like [`form`](Self::form), with only the first `physics_nodes`
    /// ids owning regions (the rest are gateways).
    pub fn form_with_physics(
        node_id: u32,
        total_nodes: u32,
        physics_nodes: u32,
        listener: &TcpListener,
        peer_addrs: &[String],
    ) -> Result<Arc<Self>> {
        let mut links: Vec<Option<Arc<ClusterLink>>> = (0..total_nodes).map(|_| None).collect();
        for lower in 0..node_id {
            links[lower as usize] = Some(ClusterLink::connect(&peer_addrs[lower as usize], node_id)?);
        }
        let mut expected: usize = (total_nodes - node_id - 1) as usize;
        while expected > 0 {
            let (peer_id, link) = ClusterLink::accept_identified(listener)?;
            if peer_id <= node_id || peer_id >= total_nodes || links[peer_id as usize].is_some() {
                return Err(anyhow!("unexpected peer id {peer_id}"));
            }
            links[peer_id as usize] = Some(link);
            expected -= 1;
        }
        Ok(Self::new_with_physics(node_id, total_nodes, physics_nodes, links))
    }

    /// Owning node of a chunk: migration override if present, else the
    /// deterministic hash — always one of the physics nodes.
    pub fn owner(&self, chunk: ChunkPos) -> u32 {
        let region = (chunk.x >> 2, chunk.z >> 2);
        if let Some(&o) = self.overrides.lock().expect("overrides").get(&region) {
            return o % self.physics_nodes;
        }
        owner_node(chunk, self.physics_nodes)
    }

    fn link_to(&self, node: u32) -> &Arc<ClusterLink> {
        self.links[node as usize]
            .as_ref()
            .expect("no link to that node (self?)")
    }

    pub(crate) fn send_forward(&self, node: u32, items: Vec<(Event, u8)>) {
        self.link_to(node).send_forward(items);
    }

    pub(crate) fn send_action(&self, node: u32, action: BlockAction) {
        self.link_to(node).send_action(action);
    }

    /// Mirror executed writes to every peer (each keeps a full replica
    /// for its clients and for border reads).
    pub(crate) fn broadcast_write_sync(&self, payloads: Vec<EventPayload>) {
        if payloads.is_empty() {
            return;
        }
        for link in self.links.iter().flatten() {
            link.send_write_sync(payloads.clone());
        }
    }

    /// Migrate a region to a new owner: install the override locally and
    /// broadcast the flip. State transfer is unnecessary — every node's
    /// replica is already current via WriteSync (deterministic worldgen +
    /// mirrored writes). During propagation the old and new owner may
    /// briefly both execute events for the region: the same transient
    /// dual-ownership the intra-node rebalancer produces, tolerated by
    /// the stale guard + confluent rules. Single-initiator assumption:
    /// concurrent conflicting migrations of the SAME region are not
    /// arbitrated in this prototype.
    pub fn migrate_region(&self, region: (i32, i32), new_owner: u32) {
        self.apply_transfer(region, new_owner);
        for link in self.links.iter().flatten() {
            let _ = link.out_tx.send(OutFrame::Transfer { region, new_owner });
        }
        tracing::info!(
            "cluster: region {:?} migrated to node {} (announced to {} peers)",
            region, new_owner, self.total_nodes - 1,
        );
    }

    fn apply_transfer(&self, region: (i32, i32), new_owner: u32) {
        self.overrides.lock().expect("overrides").insert(region, new_owner);
    }

    /// This node's sent/received totals across all links.
    pub(crate) fn totals(&self) -> (u64, u64) {
        let mut sent = 0;
        let mut received = 0;
        for link in self.links.iter().flatten() {
            sent += link.sent.load(Ordering::SeqCst);
            received += link.received.load(Ordering::SeqCst);
        }
        (sent, received)
    }

    /// Start all reader threads.
    pub fn attach(
        self: &Arc<Self>,
        world: Arc<World>,
        bus: Arc<SpatialBus>,
        physics: PhysicsHandle,
    ) {
        for link in self.links.iter().flatten() {
            link.start_reader(
                Arc::downgrade(self),
                Arc::clone(&world),
                Arc::clone(&bus),
                physics.clone(),
            );
        }
    }

    /// One global snapshot: quiet iff every node's pending is 0 and the
    /// mesh-wide counted-frame ledger balances (Σ sent == Σ received,
    /// including links this node can't see — peers report node totals).
    fn quiet_round(&self, physics: &PhysicsHandle, token: u64) -> bool {
        if physics.pending() != 0 {
            return false;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let (mut global_sent, mut global_received) = self.totals();
        for link in self.links.iter().flatten() {
            match link.ping_pong(token, deadline) {
                Some(p) if p.pending == 0 => {
                    global_sent += p.sent;
                    global_received += p.received;
                }
                _ => return false,
            }
        }
        global_sent == global_received && physics.pending() == 0
    }

    /// Block until the whole mesh is quiescent (two consecutive quiet
    /// rounds) or `timeout` elapses.
    pub fn wait_global_quiet(&self, physics: &PhysicsHandle, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut token = 1u64;
        let mut streak = 0;
        while Instant::now() < deadline {
            if self.quiet_round(physics, token) {
                streak += 1;
                if streak >= 2 {
                    return true;
                }
            } else {
                streak = 0;
                std::thread::sleep(Duration::from_millis(2));
            }
            token += 1;
        }
        false
    }
}

/// Apply a peer's executed writes to the local replica world. These are
/// authoritative outcomes from the owner — applied verbatim (no stale
/// guard, no dirty marking, no rule evaluation).
fn apply_replica_writes(world: &World, payloads: &[EventPayload]) {
    for p in payloads {
        match p {
            EventPayload::BlockSet { pos, new, .. } => {
                world.set_block_untracked(*pos, *new);
            }
            EventPayload::LightSet { pos, light_type, new, .. } => match light_type {
                LightType::Sky => {
                    world.set_sky_light_if_loaded(*pos, *new);
                }
                LightType::Block => {
                    world.set_block_light_if_loaded(*pos, *new);
                }
            },
            EventPayload::LightBatch { changes } => {
                for c in changes.iter() {
                    match c.light_type {
                        LightType::Sky => {
                            world.set_sky_light_if_loaded(c.pos, c.new);
                        }
                        LightType::Block => {
                            world.set_block_light_if_loaded(c.pos, c.new);
                        }
                    }
                }
            }
            EventPayload::BlockNotify { .. } | EventPayload::LightNotify { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_codec_roundtrip() {
        let payloads = vec![
            EventPayload::BlockSet {
                pos: BlockPos::new(-5, 64, 1 << 40),
                old: BlockId(0),
                new: BlockId(118),
            },
            EventPayload::BlockNotify { pos: BlockPos::new(1, -64, -1) },
            EventPayload::LightSet {
                pos: BlockPos::new(0, 0, 0),
                light_type: LightType::Block,
                old: 3,
                new: 14,
            },
            EventPayload::LightNotify { pos: BlockPos::new(7, 7, 7) },
            EventPayload::LightBatch {
                changes: vec![
                    LightCell {
                        pos: BlockPos::new(2, 5, 9),
                        light_type: LightType::Sky,
                        old: 0,
                        new: 15,
                    },
                    LightCell {
                        pos: BlockPos::new(-9, 70, 3),
                        light_type: LightType::Block,
                        old: 14,
                        new: 0,
                    },
                ]
                .into(),
            },
        ];

        let mut buf = Vec::new();
        for p in &payloads {
            encode_payload(&mut buf, p);
        }
        let mut r = Reader::new(&buf);
        for expect in &payloads {
            let got = decode_payload(&mut r).unwrap();
            assert_eq!(format!("{expect:?}"), format!("{got:?}"));
        }
        assert_eq!(r.at, buf.len(), "codec must consume exactly what it wrote");
    }

    #[test]
    fn owner_node_is_deterministic_and_region_grained() {
        assert_eq!(owner_node(ChunkPos::new(3, 9), 2), owner_node(ChunkPos::new(3, 9), 2));
        // All chunks of a region share a node.
        let n = owner_node(ChunkPos::new(8, 8), 2);
        for cx in 8..12 {
            for cz in 8..12 {
                assert_eq!(owner_node(ChunkPos::new(cx, cz), 2), n);
            }
        }
        // Single node owns everything.
        for cx in -40..40 {
            assert_eq!(owner_node(ChunkPos::new(cx, -cx), 1), 0);
        }
        // Two nodes both own a reasonable share.
        let mut counts = [0usize; 2];
        for rx in -16..16 {
            for rz in -16..16 {
                counts[owner_node(ChunkPos::new(rx * 4, rz * 4), 2) as usize] += 1;
            }
        }
        assert!(counts[0] > 256 && counts[1] > 256, "node split skewed: {counts:?}");
    }
}

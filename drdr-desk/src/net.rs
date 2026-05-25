//! DrDrNet **over the wire** — phase 8 wiring.
//!
//! Before phase 8 DrDrDesk ran the DrDrNet reactor on 127.0.0.1 and only
//! the desktop's own [`NetApp`] talked to it. Phase 8 puts the same
//! protocol on a real network interface so two DrDrOS machines on the
//! same LAN can find each other and exchange chat lines — with no
//! configuration, no broker, no SDP.
//!
//! Three pieces, three threads:
//!
//!   1. **TCP reactor** — Tier 3 [`reactor::Listener`] bound to
//!      `0.0.0.0:0` (the OS picks a free port; we read it back). One
//!      handler multiplexes [`status::KIND_STAT_REQ`] (the existing
//!      NetApp panel) and [`chat::KIND_CHAT_SAY`] (a peer pushing a
//!      chat line). Status frames get a `Stat` reply; chat frames are
//!      appended to a shared log and consumed by DrDrChat.
//!   2. **UDP HELLO broadcaster** — broadcasts our [`Peer`] every
//!      [`HELLO_INTERVAL`] on UDP port [`DISCOVERY_PORT`] so other
//!      DrDrOS nodes know we exist.
//!   3. **UDP receiver** — listens on the same port, feeds HELLOs +
//!      BYEs into a shared [`PeerDirectory`], sweeps stale peers.
//!
//! Everything lives behind `Arc<Mutex<…>>` handles: the apps in
//! `apps.rs` clone the arcs and read them at render time. The mutex is
//! contended only at the speed of human chat / heartbeats — not hot.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use drdr_net::chat::{ChatMsg, KIND_CHAT_SAY};
use drdr_net::discovery::{
    self, DISCOVERY_PORT, HELLO_INTERVAL, KIND_HELLO, Peer, PeerDirectory, build_hello,
};
use drdr_net::status::{KIND_STAT_OK, KIND_STAT_REQ, Stat};
use drdr_net::{Frame, pack, reactor, unpack};

/// Hard cap on the chat log so a noisy peer can't grow drdr-desk's
/// memory forever. 256 lines is plenty for an interactive window — a
/// rolling buffer drops the oldest entries as new ones arrive.
const CHAT_LOG_CAP: usize = 256;

/// Everything DrDrChat and the [`NetApp`] need to share with the
/// background networking threads. Cheap to clone — every field is an
/// `Arc` or a `Copy`-ish primitive.
#[derive(Clone)]
pub struct NetState {
    /// Live peers, by id. Updated by the UDP receiver thread; read by
    /// DrDrChat each render.
    pub directory: Arc<Mutex<PeerDirectory>>,
    /// Append-only chat log, capped at [`CHAT_LOG_CAP`]. The reactor
    /// pushes incoming lines; DrDrChat's `send_to` push outgoing ones
    /// (with `from = me.host`) so the local user sees their own messages.
    pub chat_log: Arc<Mutex<Vec<ChatMsg>>>,
    /// This node, as we announce ourselves on the wire.
    pub me: Peer,
    /// The address the local reactor is actually listening on (port is
    /// chosen by the OS at bind time). The legacy NetApp also dials this
    /// over loopback to render its status panel.
    pub reactor_addr: SocketAddr,
}

impl NetState {
    /// Stand the whole DrDrNet-over-the-wire stack up. On unrecoverable
    /// errors (e.g. the OS refuses any TCP bind at all) returns `Err`
    /// and the caller falls back to a desktop with no DrDrNet panel —
    /// same graceful degradation as before phase 8.
    pub fn start(host: String) -> io::Result<Self> {
        // Bind the reactor to *every* interface so peers on the LAN can
        // reach us; port 0 → the kernel picks a free one (the discovery
        // HELLO carries the chosen port so peers know where to dial).
        let listener = reactor::Listener::bind("0.0.0.0:0")?;
        let reactor_addr = listener.local_addr()?;

        let me = Peer {
            id: discovery::fresh_self_id(),
            host: host.clone(),
            tcp_port: reactor_addr.port(),
        };

        let directory: Arc<Mutex<PeerDirectory>> = Arc::new(Mutex::new(PeerDirectory::new()));
        let chat_log: Arc<Mutex<Vec<ChatMsg>>> = Arc::new(Mutex::new(Vec::new()));

        // ── Reactor thread: status + chat in one handler. ───────────
        let chat_log_rx = Arc::clone(&chat_log);
        let me_for_status = me.clone();
        let start = Instant::now();
        thread::spawn(move || {
            let mut served: u64 = 0;
            let _ = listener.run(move |f: &Frame| match f.kind {
                KIND_STAT_REQ => {
                    served += 1;
                    let stat = Stat {
                        uptime_secs: start.elapsed().as_secs(),
                        requests: served,
                        host: me_for_status.host.clone(),
                    };
                    Some(Frame::with_id(KIND_STAT_OK, f.id, pack(&stat)))
                }
                KIND_CHAT_SAY => {
                    if let Ok(msg) = unpack::<ChatMsg>(&f.payload) {
                        push_chat(&chat_log_rx, msg);
                    }
                    None // fire-and-forget; no reply
                }
                _ => None,
            });
        });

        // ── UDP discovery: HELLO broadcaster + receiver. Best-effort:
        // a kernel that won't grant SO_BROADCAST or a missing route on
        // a brand-new VM is a soft failure; the desktop still works, we
        // just don't find peers.
        if let Err(e) = start_discovery(me.clone(), Arc::clone(&directory)) {
            eprintln!("[drdr-desk] discovery disabled: {e}");
        }

        Ok(Self { directory, chat_log, me, reactor_addr })
    }
}

/// Append a chat line to the shared log, dropping the oldest if we've
/// hit the cap. Mutex held only for the push.
pub fn push_chat(log: &Arc<Mutex<Vec<ChatMsg>>>, msg: ChatMsg) {
    if let Ok(mut g) = log.lock() {
        if g.len() >= CHAT_LOG_CAP {
            g.remove(0);
        }
        g.push(msg);
    }
}

/// Current wall-clock seconds since the Unix epoch. Zero on a clockless
/// system rather than panicking — a chat line with `ts = 0` still works.
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Spawn HELLO broadcaster + receiver threads. They share a `UdpSocket`
/// the way the BSD examples do — one bind, two roles.
fn start_discovery(me: Peer, directory: Arc<Mutex<PeerDirectory>>) -> io::Result<()> {
    // Bind to every interface so we receive HELLOs from any subnet the
    // kernel routes to us. One drdr-desk per host, so SO_REUSEADDR
    // isn't needed; the bind fails noisily if a second instance starts.
    let socket = UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT))?;
    socket.set_broadcast(true)?;
    // 250 ms read timeout so the receive loop can wake periodically and
    // sweep stale peers even on a totally quiet LAN.
    socket.set_read_timeout(Some(Duration::from_millis(250)))?;
    let socket = Arc::new(socket);

    // ── Sender thread.
    let sock_tx = Arc::clone(&socket);
    let me_tx = me.clone();
    thread::spawn(move || {
        // Broadcast address — every host on the local subnet listens.
        let bcast = SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), DISCOVERY_PORT);
        loop {
            let _ = sock_tx.send_to(&build_hello(&me_tx), bcast);
            thread::sleep(HELLO_INTERVAL);
        }
    });

    // ── Receiver thread.
    let sock_rx = Arc::clone(&socket);
    let dir_rx = Arc::clone(&directory);
    let self_id = me.id;
    thread::spawn(move || {
        let mut buf = [0u8; 1500];
        let mut last_sweep = Instant::now();
        loop {
            match sock_rx.recv_from(&mut buf) {
                Ok((n, src)) => {
                    if let Ok((kind, peer)) = discovery::parse_announcement(&buf[..n]) {
                        // Don't add ourselves to our own directory — we
                        // hear our own broadcast on a loopback'd subnet.
                        if peer.id != self_id {
                            if let Ok(mut d) = dir_rx.lock() {
                                if kind == KIND_HELLO {
                                    d.observe_hello(peer, src.ip(), Instant::now());
                                } else {
                                    d.observe_bye(peer.id);
                                }
                            }
                        }
                    }
                }
                // Timeout — fall through to the periodic sweep.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
                Err(_) => continue,
            }
            if last_sweep.elapsed() >= Duration::from_secs(1) {
                last_sweep = Instant::now();
                if let Ok(mut d) = dir_rx.lock() {
                    d.sweep(Instant::now());
                }
            }
        }
    });

    // BYE on Ctrl-C is "best effort and rarely runs" — we'd need a
    // dedicated signal-handler thread to do better, and the TTL on the
    // peer side already collapses the entry within PEER_TTL. So we
    // simply rely on the timeout for the disorderly cases.
    let _ = me;
    Ok(())
}

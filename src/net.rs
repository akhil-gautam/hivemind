//! LAN discovery: interface discovery + a tiny UDP multicast beacon.
//!
//! Nodes announce themselves on `239.255.42.99:4789` (TTL 1, link-local) and
//! additionally broadcast on the interface broadcast address for APs that
//! filter multicast. One datagram every 2 s keeps the peer table fresh.

use crate::util::log;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(2);
pub const PEER_TTL: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
pub struct Iface {
    pub name: String,
    pub ipv4: Option<Ipv4Addr>,
    pub netmask: Option<Ipv4Addr>,
    pub broadcast: Option<Ipv4Addr>,
    pub multicast: bool,
    pub up: bool,
}

/// Parse `ifconfig -a` output. Avoids pulling in a libc struct layout.
pub fn interfaces() -> Vec<Iface> {
    let out = match std::process::Command::new("ifconfig").arg("-a").output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return Vec::new(),
    };
    let mut list: Vec<Iface> = Vec::new();
    for line in out.lines() {
        if line.contains("flags=") {
            let head = line.split(':').next().unwrap_or("").trim().to_string();
            if !head.is_empty() && head.chars().all(|c| c.is_ascii_alphanumeric()) {
                let flags = line
                    .split_once('<')
                    .and_then(|(_, rest)| rest.split_once('>'))
                    .map(|(f, _)| f.to_string())
                    .unwrap_or_default();
                list.push(Iface {
                    name: head,
                    ipv4: None,
                    netmask: None,
                    broadcast: None,
                    multicast: flags.contains("MULTICAST"),
                    up: flags.contains("UP"),
                });
            }
            continue;
        }
        let t = line.trim();
        let Some(rest) = t.strip_prefix("inet ") else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let addr = parts.next().unwrap_or("").to_string();
        let mut netmask = None;
        let mut broadcast = None;
        while let Some(kv) = parts.next() {
            if let Some(v) = kv.strip_prefix("netmask=") {
                netmask = Some(v.to_string());
            } else if let Some(v) = kv.strip_prefix("broadcast=") {
                broadcast = Some(v.to_string());
            }
        }
        if let Some(iface) = list.last_mut() {
            if iface.ipv4.is_none() {
                iface.ipv4 = addr.parse().ok();
                iface.netmask = netmask.and_then(|s| s.parse().ok());
                iface.broadcast = broadcast.and_then(|s| s.parse().ok());
            }
        }
    }
    list
}

/// Best guess at the address other machines on the WiFi can reach.
pub fn primary_ipv4(ifaces: &[Iface]) -> Option<Ipv4Addr> {
    let skip = ["utun", "awdl", "llw", "bridge", "gif", "stf", "anpi", "ap1", "vmenet"];
    let mut fallback = None;
    for iface in ifaces {
        let Some(ip) = iface.ipv4 else { continue };
        if ip.is_loopback() {
            continue;
        }
        if fallback.is_none() {
            fallback = Some(ip);
        }
        if !iface.up || !iface.multicast {
            continue;
        }
        if skip.iter().any(|p| iface.name.starts_with(p)) {
            continue;
        }
        if iface.broadcast.is_none() {
            continue;
        }
        return Some(ip);
    }
    fallback
}

#[derive(Clone, Debug)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub agent: String,
    pub last_seen_ms: u64,
}

impl Peer {
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "host": self.host,
            "port": self.port,
            "agent": self.agent,
            "last_seen_ms": self.last_seen_ms,
        })
    }
}

/// What a node tells the mesh about itself. Kept behind a mutex so the UI can
/// rename a Mac or change its agent without a restart.
#[derive(Clone, Debug)]
pub struct Identity {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub agent: String,
}

pub struct Discovery {
    pub peers: Arc<Mutex<HashMap<String, Peer>>>,
}

impl Discovery {
    pub fn start(
        identity: Arc<Mutex<Identity>>,
        group: &str,
        discovery_port: u16,
        ifaces: Vec<Iface>,
    ) -> std::io::Result<Arc<Discovery>> {
        let socket = make_socket(discovery_port)?;
        let group_ip: Ipv4Addr = group.parse().unwrap_or(Ipv4Addr::new(239, 255, 42, 99));

        // Join the group on every usable interface so the beacon is received
        // regardless of which WiFi the Mac is on.
        for iface in &ifaces {
            if let Some(ip) = iface.ipv4 {
                if iface.multicast && !ip.is_loopback() {
                    let _ = socket.join_multicast_v4(&group_ip, &ip);
                }
            }
        }
        let _ = socket.join_multicast_v4(&group_ip, &Ipv4Addr::UNSPECIFIED);
        if let Some(primary) = primary_ipv4(&ifaces) {
            let _ = socket.set_multicast_if_v4(&primary);
        }
        socket.set_read_timeout(Some(Duration::from_millis(500)))?;
        let udp: UdpSocket = socket.try_clone()?.into();

        let peers: Arc<Mutex<HashMap<String, Peer>>> = Arc::new(Mutex::new(HashMap::new()));

        let identity2 = identity.clone();
        let announce = move || -> (Vec<u8>, String) {
            let id = identity2.lock().unwrap();
            let bytes = serde_json::to_vec(&serde_json::json!({
                "t": "announce",
                "id": id.id,
                "name": id.name,
                "host": id.host,
                "port": id.port,
                "agent": id.agent,
                "ts": crate::util::now_ms(),
            }))
            .unwrap_or_default();
            (bytes, id.id.clone())
        };
        let hello_bytes = {
            let id = identity.lock().unwrap();
            serde_json::to_vec(&serde_json::json!({ "t": "hello", "id": id.id, "name": id.name }))
                .unwrap_or_default()
        };

        let targets: Vec<SocketAddr> = {
            let mut t: Vec<SocketAddr> = vec![SocketAddr::new(group_ip.into(), discovery_port)];
            for iface in &ifaces {
                if let Some(b) = iface.broadcast {
                    if iface.multicast && !b.is_unspecified() {
                        t.push(SocketAddr::new(std::net::IpAddr::V4(b), discovery_port));
                    }
                }
            }
            t
        };

        let peers2 = peers.clone();
        let targets2 = targets.clone();
        let sock = socket.try_clone()?;
        let announce2 = announce.clone();
        std::thread::Builder::new()
            .name("hivemind-discovery".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let mut last_announce = Instant::now() - ANNOUNCE_INTERVAL;
                let mut buf = [0u8; 2048];
                loop {
                    let (ann_bytes, self_id) = announce2();
                    // Send periodic announcements.
                    if last_announce.elapsed() >= ANNOUNCE_INTERVAL {
                        for target in &targets2 {
                            if let Err(e) = sock.send_to(&ann_bytes, &SockAddr::from(*target)) {
                                log(
                                    "debug",
                                    &format!("beacon send to {} failed: {}", target, e),
                                );
                            }
                        }
                        last_announce = Instant::now();
                    }
                    // Read beacons.
                    match udp.recv_from(&mut buf) {
                        Ok((n, from)) => {
                            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) else {
                                continue;
                            };
                            log("debug", &format!("beacon from {}", from));
                            let kind = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
                            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
                            if id.is_empty() || id == self_id {
                                continue;
                            }
                            match kind {
                                "announce" => {
                                    let peer = Peer {
                                        id: id.to_string(),
                                        name: v
                                            .get("name")
                                            .and_then(|x| x.as_str())
                                            .unwrap_or("?")
                                            .to_string(),
                                        host: v
                                            .get("host")
                                            .and_then(|x| x.as_str())
                                            .unwrap_or("?")
                                            .to_string(),
                                        port: v.get("port").and_then(|x| x.as_u64()).unwrap_or(0) as u16,
                                        agent: v
                                            .get("agent")
                                            .and_then(|x| x.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        last_seen_ms: crate::util::now_ms(),
                                    };
                                    let mut map = peers2.lock().unwrap();
                                    map.insert(peer.id.clone(), peer);
                                }
                                "hello" => {
                                    // Reply directly so the newcomer learns us.
                                    let _ = sock.send_to(&ann_bytes, &SockAddr::from(from));
                                }
                                _ => {}
                            }
                        }
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut => {}
                        Err(_) => break,
                    }
                }
            })?;

        // Ask everyone who is already out there.
        for target in &targets {
            let _ = socket.send_to(&hello_bytes, &SockAddr::from(*target));
        }

        let peers3 = peers.clone();
        std::thread::Builder::new()
            .name("hivemind-discovery-prune".into())
            .stack_size(128 * 1024)
            .spawn(move || loop {
                std::thread::sleep(Duration::from_secs(5));
                let mut map = peers3.lock().unwrap();
                let cutoff = crate::util::now_ms() - PEER_TTL.as_millis() as u64;
                map.retain(|_, p| p.last_seen_ms > cutoff);
            })?;

        Ok(Arc::new(Discovery { peers }))
    }

    pub fn peers(&self) -> Vec<Peer> {
        let map = self.peers.lock().unwrap();
        let mut list: Vec<Peer> = map.values().cloned().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }
}

/// Live scan: listen for beacons for `duration` and return the nodes seen.
/// Used by the CLI so it never depends on a stale cache file.
pub fn scan(group: &str, port: u16, ifaces: &[Iface], duration: Duration) -> Vec<Peer> {
    let Ok(socket) = make_socket(port) else {
        return Vec::new();
    };
    let group_ip: Ipv4Addr = group.parse().unwrap_or(Ipv4Addr::new(239, 255, 42, 99));
    for iface in ifaces {
        if let Some(ip) = iface.ipv4 {
            if iface.multicast && !ip.is_loopback() {
                let _ = socket.join_multicast_v4(&group_ip, &ip);
            }
        }
    }
    let _ = socket.join_multicast_v4(&group_ip, &Ipv4Addr::UNSPECIFIED);
    if let Some(primary) = primary_ipv4(ifaces) {
        let _ = socket.set_multicast_if_v4(&primary);
    }
    if socket.set_read_timeout(Some(Duration::from_millis(200))).is_err() {
        return Vec::new();
    }
    let Ok(udp) = socket.try_clone() else {
        return Vec::new();
    };
    let udp: UdpSocket = udp.into();

    let hello = serde_json::json!({ "t": "hello", "id": "scan", "name": "scan" });
    let hello_bytes = serde_json::to_vec(&hello).unwrap_or_default();
    let mut targets: Vec<SocketAddr> = vec![SocketAddr::new(group_ip.into(), port)];
    for iface in ifaces {
        if let Some(b) = iface.broadcast {
            if iface.multicast && !b.is_unspecified() {
                targets.push(SocketAddr::new(std::net::IpAddr::V4(b), port));
            }
        }
    }
    for target in &targets {
        let _ = socket.send_to(&hello_bytes, &SockAddr::from(*target));
    }

    let mut found: HashMap<String, Peer> = HashMap::new();
    let deadline = Instant::now() + duration;
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        match udp.recv_from(&mut buf) {
            Ok((n, _)) => {
                let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) else {
                    continue;
                };
                if v.get("t").and_then(|x| x.as_str()) != Some("announce") {
                    continue;
                }
                let Some(id) = v.get("id").and_then(|x| x.as_str()) else {
                    continue;
                };
                let peer = Peer {
                    id: id.to_string(),
                    name: v
                        .get("name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("?")
                        .to_string(),
                    host: v
                        .get("host")
                        .and_then(|x| x.as_str())
                        .unwrap_or("?")
                        .to_string(),
                    port: v.get("port").and_then(|x| x.as_u64()).unwrap_or(0) as u16,
                    agent: v
                        .get("agent")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    last_seen_ms: crate::util::now_ms(),
                };
                found.insert(peer.id.clone(), peer);
            }
            Err(_) => continue,
        }
    }
    let mut list: Vec<Peer> = found.into_values().collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

fn make_socket(port: u16) -> std::io::Result<Socket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // SO_REUSEADDR lets several nodes share the beacon port; SO_REUSEPORT is
    // what BSD/macOS requires for a second unicast bind on the same port.
    socket.set_reuse_address(true)?;
    let _ = socket.set_reuse_port(true);
    socket.set_broadcast(true)?;
    socket.set_multicast_loop_v4(true)?;
    socket.set_multicast_ttl_v4(1)?;
    let addr = SockAddr::from(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port));
    socket.bind(&addr)?;
    Ok(socket)
}

/// Persist the peer table so the CLI can list nodes without a running daemon.
pub fn save_peers(path: &std::path::Path, peers: &[Peer], self_peer: Option<&Peer>) {
    let mut list: Vec<serde_json::Value> = peers.iter().map(|p| p.to_json()).collect();
    if let Some(p) = self_peer {
        list.push(p.to_json());
    }
    let v = serde_json::json!({ "peers": list, "updated_ms": crate::util::now_ms() });
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(&v) {
        if std::fs::write(path, text + "\n").is_err() {
            log("warn", "could not persist peer table");
        }
    }
}

pub fn load_peers(path: &std::path::Path) -> Vec<Peer> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    v.get("peers")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    Some(Peer {
                        id: p.get("id")?.as_str()?.to_string(),
                        name: p.get("name")?.as_str()?.to_string(),
                        host: p.get("host")?.as_str()?.to_string(),
                        port: p.get("port")?.as_u64()? as u16,
                        agent: p
                            .get("agent")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        last_seen_ms: p
                            .get("last_seen_ms")
                            .and_then(|x| x.as_u64())
                            .unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

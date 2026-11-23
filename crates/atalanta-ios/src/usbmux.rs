// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! A local TCP port forwarded to a port on a USB-connected iPhone through
//! `usbmuxd` (what libimobiledevice's `iproxy` does). WebDriverAgent listens
//! on the device; the driver talks to `localhost:<port>` as for a simulator.
//!
//! Protocol (usbmuxd "plist" messages, version 1): a 16-byte little-endian
//! header — total length, version 1, message type 8, tag — then an XML
//! plist. `ListDevices` returns each device's `DeviceID` and
//! `SerialNumber` (the UDID); `Connect` with a `DeviceID` and a
//! network-order `PortNumber` turns the socket into a raw pipe to that
//! device port on success (`Number` 0).
//!
//! Every exchange with usbmuxd has a deadline (VEL-150: a silent usbmuxd
//! hung the run), cleared once `Connect` succeeds and the socket becomes a
//! raw pipe. The local listener accepts whether or not anything listens on
//! the device, so a dead WebDriverAgent there looks alive through it
//! (VEL-025): [`Forward::probe`] asks the device port itself.

use std::path::{Path, PathBuf};
use std::time::Duration;

use atalanta_common::{AtalantaError, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixStream};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, warn};

const SOCKET: &str = "/var/run/usbmuxd";

/// usbmuxd's `Connect` result when nothing listens on the device port.
const CONNREFUSED: u64 = 3;

/// usbmuxd's socket (`ATALANTA_USBMUXD_SOCKET` overrides, for tests).
fn socket_path() -> PathBuf {
    atalanta_common::env::var("ATALANTA_USBMUXD_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(SOCKET))
}

/// Deadline for one usbmuxd request or reply (appium-ios-device uses 5 s).
fn deadline() -> Duration {
    #[cfg(test)]
    {
        let ms = tests::DEADLINE_MS.load(std::sync::atomic::Ordering::Relaxed);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    atalanta_common::settings::ms("ATALANTA_USBMUX_TIMEOUT_MS", 5000)
}

/// A running forward; dropping it stops listening and every relay.
pub struct Forward {
    task: JoinHandle<()>,
    pub local_port: u16,
    socket: PathBuf,
    /// usbmuxd's number for the device. A replugged phone gets a new one,
    /// so it is looked up again by UDID when a connection is refused as a
    /// bad device (mobile-agent's S9 spike hit 60 s of "bad device").
    device_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    udid: String,
    device_port: u16,
}

impl Drop for Forward {
    fn drop(&mut self) {
        // The accept task owns the relays' JoinSet: aborting it drops the
        // set, which aborts them (VEL-292).
        self.task.abort();
    }
}

/// What a probe of the device port found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// Something answered HTTP on the device port.
    Alive,
    /// The device refused the port, or never answered: the runner is gone.
    Dead,
}

impl Forward {
    /// Whether the runner on the device answers, through a fresh usbmux
    /// connection to its port (as pymobiledevice3's `WdaServiceClient` and
    /// tidevice's `_is_alive` do): refused means dead; any other failure is
    /// checked once more a second later ("maybe stuck by another request").
    pub async fn probe(&self) -> Liveness {
        for attempt in 0..2 {
            let id = self.device_id.load(std::sync::atomic::Ordering::Relaxed);
            let mut answer = probe_once(&self.socket, id, self.device_port).await;
            if matches!(answer, Ok(false)) {
                // A replugged phone has a new number: try it at once.
                let new = rekey(&self.socket, &self.udid, &self.device_id, id).await;
                if new != id {
                    answer = probe_once(&self.socket, new, self.device_port).await;
                }
            }
            match answer {
                Ok(true) => return Liveness::Alive,
                Err(Refused) => return Liveness::Dead,
                Ok(false) if attempt == 0 => tokio::time::sleep(Duration::from_secs(1)).await,
                Ok(false) => {}
            }
        }
        Liveness::Dead
    }
}

/// The device refused the port.
struct Refused;

/// `Ok(true)` when the device port answered an HTTP request.
async fn probe_once(
    socket: &Path,
    device_id: u64,
    port: u16,
) -> std::result::Result<bool, Refused> {
    let mut sock = match connect(socket, device_id, port).await {
        Ok(s) => s,
        Err(ConnectError::Refused) => return Err(Refused),
        Err(ConnectError::Other(e)) => {
            debug!("usbmux probe: {e}");
            return Ok(false);
        }
    };
    let ask = async {
        sock.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut head = [0u8; 5];
        sock.read_exact(&mut head).await?;
        Ok::<_, std::io::Error>(&head == b"HTTP/")
    };
    Ok(matches!(
        tokio::time::timeout(deadline(), ask).await,
        Ok(Ok(true))
    ))
}

/// Forward `127.0.0.1:local_port` to `device_port` on the device `udid`.
pub async fn forward(udid: &str, local_port: u16, device_port: u16) -> Result<Forward> {
    forward_via(&socket_path(), udid, local_port, device_port).await
}

async fn forward_via(
    socket: &Path,
    udid: &str,
    local_port: u16,
    device_port: u16,
) -> Result<Forward> {
    let first_id = device_id_for(socket, udid).await?;
    let device_id = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(first_id));
    let listener = TcpListener::bind(("127.0.0.1", local_port))
        .await
        .map_err(|e| AtalantaError::Config(format!("port {local_port}: {e}")))?;
    let local_port = listener
        .local_addr()
        .map(|a| a.port())
        .unwrap_or(local_port);
    let sock_path = socket.to_path_buf();
    let ids = device_id.clone();
    let task_udid = udid.to_string();
    let task = tokio::spawn(async move {
        let mut relays = JoinSet::new();
        let mut backoff = Duration::ZERO;
        loop {
            let (local, _) = match listener.accept().await {
                Ok(c) => {
                    backoff = Duration::ZERO;
                    c
                }
                Err(e) => {
                    // Out of descriptors and the like: keep serving, but
                    // back off (50 ms → 2 s) and warn once per streak, not
                    // every 50 ms forever.
                    if backoff.is_zero() {
                        warn!("usbmux forward accept: {e}");
                    } else {
                        debug!("usbmux forward accept: {e}");
                    }
                    backoff =
                        (backoff * 2).clamp(Duration::from_millis(50), Duration::from_secs(2));
                    tokio::time::sleep(backoff).await;
                    continue;
                }
            };
            // Reap finished relays so the set doesn't grow for the run.
            while relays.try_join_next().is_some() {}
            let sock_path = sock_path.clone();
            let ids = ids.clone();
            let udid = task_udid.clone();
            relays.spawn(async move {
                let mut local = local;
                let id = ids.load(std::sync::atomic::Ordering::Relaxed);
                let first = connect(&sock_path, id, device_port).await;
                // A bad-device reply after a replug: the same UDID under
                // its new number, once.
                let attempt = match first {
                    Err(ConnectError::Other(_)) => {
                        let new = rekey(&sock_path, &udid, &ids, id).await;
                        if new != id {
                            connect(&sock_path, new, device_port).await
                        } else {
                            first
                        }
                    }
                    other => other,
                };
                match attempt {
                    Ok(mut remote) => {
                        let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
                    }
                    Err(e) => {
                        warn!("usbmux connect failed: {e}");
                        // Reset rather than close, so the client sees at once
                        // that nothing answered. A zero linger never blocks
                        // (the deprecation is about non-zero ones).
                        #[allow(deprecated)]
                        let _ = local.set_linger(Some(Duration::ZERO));
                    }
                }
            });
        }
    });
    debug!(udid, local_port, device_port, "usbmux forward up");
    Ok(Forward {
        task,
        local_port,
        socket: socket.to_path_buf(),
        device_id,
        udid: udid.to_string(),
        device_port,
    })
}

/// usbmuxd's current number for `udid`, stored in `ids` when it changed
/// from `old` (a replug); `old` when the lookup fails.
async fn rekey(socket: &Path, udid: &str, ids: &std::sync::atomic::AtomicU64, old: u64) -> u64 {
    match device_id_for(socket, udid).await {
        Ok(new) if new != old => {
            debug!(
                udid,
                old, new, "usbmux device id changed (replugged); re-keyed"
            );
            ids.store(new, std::sync::atomic::Ordering::Relaxed);
            new
        }
        _ => old,
    }
}

/// usbmuxd's numeric id for the device with this UDID.
async fn device_id_for(socket: &Path, udid: &str) -> Result<u64> {
    let reply = async {
        let mut sock = open(socket).await?;
        send(&mut sock, 1, &request("ListDevices", &[])).await?;
        receive(&mut sock).await
    }
    .await
    .map_err(|e| unreachable(udid, e))?;
    find_device_id(&reply, udid)
        .map(|(id, _)| id)
        .ok_or_else(|| AtalantaError::DeviceNotFound {
            id: udid.to_string(),
            available: vec![
                "(no USB-connected device with that UDID; connect it with a cable)".into(),
            ],
        })
}

fn unreachable(udid: &str, detail: String) -> AtalantaError {
    AtalantaError::WdaUnreachable {
        device_id: udid.to_string(),
        url: format!("usbmux://{udid}"),
        detail,
    }
}

enum ConnectError {
    /// Nothing listens on the device port.
    Refused,
    Other(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused => f.write_str("the device refused the port; is WebDriverAgent running?"),
            Self::Other(e) => f.write_str(e),
        }
    }
}

/// A socket piped to `port` on the device.
async fn connect(
    socket: &Path,
    device_id: u64,
    port: u16,
) -> std::result::Result<UnixStream, ConnectError> {
    let mut sock = open(socket).await.map_err(ConnectError::Other)?;
    let fields = [
        ("DeviceID", Field::Int(device_id)),
        // usbmuxd wants the port in network byte order, as an integer.
        ("PortNumber", Field::Int(u64::from(port.to_be()))),
    ];
    send(&mut sock, 2, &request("Connect", &fields))
        .await
        .map_err(ConnectError::Other)?;
    let reply = receive(&mut sock).await.map_err(ConnectError::Other)?;
    match integer_after(&reply, "Number") {
        Some(0) => Ok(sock),
        Some(CONNREFUSED) => Err(ConnectError::Refused),
        other => Err(ConnectError::Other(format!(
            "usbmuxd refused the connection to port {port} (result {other:?})"
        ))),
    }
}

async fn open(socket: &Path) -> std::result::Result<UnixStream, String> {
    if !socket.exists() {
        return Err(format!(
            "{} not found (usbmuxd is macOS's device daemon)",
            socket.display()
        ));
    }
    tokio::time::timeout(deadline(), UnixStream::connect(socket))
        .await
        .map_err(|_| "usbmuxd: connect timed out".to_string())?
        .map_err(|e| format!("usbmuxd: {e}"))
}

enum Field {
    Int(u64),
}

fn request(kind: &str, fields: &[(&str, Field)]) -> String {
    let mut body = format!(
        "<key>MessageType</key><string>{kind}</string>\
         <key>ClientVersionString</key><string>atalanta</string>\
         <key>ProgName</key><string>atalanta</string>"
    );
    for (k, v) in fields {
        match v {
            Field::Int(n) => body.push_str(&format!("<key>{k}</key><integer>{n}</integer>")),
        }
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>{body}</dict></plist>\n"
    )
}

async fn send(sock: &mut UnixStream, tag: u32, plist: &str) -> std::result::Result<(), String> {
    let payload = plist.as_bytes();
    let mut msg = Vec::with_capacity(16 + payload.len());
    msg.extend_from_slice(&(16 + payload.len() as u32).to_le_bytes());
    msg.extend_from_slice(&1u32.to_le_bytes()); // version
    msg.extend_from_slice(&8u32.to_le_bytes()); // plist message
    msg.extend_from_slice(&tag.to_le_bytes());
    msg.extend_from_slice(payload);
    tokio::time::timeout(deadline(), sock.write_all(&msg))
        .await
        .map_err(|_| "usbmuxd write timed out".to_string())?
        .map_err(|e| format!("usbmuxd write: {e}"))
}

async fn receive(sock: &mut UnixStream) -> std::result::Result<String, String> {
    let read = async {
        let mut header = [0u8; 16];
        sock.read_exact(&mut header).await?;
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        // A plist reply is a few KB; a bigger length is a garbled frame.
        if !(16..=1 << 20).contains(&len) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("bad frame length {len}"),
            ));
        }
        let mut body = vec![0u8; len - 16];
        sock.read_exact(&mut body).await?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    };
    tokio::time::timeout(deadline(), read)
        .await
        .map_err(|_| "usbmuxd read timed out".to_string())?
        .map_err(|e| format!("usbmuxd read: {e}"))
}

/// The integer following `<key>{key}</key>`.
fn integer_after(plist: &str, key: &str) -> Option<u64> {
    let tag = format!("<key>{key}</key>");
    let rest = &plist[plist.find(&tag)? + tag.len()..];
    let rest = rest.trim_start().strip_prefix("<integer>")?;
    rest[..rest.find("</integer>")?].trim().parse().ok()
}

/// The `DeviceID` of the entry whose `SerialNumber` is `udid` (compared
/// without dashes or case: usbmuxd and Xcode format newer UDIDs differently),
/// and whether that entry is the cable. A device that is both cabled and
/// Wi-Fi-paired is listed twice: the USB entry wins wherever it is in the
/// list (N5; pymobiledevice3 does the same), so the relay never goes over
/// Wi-Fi when a cable is there.
fn find_device_id(list: &str, udid: &str) -> Option<(u64, bool)> {
    let norm = |s: &str| s.replace('-', "").to_ascii_uppercase();
    let want = norm(udid);
    // Each device is a <dict> holding DeviceID and a flat Properties dict
    // (SerialNumber, ConnectionType "USB"/"Network", usually DeviceID again).
    let mut found: Option<(u64, bool)> = None;
    let mut prev_end = 0;
    let tag = "<key>Properties</key>";
    while let Some(at) = list[prev_end..].find(tag).map(|i| i + prev_end) {
        let body = &list[at + tag.len()..];
        let props = &body[..body.find("</dict>").unwrap_or(body.len())];
        let end = at + tag.len() + props.len();
        let outer = &list[prev_end..at];
        let id = integer_after(props, "DeviceID")
            .or_else(|| integer_after(&outer[outer.rfind("<key>DeviceID</key>")?..], "DeviceID"));
        prev_end = end;
        let (Some(id), Some(serial)) = (id, string_after(props, "SerialNumber")) else {
            continue;
        };
        if norm(serial) != want {
            continue;
        }
        if string_after(props, "ConnectionType") == Some("USB") {
            return Some((id, true));
        }
        found = found.or(Some((id, false)));
    }
    found
}

/// The `<string>` following `<key>{key}</key>` in `chunk`.
fn string_after<'a>(chunk: &'a str, key: &str) -> Option<&'a str> {
    let tag = format!("<key>{key}</key>");
    let rest = chunk[chunk.find(&tag)? + tag.len()..].trim_start();
    let rest = rest.strip_prefix("<string>")?;
    rest.split("</string>").next()
}

/// Whether usbmuxd lists `udid` on a cable (best effort: any failure is
/// "no"). Decides whether the runner on the device may bind loopback.
pub async fn connected_by_usb(udid: &str) -> bool {
    let socket = socket_path();
    let reply = async {
        let mut sock = open(&socket).await?;
        send(&mut sock, 1, &request("ListDevices", &[])).await?;
        receive(&mut sock).await
    }
    .await;
    matches!(
        reply.ok().and_then(|r| find_device_id(&r, udid)),
        Some((_, true))
    )
}

/// How usbmuxd lists `udid`: `Some(true)` on a cable, `Some(false)` over
/// Wi-Fi only, `None` when it isn't listed or usbmuxd can't be asked (then
/// the forward fails with its own error).
pub async fn connection(udid: &str) -> Option<bool> {
    let socket = socket_path();
    let reply = async {
        let mut sock = open(&socket).await?;
        send(&mut sock, 1, &request("ListDevices", &[])).await?;
        receive(&mut sock).await
    }
    .await;
    reply
        .ok()
        .and_then(|r| find_device_id(&r, udid))
        .map(|(_, usb)| usb)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"<plist version="1.0"><dict><key>DeviceList</key><array>
<dict><key>DeviceID</key><integer>3</integer><key>MessageType</key><string>Attached</string>
<key>Properties</key><dict><key>ConnectionType</key><string>USB</string><key>DeviceID</key><integer>3</integer>
<key>SerialNumber</key><string>00008130001451402EC1001C</string></dict></dict>
<dict><key>DeviceID</key><integer>7</integer><key>Properties</key><dict>
<key>SerialNumber</key><string>abcdef0123456789abcdef0123456789abcdef01</string></dict></dict>
</array></dict></plist>"#;

    #[test]
    fn finds_the_device_by_udid_in_either_format() {
        assert_eq!(
            find_device_id(LIST, "00008130-001451402EC1001C"),
            Some((3, true))
        );
        assert_eq!(
            find_device_id(LIST, "ABCDEF0123456789ABCDEF0123456789ABCDEF01"),
            Some((7, false))
        );
        assert_eq!(find_device_id(LIST, "nope"), None);
    }

    #[test]
    fn a_device_listed_over_wifi_and_usb_resolves_to_the_usb_entry() {
        // N5: the first serial match won, so a Wi-Fi entry listed first
        // relayed every request over the network.
        let both = r#"<plist version="1.0"><dict><key>DeviceList</key><array>
<dict><key>DeviceID</key><integer>9</integer><key>Properties</key><dict>
<key>ConnectionType</key><string>Network</string><key>DeviceID</key><integer>9</integer>
<key>SerialNumber</key><string>00008130001451402EC1001C</string></dict></dict>
<dict><key>DeviceID</key><integer>4</integer><key>Properties</key><dict>
<key>ConnectionType</key><string>USB</string><key>DeviceID</key><integer>4</integer>
<key>SerialNumber</key><string>00008130001451402EC1001C</string></dict></dict>
</array></dict></plist>"#;
        assert_eq!(
            find_device_id(both, "00008130-001451402EC1001C"),
            Some((4, true))
        );
        let wifi_only = both
            .split("<dict><key>DeviceID</key><integer>4")
            .next()
            .unwrap();
        assert_eq!(
            find_device_id(
                &format!("{wifi_only}</array></dict></plist>"),
                "00008130001451402EC1001C"
            ),
            Some((9, false))
        );
    }

    /// How the fake usbmuxd answers `Connect`.
    #[derive(Clone, Copy)]
    enum Device {
        /// Never answers anything.
        Silent,
        /// `Number` 3: nothing listens on the device port.
        Refused,
        /// Connects; the device port answers HTTP.
        Http,
        /// Connects; the device port reads and never answers.
        Mute,
        /// Lists the phone under [`CURRENT_ID`] and connects only to that
        /// number (BadDevice otherwise), the port answering HTTP: a test
        /// "replugs" by changing it.
        Replugged,
    }

    /// The fake's number for the phone in [`Device::Replugged`].
    static CURRENT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(3);

    /// A fake usbmuxd on a Unix socket in a fresh temp dir. Each accepted
    /// pipe is reported on the channel, so a test can see it close.
    fn fake_usbmuxd(
        device: Device,
    ) -> (
        PathBuf,
        tokio::sync::mpsc::UnboundedReceiver<tokio::task::JoinHandle<()>>,
    ) {
        // Unix socket paths are short (104 bytes): not under $TMPDIR.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = PathBuf::from(format!("/tmp/vmux-{}-{n}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if matches!(device, Device::Silent) {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        return;
                    }
                    let Ok(msg) = receive(&mut s).await else {
                        return;
                    };
                    let current = CURRENT_ID.load(std::sync::atomic::Ordering::SeqCst);
                    if msg.contains("ListDevices") {
                        let list = if matches!(device, Device::Replugged) {
                            LIST.replace(
                                "<integer>3</integer>",
                                &format!("<integer>{current}</integer>"),
                            )
                        } else {
                            LIST.to_string()
                        };
                        let _ = send(&mut s, 1, &list).await;
                        return;
                    }
                    let asked = integer_after(&msg, "DeviceID");
                    let number = match device {
                        Device::Refused => 3,
                        // usbmuxd's BadDevice.
                        Device::Replugged if asked != Some(current) => 2,
                        _ => 0,
                    };
                    let reply = format!(
                        "<plist><dict><key>Number</key><integer>{number}</integer></dict></plist>"
                    );
                    let _ = send(&mut s, 2, &reply).await;
                    if number != 0 {
                        return;
                    }
                    let pipe = tokio::spawn(async move {
                        let mut buf = [0u8; 1024];
                        loop {
                            match s.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(_) if matches!(device, Device::Http | Device::Replugged) => {
                                    let _ = s
                                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nI-AM-ALIVE")
                                        .await;
                                }
                                Ok(_) => {}
                            }
                        }
                    });
                    let _ = tx.send(pipe);
                });
            }
        });
        (path, rx)
    }

    /// The tests' deadline. Not through the process env: `set_var` while
    /// other test threads read the env is a data race on macOS.
    pub(super) static DEADLINE_MS: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    fn short_deadlines() {
        DEADLINE_MS.store(300, std::sync::atomic::Ordering::Relaxed);
    }

    const UDID: &str = "00008130-001451402EC1001C";

    #[tokio::test]
    async fn a_silent_usbmuxd_times_out() {
        // VEL-150: reads had no deadline; a silent usbmuxd hung the run.
        short_deadlines();
        let (sock, _) = fake_usbmuxd(Device::Silent);
        let started = std::time::Instant::now();
        let e = forward_via(&sock, UDID, 0, 8100).await.err().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        // A device-side failure, not a config error (VEL-291).
        assert!(matches!(e, AtalantaError::WdaUnreachable { .. }), "{e:?}");
    }

    #[tokio::test]
    async fn a_refused_device_port_is_dead_and_reads_reach_recovery() {
        // VEL-025: the forward accepted locally, so a dead WDA looked alive.
        short_deadlines();
        let (sock, _) = fake_usbmuxd(Device::Refused);
        let f = forward_via(&sock, UDID, 0, 8100).await.unwrap();
        assert_eq!(f.probe().await, Liveness::Dead);
        let mut c = crate::wda::WdaClient::with_timeout(
            &format!("http://127.0.0.1:{}", f.local_port),
            Duration::from_secs(2),
        );
        c.set_session_id("s".into());
        let e = c.get_source().await.unwrap_err();
        assert!(matches!(e, AtalantaError::WdaUnreachable { .. }), "{e:?}");
    }

    // Row 1.6: usbmuxd numbers a replugged phone anew; the forward looks
    // the UDID up again instead of failing every connection.
    #[tokio::test]
    async fn a_replugged_phone_is_found_under_its_new_number() {
        short_deadlines();
        CURRENT_ID.store(3, std::sync::atomic::Ordering::SeqCst);
        let (sock, _) = fake_usbmuxd(Device::Replugged);
        let f = forward_via(&sock, UDID, 0, 8100).await.unwrap();
        assert_eq!(f.probe().await, Liveness::Alive);
        CURRENT_ID.store(11, std::sync::atomic::Ordering::SeqCst);
        // Through the local port, as WDA requests go.
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", f.local_port))
            .await
            .unwrap();
        c.write_all(b"GET /status HTTP/1.1\r\n\r\n").await.unwrap();
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(2), c.read(&mut buf))
            .await
            .expect("the relay answered")
            .unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).contains("200 OK"));
        assert_eq!(f.device_id.load(std::sync::atomic::Ordering::Relaxed), 11);
        assert_eq!(f.probe().await, Liveness::Alive);
    }

    #[tokio::test]
    async fn a_device_port_that_answers_is_alive() {
        short_deadlines();
        let (sock, _) = fake_usbmuxd(Device::Http);
        let f = forward_via(&sock, UDID, 0, 8100).await.unwrap();
        assert_eq!(f.probe().await, Liveness::Alive);
    }

    #[tokio::test]
    async fn dropping_the_forward_stops_its_relays() {
        // VEL-292: relays outlived the forward.
        short_deadlines();
        let (sock, mut pipes) = fake_usbmuxd(Device::Mute);
        let f = forward_via(&sock, UDID, 0, 8100).await.unwrap();
        let mut local = tokio::net::TcpStream::connect(("127.0.0.1", f.local_port))
            .await
            .unwrap();
        local.write_all(b"hello").await.unwrap();
        let pipe = tokio::time::timeout(Duration::from_secs(2), pipes.recv())
            .await
            .unwrap()
            .unwrap();
        drop(f);
        // The device side of the relay sees EOF once the relay is gone.
        tokio::time::timeout(Duration::from_secs(2), pipe)
            .await
            .expect("relay still running after the forward was dropped")
            .unwrap();
    }

    #[tokio::test]
    async fn a_garbled_frame_length_is_an_error() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let mut frame = Vec::new();
        frame.extend_from_slice(&u32::MAX.to_le_bytes());
        frame.extend_from_slice(&[0u8; 12]);
        a.write_all(&frame).await.unwrap();
        let e = receive(&mut b).await.unwrap_err();
        assert!(e.contains("bad frame length"), "{e}");
    }

    #[test]
    fn connect_request_carries_the_port_in_network_order() {
        let r = request(
            "Connect",
            &[("PortNumber", Field::Int(u64::from(8100u16.to_be())))],
        );
        // 8100 = 0x1FA4 → network order 0xA41F = 42015 on little-endian hosts.
        assert!(
            r.contains("<key>PortNumber</key><integer>42015</integer>"),
            "{r}"
        );
        assert!(r.contains("<string>Connect</string>"));
        assert_eq!(
            integer_after("<key>Number</key><integer>0</integer>", "Number"),
            Some(0)
        );
    }
}

// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

import Foundation

// Host (macOS) checks for the AtalantaSyncProbe logic that doesn't need
// UIKit: timers, the idle hold, the URL blocklist, port resolution and the
// server's protocol over a real loopback socket. Run with ./run.sh.
var failures = 0
func check(_ ok: Bool, _ name: String) {
    print("\(ok ? "ok  " : "FAIL") \(name)")
    if !ok { failures += 1 }
}

check(timerRegression(), "P3: an invalidated, never-firing timer no longer keeps the app busy")

let state = AppStateTracker.shared
func spin(_ s: TimeInterval) { RunLoop.current.run(until: Date().addingTimeInterval(s)) }

// A long timer is not pending work.
let long = Timer.scheduledTimer(withTimeInterval: 30, repeats: false) { _ in }
check(TimerTracker.pendingCount == 0 && state.isIdle, "P3: a 30 s timer is not tracked")
long.invalidate()

// A short timer is busy until it fires.
var fired = false
_ = Timer.scheduledTimer(withTimeInterval: 0.2, repeats: false) { _ in fired = true }
check(TimerTracker.pendingCount == 1 && !state.isIdle, "P3: a 0.2 s timer is busy")
check(state.pendingReasons.contains("timer:1"), "P3: pending names timer:1")
spin(0.4)
check(fired && TimerTracker.pendingCount == 0 && state.isIdle, "P3: idle once the timer fired")

// Repeating timers are never tracked.
let rep = Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { _ in }
check(TimerTracker.pendingCount == 0, "P3: a repeating timer is not tracked")
rep.invalidate()

// The target/selector API is covered too.
final class Target: NSObject { var hits = 0; @objc func tick(_ t: Timer) { hits += 1 } }
let target = Target()
let sel = Timer.scheduledTimer(timeInterval: 0.2, target: target, selector: #selector(Target.tick(_:)), userInfo: nil, repeats: false)
check(TimerTracker.pendingCount == 1, "P3: target/selector timer is busy")
sel.invalidate()
check(TimerTracker.pendingCount == 0 && state.isIdle, "P3: target/selector timer idle after invalidate")
_ = Timer.scheduledTimer(timeInterval: 0.1, target: target, selector: #selector(Target.tick(_:)), userInfo: nil, repeats: false)
spin(0.3)
check(target.hits == 1 && TimerTracker.pendingCount == 0, "P3: target/selector timer still fires and then reads idle")

// A shorter max interval drops a timer from the count.
TimerTracker.maxTrackableInterval = 0.05
let medium = Timer.scheduledTimer(withTimeInterval: 0.2, repeats: false) { _ in }
check(TimerTracker.pendingCount == 0, "P3: max trackable interval is configurable")
medium.invalidate()
TimerTracker.maxTrackableInterval = 1.5

// m5: an unmatched decrement can't hide later work.
do {
    state.decrementNetwork()
    state.decrementNetwork()
    state.incrementNetwork()
    check(!state.isIdle && state.pendingReasons.contains("network:1"),
          "m5: unmatched decrements are floored at 0, a later request is busy")
    state.decrementNetwork()
    check(state.isIdle, "m5: idle once that request finished")
    state.decrementTimer()
    state.incrementTimer()
    check(!state.isIdle, "m5: same for hand-marked timers")
    state.decrementTimer()
}

// P5: idle must hold.
do {
    let t0 = Date()
    let r = IdleWait.wait(timeoutMs: 1000, quietMs: nil) { true }
    let ms = Date().timeIntervalSince(t0) * 1000
    check(r.idle && ms >= 30, "P5: steady idle answers only after 3 samples (\(Int(ms)) ms)")
}
do {
    // Idle gaps of 25 ms between 25 ms of work: the old probe answered
    // in the first gap; idle never holds for 3 samples 16 ms apart.
    let t0 = Date()
    let r = IdleWait.wait(timeoutMs: 400, quietMs: nil) {
        Int(Date().timeIntervalSince(t0) * 1000 / 25) % 2 == 0
    }
    check(!r.idle, "P5: idle in 25 ms gaps between work is not idle")
}
do {
    // Busy for 100 ms, then idle: answers after it has held.
    let t0 = Date()
    let r = IdleWait.wait(timeoutMs: 1000, quietMs: 50) { Date().timeIntervalSince(t0) > 0.1 }
    check(r.idle && r.waitedMs >= 150, "P5: quiet_ms is honoured (\(r.waitedMs) ms)")
}

// P6: the URL blocklist.
let bl = URLBlocklist()
check(bl.isIgnored("wss://chat.example.com/live"), "P6: wss is ignored")
check(bl.isIgnored("https://x.example.com/socket.io/?EIO=4&transport=polling"), "P6: socket.io long-poll is ignored")
check(bl.isIgnored("https://x.example.com/api/longpoll?since=3"), "P6: longpoll is ignored")
check(!bl.isIgnored("https://x.example.com/api/items"), "P6: an ordinary request counts")
check(bl.add(["/analytics/", "("]) == ["("], "P6: invalid patterns are reported")
check(bl.isIgnored("https://x.example.com/analytics/beacon"), "P6: added pattern applies")
bl.set([])
check(!bl.isIgnored("wss://chat.example.com/live"), "P6: replace drops the defaults")

if #available(macOS 10.15, *) {
    let ws = URLSession.shared.webSocketTask(with: URL(string: "ws://127.0.0.1:9/")!)
    check(NetworkTracker.isIgnored(ws), "P6: a URLSessionWebSocketTask is ignored")
}
let plain = URLSession.shared.dataTask(with: URL(string: "https://example.com/api")!)
check(!NetworkTracker.isIgnored(plain), "P6: a data task counts")
let poll = URLSession.shared.dataTask(with: URL(string: "https://example.com/socket.io/?transport=polling")!)
check(NetworkTracker.isIgnored(poll), "P6: a long-poll data task is ignored")

// P2: port resolution.
check(SyncServer.resolvePort(explicit: nil, environment: [:]) == 19400, "P2: default port")
check(SyncServer.resolvePort(explicit: nil, environment: ["ATALANTA_SYNC_PORT": "41000"]) == 41000, "P2: ATALANTA_SYNC_PORT")
check(SyncServer.resolvePort(explicit: 1234, environment: ["ATALANTA_SYNC_PORT": "41000"]) == 1234, "P2: explicit port wins")
check(SyncServer.resolvePort(explicit: nil, environment: ["ATALANTA_SYNC_PORT": "nope"]) == 19400, "P2: bad env falls back")

// P2: the server answers hello over loopback.
func freePort() -> UInt16 {
    let fd = socket(AF_INET, SOCK_STREAM, 0)
    var addr = sockaddr_in()
    addr.sin_family = sa_family_t(AF_INET)
    addr.sin_addr.s_addr = inet_addr("127.0.0.1")
    addr.sin_port = 0
    var len = socklen_t(MemoryLayout<sockaddr_in>.size)
    withUnsafeMutablePointer(to: &addr) { p in
        p.withMemoryRebound(to: sockaddr.self, capacity: 1) { _ = bind(fd, $0, len) }
    }
    withUnsafeMutablePointer(to: &addr) { p in
        p.withMemoryRebound(to: sockaddr.self, capacity: 1) { _ = getsockname(fd, $0, &len) }
    }
    close(fd)
    return UInt16(bigEndian: addr.sin_port)
}

/// Send `lines` on one connection; the reply lines, in order.
func askAll(_ port: UInt16, _ lines: [String]) -> [String] {
    let fd = socket(AF_INET, SOCK_STREAM, 0)
    defer { close(fd) }
    var tv = timeval(tv_sec: 5, tv_usec: 0)
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
    var addr = sockaddr_in()
    addr.sin_family = sa_family_t(AF_INET)
    addr.sin_addr.s_addr = inet_addr("127.0.0.1")
    addr.sin_port = port.bigEndian
    let rc = withUnsafePointer(to: &addr) { p in
        p.withMemoryRebound(to: sockaddr.self, capacity: 1) {
            connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
        }
    }
    guard rc == 0 else { return [] }
    var replies = [String]()
    var buf = [UInt8](repeating: 0, count: 4096)
    var pending = [UInt8]()
    for line in lines {
        let bytes = Array((line + "\n").utf8)
        _ = bytes.withUnsafeBytes { write(fd, $0.baseAddress, bytes.count) }
        while !pending.contains(UInt8(ascii: "\n")) {
            let n = read(fd, &buf, buf.count)
            if n <= 0 { return replies }
            pending.append(contentsOf: buf[0..<n])
        }
        let nl = pending.firstIndex(of: UInt8(ascii: "\n"))!
        replies.append(String(decoding: pending[..<nl], as: UTF8.self))
        pending.removeSubrange(...nl)
    }
    return replies
}

func ask(_ port: UInt16, _ line: String, host: String = "127.0.0.1") -> String? {
    let fd = socket(AF_INET, SOCK_STREAM, 0)
    defer { close(fd) }
    var tv = timeval(tv_sec: 5, tv_usec: 0)
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
    var addr = sockaddr_in()
    addr.sin_family = sa_family_t(AF_INET)
    addr.sin_addr.s_addr = inet_addr(host)
    addr.sin_port = port.bigEndian
    let rc = withUnsafePointer(to: &addr) { p in
        p.withMemoryRebound(to: sockaddr.self, capacity: 1) {
            connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
        }
    }
    guard rc == 0 else { return nil }
    let bytes = Array((line + "\n").utf8)
    _ = bytes.withUnsafeBytes { write(fd, $0.baseAddress, bytes.count) }
    var out = [UInt8]()
    var buf = [UInt8](repeating: 0, count: 4096)
    while !out.contains(UInt8(ascii: "\n")) {
        let n = read(fd, &buf, buf.count)
        if n <= 0 { break }
        out.append(contentsOf: buf[0..<n])
    }
    return String(decoding: out, as: UTF8.self)
}

let port = freePort()
let server = SyncServer(stateTracker: state, port: port)
server.start()
var hello: String?
for _ in 0..<50 {
    hello = ask(port, #"{"cmd":"hello"}"#)
    if hello != nil { break }
    usleep(20_000)
}
let pid = ProcessInfo.processInfo.processIdentifier
check(hello?.contains("\"hello\":true") == true && hello?.contains("\"pid\":\(pid)") == true
      && hello?.contains("\"version\":4") == true, "P2: hello returns bundle + pid (\(hello ?? "nil"))")
let idle = ask(port, #"{"cmd":"wait_idle","timeout_ms":500,"quiet_ms":20}"#)
check(idle?.contains("\"idle\":true") == true, "P5: wait_idle over the wire (\(idle ?? "nil"))")
let cfg = ask(port, #"{"cmd":"config","ignore_urls":["/beacon/"]}"#)
check(cfg?.contains("\"ok\":true") == true && URLBlocklist.shared.isIgnored("https://a/beacon/1"),
      "P6: config adds ignore_urls (\(cfg ?? "nil"))")
// probe-urls: the host sends its list on every connection; it is not
// added twice.
let before = URLBlocklist.shared.count
_ = ask(port, #"{"cmd":"config","ignore_urls":["/beacon/"]}"#)
check(URLBlocklist.shared.count == before, "probe-urls: a pattern already on the list is not added again")

// probe-urls: max_timer_ms holds for the connection that sent it only.
do {
    let t = Timer.scheduledTimer(withTimeInterval: 0.8, repeats: false) { _ in }
    let short = askAll(port, [#"{"cmd":"config","max_timer_ms":100}"#, #"{"cmd":"status"}"#])
    check(short.count == 2 && short[1].contains("\"idle\":true"),
          "probe-urls: max_timer_ms 100 ignores a 0.8 s timer on that connection (\(short))")
    let other = ask(port, #"{"cmd":"status"}"#)
    check(other?.contains("timer:1") == true,
          "probe-urls: another connection keeps the 1.5 s default (\(other ?? "nil"))")
    check(TimerTracker.maxTrackableInterval == 1.5, "probe-urls: the global horizon is unchanged")
    t.invalidate()
    let long = Timer.scheduledTimer(withTimeInterval: 3, repeats: false) { _ in }
    let wide = askAll(port, [#"{"cmd":"config","max_timer_ms":5000}"#,
                             #"{"cmd":"wait_idle","timeout_ms":100}"#])
    check(wide.count == 2 && wide[1].contains("timer:1"),
          "probe-urls: max_timer_ms 5000 waits for a 3 s timer (\(wide))")
    check(ask(port, #"{"cmd":"status"}"#)?.contains("\"idle\":true") == true,
          "probe-urls: the 3 s timer is not busy at the default horizon")
    long.invalidate()
}

hookChecks(port: port)
perpetualChecks(port: port)

let old = ask(port, #"{"cmd":"bogus"}"#)
check(old?.contains("unknown command") == true, "unknown command is an error reply")

// Loopback only: the same port on a non-loopback address must refuse.
func lanAddress() -> String? {
    var ifaddr: UnsafeMutablePointer<ifaddrs>?
    guard getifaddrs(&ifaddr) == 0, let first = ifaddr else { return nil }
    defer { freeifaddrs(ifaddr) }
    var p: UnsafeMutablePointer<ifaddrs>? = first
    while let cur = p {
        if let sa = cur.pointee.ifa_addr, sa.pointee.sa_family == sa_family_t(AF_INET),
           (cur.pointee.ifa_flags & UInt32(IFF_LOOPBACK)) == 0 {
            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            getnameinfo(sa, socklen_t(sa.pointee.sa_len), &host, socklen_t(host.count), nil, 0, NI_NUMERICHOST)
            return String(cString: host)
        }
        p = cur.pointee.ifa_next
    }
    return nil
}
if let lan = lanAddress() {
    check(ask(port, #"{"cmd":"hello"}"#, host: lan) == nil, "P2/NB-11: not reachable on \(lan)")
} else {
    print("skip loopback-only check: no non-loopback IPv4 interface")
}
server.stop()

print(failures == 0 ? "all host checks passed" : "\(failures) host check(s) FAILED")
exit(failures == 0 ? 0 : 1)

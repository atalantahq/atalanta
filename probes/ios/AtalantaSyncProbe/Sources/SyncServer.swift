import Foundation
import Network

/// TCP server that exposes app idle state to the Atalanta test runner.
/// Protocol: newline-delimited JSON over TCP, bound to the loopback
/// interface only (a physical device on Wi-Fi exposed it to the LAN).
///
/// Commands:
///   {"cmd": "hello"}
///   {"cmd": "status"}
///   {"cmd": "wait_idle", "timeout_ms": 5000, "quiet_ms": 50}
///   {"cmd": "config", "ignore_urls": ["regex", ...], "replace": false, "max_timer_ms": 1500}
///   {"cmd": "config", "framework": "react_native"}
///   {"cmd": "config", "ignore_trackers": ["rn_timers"]}  (process-wide, replaces the last list)
///
/// `ignore_urls` changes the process-wide list (a pattern already there is
/// not added twice); `max_timer_ms` holds for the connection that sent it
/// (each host connection is one run), so two runs never see each other's.
/// `framework` turns on that framework's trackers for the process; the
/// reply names it (null: this probe has none for it) and what can't tell.
///
/// Responses:
///   {"hello": true, "bundle": "com.example", "pid": 123, "version": 3, "platform": "ios"}
///   {"idle": true, "pending": [], "ts": 1710000000}
///   {"idle": false, "pending": ["animation:2", "network:1"], "waited_ms": 5000}
///   {"ok": true, "invalid": []}
///   {"ok": true, "invalid": [], "framework": "react_native", "unknown": []}
@available(iOS 12.0, macOS 10.14, *)
final class SyncServer {
    /// Settings one host connection sent with `config`.
    final class Session {
        /// Timer horizon for this connection; nil is the global one.
        var maxTimerInterval: TimeInterval?
    }

    /// Protocol version reported by `hello`. 1 was the protocol without it;
    /// 3 takes `max_timer_ms` in `config` for one connection only (the host
    /// sends it to 3 and later only); 4 takes `framework` in `config`
    /// (React Native tracking) and tracks `performSelector:afterDelay:`.
    static let protocolVersion = 4
    static let defaultPort: UInt16 = 19400

    private let stateTracker: AppStateTracker
    private let port: UInt16
    private var listener: NWListener?

    init(stateTracker: AppStateTracker, port: UInt16) {
        self.stateTracker = stateTracker
        self.port = port
    }

    /// The port to listen on: an explicit one, else `ATALANTA_SYNC_PORT`
    /// (the host passes a per-device port through
    /// `SIMCTL_CHILD_ATALANTA_SYNC_PORT` / the launch environment, so
    /// parallel simulators sharing the Mac's loopback don't collide), else
    /// 19400.
    static func resolvePort(explicit: UInt16?, environment: [String: String]) -> UInt16 {
        if let explicit { return explicit }
        if let raw = environment["ATALANTA_SYNC_PORT"]?.trimmingCharacters(in: .whitespaces),
           let port = UInt16(raw), port != 0 {
            return port
        }
        return defaultPort
    }

    func start() {
        guard let nwPort = NWEndpoint.Port(rawValue: port) else { return }

        let params = NWParameters.tcp
        params.allowLocalEndpointReuse = true
        params.requiredLocalEndpoint = .hostPort(host: .ipv4(.loopback), port: nwPort)

        do {
            listener = try NWListener(using: params)
        } catch {
            print("[AtalantaSyncProbe] Failed to create listener: \(error)")
            return
        }

        listener?.newConnectionHandler = { [weak self] connection in
            self?.handleConnection(connection)
        }

        listener?.stateUpdateHandler = { [port] state in
            switch state {
            case .ready:
                print("[AtalantaSyncProbe] Sync server listening on port \(port)")
            case .failed(let error):
                print("[AtalantaSyncProbe] Listener failed: \(error)")
            default:
                break
            }
        }

        listener?.start(queue: DispatchQueue(label: "com.atalanta.sync-server"))
    }

    func stop() {
        listener?.cancel()
        listener = nil
    }

    private func handleConnection(_ connection: NWConnection) {
        connection.start(queue: DispatchQueue(label: "com.atalanta.sync-client"))
        readLine(from: connection, buffer: Data(), session: Session())
    }

    private func readLine(from connection: NWConnection, buffer: Data, session: Session) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 4096) { [weak self] data, _, isComplete, error in
            guard let self, error == nil else {
                connection.cancel()
                return
            }

            // TCP gives no message boundaries: a receive() may deliver a
            // fragment of a request or several coalesced ones. Buffer until
            // newline and answer every complete line.
            var buffer = buffer
            if let data { buffer.append(data) }

            var responses = [String]()
            while let nl = buffer.firstIndex(of: UInt8(ascii: "\n")) {
                let lineData = buffer.subdata(in: buffer.startIndex..<nl)
                buffer.removeSubrange(buffer.startIndex...nl)
                if let line = String(data: lineData, encoding: .utf8)?
                    .trimmingCharacters(in: .whitespacesAndNewlines),
                    !line.isEmpty {
                    responses.append(self.processCommand(line, session: session))
                }
            }

            if responses.isEmpty {
                if isComplete {
                    connection.cancel()
                } else {
                    self.readLine(from: connection, buffer: buffer, session: session)
                }
                return
            }

            let responseData = (responses.joined(separator: "\n") + "\n").data(using: .utf8) ?? Data()
            connection.send(content: responseData, completion: .contentProcessed { _ in
                if isComplete {
                    connection.cancel()
                } else {
                    self.readLine(from: connection, buffer: buffer, session: session)
                }
            })
        }
    }

    func processCommand(_ json: String, session: Session = Session()) -> String {
        guard let data = json.data(using: .utf8),
              let command = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let cmd = command["cmd"] as? String
        else {
            return """
            {"error": "invalid command"}
            """
        }

        switch cmd {
        case "status":
            return statusResponse(session)

        case "hello":
            return helloResponse()

        case "wait_idle":
            let timeoutMs = command["timeout_ms"] as? Int ?? 5000
            let quietMs = command["quiet_ms"] as? Int
            return waitIdleResponse(timeoutMs: timeoutMs, quietMs: quietMs, session: session)

        case "config":
            return configResponse(command, session: session)

        default:
            return Self.json(["error": "unknown command: \(cmd)"])
        }
    }

    private static func json(_ object: [String: Any]) -> String {
        guard let data = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]),
              let text = String(data: data, encoding: .utf8)
        else { return "{\"error\":\"encoding failed\"}" }
        return text
    }

    /// Who this probe is: the host checks the bundle (and logs the pid) so
    /// it never syncs on another app's or another simulator's probe.
    private func helloResponse() -> String {
        let info = ProcessInfo.processInfo
        return Self.json([
            "hello": true,
            "bundle": Bundle.main.bundleIdentifier ?? info.processName,
            "pid": Int(info.processIdentifier),
            "version": Self.protocolVersion,
            "platform": "ios",
            "unknown": stateTracker.unknownTrackers,
        ])
    }

    /// `ignore_urls` adds URL patterns (regular expressions) whose requests
    /// never hold the app busy; `replace: true` drops the defaults first.
    /// `max_timer_ms` is this connection's timer horizon; it used to set
    /// the process-wide one, so the last run to connect won.
    private func configResponse(_ command: [String: Any], session: Session) -> String {
        var invalid: [String] = []
        if let patterns = command["ignore_urls"] as? [String] {
            if command["replace"] as? Bool == true {
                invalid = URLBlocklist.shared.set(patterns)
            } else {
                invalid = URLBlocklist.shared.add(patterns)
            }
        }
        if let maxTimer = command["max_timer_ms"] as? Int {
            session.maxTimerInterval = TimeInterval(max(0, maxTimer)) / 1000
        }
        var reply: [String: Any] = ["ok": true, "invalid": invalid]
        if let names = command["ignore_trackers"] as? [String] {
            reply["unknown_trackers"] = stateTracker.optOut(names)
        }
        if let framework = command["framework"] as? String {
            reply["framework"] = stateTracker.enableFramework(framework) ? framework : NSNull()
            reply["unknown"] = stateTracker.unknownTrackers
        }
        return Self.json(reply)
    }

    private func statusResponse(_ session: Session) -> String {
        let idle = stateTracker.isIdle(maxTimerInterval: session.maxTimerInterval)
        let pending = stateTracker.pendingReasons(maxTimerInterval: session.maxTimerInterval)
        let ts = Int(Date().timeIntervalSince1970)
        let pendingJson = pending.map { "\"\($0)\"" }.joined(separator: ",")
        return """
        {"idle":\(idle),"pending":[\(pendingJson)],"ts":\(ts)}
        """
    }

    private func waitIdleResponse(timeoutMs: Int, quietMs: Int?, session: Session) -> String {
        let horizon = session.maxTimerInterval
        let outcome = IdleWait.wait(timeoutMs: timeoutMs, quietMs: quietMs) {
            stateTracker.isIdle(maxTimerInterval: horizon)
        }
        // Idle replies still say what perpetual work was ignored.
        let pending = outcome.idle ? stateTracker.ignoredNotes : stateTracker.pendingReasons(maxTimerInterval: horizon)
        return Self.json([
            "idle": outcome.idle,
            "pending": pending,
            "waited_ms": outcome.waitedMs,
        ])
    }
}

// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

import Foundation

/// AtalantaSyncProbe: Embedded in debug app builds to provide native idle detection.
///
/// Usage:
///   In your AppDelegate or @main:
///     #if DEBUG
///     AtalantaSyncProbe.start()
///     #endif
///
/// The probe tracks RunLoop, animations, network requests, and timers to determine
/// when the app is idle. A TCP server on the loopback interface (port 19400, or
/// `ATALANTA_SYNC_PORT` from the launch environment) exposes this state to
/// the Atalanta test runner, which checks the probe's `hello` (bundle id and
/// pid) before trusting it.
public final class AtalantaSyncProbe {
    public static let shared = AtalantaSyncProbe()

    private let stateTracker = AppStateTracker.shared
    private var server: SyncServer?
    private var runLoopObserver: RunLoopObserver?
    private var animationTracker: AnimationTracker?
    private var networkTracker: NetworkTracker?
    private var timerTracker: TimerTracker?

    private init() {}

    /// Start the probe. `port` defaults to `ATALANTA_SYNC_PORT` from the
    /// environment, else 19400.
    public static func start(port: UInt16? = nil) {
        let resolved = SyncServer.resolvePort(
            explicit: port, environment: ProcessInfo.processInfo.environment)
        shared.startInternal(port: resolved)
    }

    /// Stop the probe and close the server.
    public static func stop() {
        shared.stopInternal()
    }

    /// Whether the app is currently idle (all trackers report no pending work).
    public static var isIdle: Bool {
        shared.stateTracker.isIdle
    }

    /// Requests to URLs matching these regular expressions never hold the app
    /// busy. Added to the defaults (WebSocket, socket.io, SockJS, SignalR,
    /// long-poll URLs; see `URLBlocklist.defaultPatterns`). Returns the
    /// patterns that are not valid regular expressions.
    @discardableResult
    public static func ignoreURLs(_ patterns: [String]) -> [String] {
        URLBlocklist.shared.add(patterns)
    }

    /// One-shot timers due within this many seconds count as pending work
    /// (default 1.5 s, as EarlGrey and Detox).
    public static var maxTrackedTimerInterval: TimeInterval {
        get { TimerTracker.maxTrackableInterval }
        set { TimerTracker.maxTrackableInterval = newValue }
    }

    /// Track React Native's JS timers and Fabric transactions now (see
    /// `ReactNativeTracker`). The host turns this on itself for a suite with
    /// `framework: react_native`; this is for apps that want it regardless.
    /// An app that knows when other RN work is busy can still report it with
    /// `AppStateTracker.shared.markRNBridgeBusy(_:)`.
    public static func enableRN() {
        _ = AppStateTracker.shared.enableFramework("react_native")
    }

    /// Trackers that never count (`runloop`, `animation`, `network`,
    /// `timer`, `rn_bridge`, `rn_timers`, `rn_mounting`): the app-side form
    /// of `config` `ignore_trackers`. Replaces the last list; returns the
    /// names the probe doesn't have.
    @discardableResult
    public static func ignoreTrackers(_ names: [String]) -> [String] {
        AppStateTracker.shared.optOut(names)
    }

    /// `queue.asyncAfter(deadline:execute:)` that counts as a pending timer
    /// until `work` ran (when due within the timer horizon). Plain
    /// `dispatch_after` / `asyncAfter` can't be observed with public API;
    /// see `DelayedWorkTracker`.
    public static func asyncAfter(deadline: DispatchTime, queue: DispatchQueue = .main, execute work: @escaping () -> Void) {
        DelayedWorkTracker.asyncAfter(deadline: deadline, queue: queue, execute: work)
    }

    private func startInternal(port: UInt16) {
        // Install trackers
        runLoopObserver = RunLoopObserver(stateTracker: stateTracker)
        runLoopObserver?.install()

        animationTracker = AnimationTracker(stateTracker: stateTracker)
        animationTracker?.install()

        networkTracker = NetworkTracker(stateTracker: stateTracker)
        networkTracker?.install()

        timerTracker = TimerTracker(stateTracker: stateTracker)
        timerTracker?.install()
        DelayedWorkTracker.install()
        // Hook RCTTiming now (nothing counted until framework react_native):
        // JS timers created before the host's config are then seen.
        ReactNativeTracker.shared.observe()

        // Start TCP server
        server = SyncServer(stateTracker: stateTracker, port: port)
        server?.start()
    }

    private func stopInternal() {
        server?.stop()
        server = nil
        runLoopObserver?.uninstall()
        runLoopObserver = nil
        animationTracker = nil
        networkTracker = nil
        timerTracker = nil
    }
}

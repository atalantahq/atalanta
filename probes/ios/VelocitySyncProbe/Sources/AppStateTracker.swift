import Foundation

/// Central registry tracking all sources of app busyness.
/// Each tracker reports its state via atomic operations.
public final class AppStateTracker {
    public static let shared = AppStateTracker()

    private let _runLoopBusy = AtomicBool()
    private let _animationCount = AtomicCounter()
    private let _networkCount = AtomicCounter()
    private let _timerCount = AtomicCounter()
    private let _rnBridgeBusy = AtomicBool()

    private init() {}

    /// Trackers opted out by name (`config` `ignore_trackers`): never counted.
    private let optOutLock = NSLock()
    private var optedOut = Set<String>()

    /// Tracker names `ignore_trackers` takes.
    public static let trackerNames = ["runloop", "animation", "network", "timer", "rn_bridge", "rn_timers", "rn_mounting"]

    /// Whether the app is idle (no pending work from any tracker).
    public var isIdle: Bool { isIdle(maxTimerInterval: nil) }

    /// `isIdle`, with a timer horizon of `maxTimerInterval` (a host
    /// connection's `max_timer_ms`) instead of the global one.
    public func isIdle(maxTimerInterval: TimeInterval?) -> Bool {
        counts(maxTimerInterval).isEmpty
    }

    /// (tracker, count) for each tracker with pending work, opt-outs left out.
    private func counts(_ maxInterval: TimeInterval?) -> [(String, Int)] {
        let out = optedOutTrackers
        var c: [(String, Int)] = []
        func add(_ name: String, _ n: @autoclosure () -> Int) {
            guard !out.contains(name) else { return }
            let v = n()
            if v > 0 { c.append((name, v)) }
        }
        add("runloop", _runLoopBusy.value ? 1 : 0)
        add("animation", _animationCount.value)
        add("network", _networkCount.value)
        add("timer", timerCount(maxInterval))
        add("rn_bridge", _rnBridgeBusy.value ? 1 : 0)
        for (name, n) in ReactNativeTracker.shared.pending(maxInterval: maxInterval) { add(name, n) }
        return c
    }

    /// Timers marked by hand plus the short one-shot timers and delayed
    /// performs the probe tracks itself (`TimerTracker`, `DelayedWorkTracker`).
    private func timerCount(_ maxInterval: TimeInterval?) -> Int {
        _timerCount.value + TimerTracker.pendingCount(maxInterval: maxInterval)
            + DelayedWorkTracker.pendingCount(maxInterval: maxInterval)
    }

    /// Turn on a framework's trackers (`config` `framework`); false for one
    /// the probe has none for. Process-wide: the app is what it is.
    public func enableFramework(_ framework: String) -> Bool {
        guard framework == "react_native" else { return false }
        ReactNativeTracker.shared.install()
        return true
    }

    /// Trackers that can't tell (a hook that didn't take).
    public var unknownTrackers: [String] {
        let out = optedOutTrackers
        return ReactNativeTracker.shared.unknown.filter { !out.contains($0) }
    }

    /// Trackers that never count (process-wide; replaces the last list).
    /// Returns the names this probe doesn't have.
    @discardableResult
    public func optOut(_ names: [String]) -> [String] {
        optOutLock.lock()
        optedOut = Set(names.filter { !$0.isEmpty })
        optOutLock.unlock()
        return names.filter { !Self.trackerNames.contains($0) }
    }

    public var optedOutTrackers: Set<String> {
        optOutLock.lock()
        defer { optOutLock.unlock() }
        return optedOut
    }

    /// `ignored: perpetual <kind>` for perpetual work no longer waited for
    /// (as of the last count): a self-rescheduling `performSelector`, a JS
    /// timer re-armed as it fires. Reported in `pending`, idle or not.
    public var ignoredNotes: [String] {
        let out = optedOutTrackers
        var notes: [String] = []
        if !out.contains("timer") && DelayedWorkTracker.ignoredPerpetual { notes.append("ignored: perpetual timer") }
        if !out.contains("rn_timers") && ReactNativeTracker.shared.ignoredPerpetual { notes.append("ignored: perpetual rn_timers") }
        return notes
    }

    /// List of reasons the app is not idle.
    public var pendingReasons: [String] { pendingReasons(maxTimerInterval: nil) }

    /// `pendingReasons` with a connection's timer horizon.
    public func pendingReasons(maxTimerInterval: TimeInterval?) -> [String] {
        var reasons: [String] = []
        for (name, n) in counts(maxTimerInterval) {
            switch name {
            case "runloop", "rn_bridge": reasons.append(name)
            default: reasons.append("\(name):\(n)")
            }
        }
        return reasons + ignoredNotes
    }

    // MARK: - Registration methods

    public func markRunLoopBusy(_ busy: Bool) { _runLoopBusy.set(busy) }
    public func incrementNetwork() { _networkCount.increment() }
    public func decrementNetwork() { _networkCount.decrement() }
    public func incrementAnimation() { _animationCount.increment() }
    public func decrementAnimation() { _animationCount.decrement() }
    public func incrementTimer() { _timerCount.increment() }
    public func decrementTimer() { _timerCount.decrement() }
    /// An explicit busy signal from the app (e.g. its own React Native
    /// bridge hook). The probe has no RN tracking of its own: nothing in it
    /// ever called this, so `enableRN()` used to enable a signal that never
    /// fired. A busy mark now counts whenever the app sets one.
    public func markRNBridgeBusy(_ busy: Bool) { _rnBridgeBusy.set(busy) }
}

// MARK: - Thread-safe atomic primitives

final class AtomicBool {
    private var _value: Int32 = 0
    var value: Bool { OSAtomicAdd32(0, &_value) != 0 }
    func set(_ newValue: Bool) {
        if newValue {
            OSAtomicTestAndSet(0, &_value)
        } else {
            OSAtomicTestAndClear(0, &_value)
        }
    }
}

/// A count that never goes below zero. The floor is applied at the
/// decrement (a compare-and-swap loop, like Android's `NetworkTracker`): an
/// unmatched decrement used to drive the stored value negative, and the next
/// increment then read as 0, so work in flight looked idle.
final class AtomicCounter {
    private var _value: Int32 = 0
    var value: Int { Int(OSAtomicAdd32(0, &_value)) }
    func increment() { OSAtomicIncrement32(&_value) }
    func decrement() {
        while true {
            let current = OSAtomicAdd32(0, &_value)
            if current <= 0 { return }
            if OSAtomicCompareAndSwap32(current, current - 1, &_value) { return }
        }
    }
}

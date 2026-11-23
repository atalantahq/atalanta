import Foundation
import ObjectiveC

/// React Native's own pending work, turned on by a host connection's
/// `config` with `"framework":"react_native"` (the suite's `framework:
/// react_native`). The probe has no compile-time dependency on React
/// Native: every hook is found by name at run time, checked with
/// `class_getInstanceMethod` / `respondsToSelector` and its argument
/// encodings, and skipped with one warning (that tracker is then reported
/// in `unknown`, never idle) when it isn't there.
///
/// - `rn_timers`: JS timers (`setTimeout`) due within the horizon. The
///   `RCTTiming` instances are found by swizzling
///   `createTimerForNextFrame:duration:jsSchedulingTime:repeats:` (RN 0.64+,
///   bridge and bridgeless) or the older `createTimer:...` with the same
///   argument types; their `_timers` ivar (`_RCTTimer`: `target`,
///   `interval`, `repeats`) is read under its `@synchronized` lock.
///   Repeating timers (`setInterval`) don't count; `repeats` is read
///   before `target`, which `-reschedule` reassigns outside the lock (only
///   for repeating timers). The `RCTTiming` hook goes in when the probe
///   starts (`observe()`), so timers JS creates before the host's `config`
///   are counted once RN tracking is on. A JS timer re-armed as it fires
///   (a recursive `setTimeout` poll, a `requestAnimationFrame` loop: a new
///   one-shot of about the same duration created within `linkWindow` of the
///   last one firing, `classReposts` times in a row over `perpetualSpan`)
///   stops counting: `ignored: perpetual rn_timers` in `pending`.
/// - `rn_mounting`: Fabric transactions scheduled
///   (`-[RCTMountingManager scheduleTransaction:]`) but not yet started on
///   the main thread (`initiateTransaction:`). Both take a C++
///   `MountingCoordinator` (by `shared_ptr` value, passed by invisible
///   reference, and by `const &`): the hooks forward the pointer
///   untouched. A transaction that never starts stops counting after
///   `staleTransaction` (one warning).
///
/// RN's networking goes through `NSURLSession` (`NetworkTracker`) and its
/// animations through Core Animation (`AnimationTracker`).
final class ReactNativeTracker {
    static let shared = ReactNativeTracker()

    /// A scheduled transaction not started after this long is dropped.
    static var staleTransaction: TimeInterval = 5

    /// A JS re-post must be created this soon after its predecessor fired.
    static let linkWindow: TimeInterval = 0.25
    static let classReposts = 3
    static let perpetualSpan: TimeInterval = 2

    private let lock = NSLock()
    private var installed = false
    private var timingHookTried = false
    private var timingHooked = false
    private var mountingHooked = false
    private let timings = NSHashTable<AnyObject>.weakObjects()
    /// Schedule times of transactions not yet initiated, oldest first.
    private var scheduled: [Date] = []

    /// A one-shot JS timer seen created: its re-post chain.
    private struct JSTimer {
        let target: Date
        let duration: TimeInterval
        let chain: Int
        let chainStart: Date
    }
    /// Live one-shot timers by "instance:callbackID".
    private var jsTimers: [String: JSTimer] = [:]
    /// Ones that fired lately (candidates for a re-post).
    private var jsFired: [JSTimer] = []
    private var jsPerpetualSeen = false

    /// Hook `RCTTiming` (record instances and re-post chains; nothing is
    /// counted until `install()`). Called when the probe starts, so JS
    /// timers created before the host's `config` are seen.
    func observe() {
        lock.lock()
        let first = !timingHookTried
        timingHookTried = true
        lock.unlock()
        if first { hookTiming() }
    }

    /// Hook RN (once). Returns the trackers that can't tell.
    @discardableResult
    func install() -> [String] {
        lock.lock()
        let first = !installed
        installed = true
        lock.unlock()
        if first {
            observe()
            hookMounting()
        }
        return unknown
    }

    var enabled: Bool {
        lock.lock()
        defer { lock.unlock() }
        return installed
    }

    /// Trackers that can't tell (their hook didn't take).
    var unknown: [String] {
        lock.lock()
        defer { lock.unlock() }
        guard installed else { return [] }
        var out: [String] = []
        if !timingHooked { out.append("rn_timers") }
        if !mountingHooked { out.append("rn_mounting") }
        return out
    }

    /// (name, count) for each tracker with pending work.
    func pending(maxInterval: TimeInterval?) -> [(String, Int)] {
        guard enabled else { return [] }
        var out: [(String, Int)] = []
        let t = timerCount(maxInterval: maxInterval)
        if t > 0 { out.append(("rn_timers", t)) }
        let m = mountingCount()
        if m > 0 { out.append(("rn_mounting", m)) }
        return out
    }

    // MARK: - RCTTiming

    private func hookTiming() {
        guard let cls = NSClassFromString("RCTTiming") else {
            ProbeWarnings.once("rn-timing", "[VelocitySyncProbe] framework react_native: no RCTTiming class; JS timers not waited for")
            return
        }
        for name in ["createTimerForNextFrame:duration:jsSchedulingTime:repeats:",
                     "createTimer:duration:jsSchedulingTime:repeats:"] {
            let sel = NSSelectorFromString(name)
            guard let method = class_getInstanceMethod(cls, sel),
                  DelayedWorkTracker.argumentsMatch(method, "@d@B") || DelayedWorkTracker.argumentsMatch(method, "@d@c")
            else { continue }
            typealias Fn = @convention(c) (AnyObject, Selector, AnyObject?, Double, AnyObject?, Bool) -> Void
            let call = unsafeBitCast(method_getImplementation(method), to: Fn.self)
            let block: @convention(block) (AnyObject, AnyObject?, Double, AnyObject?, Bool) -> Void = { [weak self] timing, id, duration, js, repeats in
                self?.sawTiming(timing, id: id, duration: duration, repeats: repeats)
                call(timing, sel, id, duration, js, repeats)
            }
            method_setImplementation(method, imp_implementationWithBlock(block))
            lock.lock()
            timingHooked = true
            lock.unlock()
            return
        }
        ProbeWarnings.once("rn-timing", "[VelocitySyncProbe] framework react_native: RCTTiming has no createTimer method this probe knows; JS timers not waited for")
    }

    private static func key(_ timing: AnyObject, _ id: Any?) -> String {
        "\(UInt(bitPattern: ObjectIdentifier(timing).hashValue)):\((id as? NSNumber)?.stringValue ?? "?")"
    }

    /// The callback ids in `timing`'s `_timers`, or nil.
    private static func liveIDs(_ timing: AnyObject) -> [Any]? {
        guard let cls = object_getClass(timing),
              let ivar = class_getInstanceVariable(cls, "_timers"),
              let timers = object_getIvar(timing, ivar) as? NSDictionary else { return nil }
        objc_sync_enter(timers)
        defer { objc_sync_exit(timers) }
        return timers.allKeys
    }

    private func sawTiming(_ timing: AnyObject, id: AnyObject?, duration: Double, repeats: Bool) {
        let live = repeats ? nil : Self.liveIDs(timing)
        let now = Date()
        lock.lock()
        defer { lock.unlock() }
        if !timings.contains(timing) { timings.add(timing) }
        guard !repeats, let live else { return }
        // Timers of this instance no longer live: fired (due) or deleted.
        let prefix = Self.key(timing, nil).dropLast(1)
        let liveKeys = Set(live.map { Self.key(timing, $0) })
        for (k, t) in jsTimers where k.hasPrefix(prefix) && !liveKeys.contains(k) {
            jsTimers[k] = nil
            if t.target <= now { jsFired.append(t) }
        }
        jsFired.removeAll { now.timeIntervalSince($0.target) > Self.linkWindow }
        var chain = 0
        var start = now
        if let i = jsFired.firstIndex(where: { abs($0.duration - duration) <= max(0.05, $0.duration / 4) }) {
            chain = jsFired[i].chain + 1
            start = jsFired[i].chainStart
            jsFired.remove(at: i)
        }
        jsTimers[Self.key(timing, id)] = JSTimer(target: now.addingTimeInterval(duration), duration: duration,
                                                 chain: chain, chainStart: start)
        if jsTimers.count > 1024 { jsTimers.removeAll() } // bounded: never grows with deleted ids
    }

    /// Whether a JS timer is the latest of a perpetual re-post chain.
    private func perpetual(_ timing: AnyObject, _ id: Any, now: Date) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard let t = jsTimers[Self.key(timing, id)] else { return false }
        return t.chain >= Self.classReposts && now.timeIntervalSince(t.chainStart) >= Self.perpetualSpan
    }

    /// Whether the last count skipped a perpetual JS timer.
    var ignoredPerpetual: Bool {
        lock.lock()
        defer { lock.unlock() }
        return installed && jsPerpetualSeen
    }

    /// One-shot JS timers due within the horizon, across RCTTiming instances.
    func timerCount(maxInterval: TimeInterval?) -> Int {
        lock.lock()
        let instances = timings.allObjects
        lock.unlock()
        let limit = max(0, maxInterval ?? TimerTracker.maxTrackableInterval)
        let now = Date()
        let horizon = now.addingTimeInterval(limit)
        var count = 0
        var perpetualSeen = false
        for timing in instances {
            guard let cls = object_getClass(timing),
                  let ivar = class_getInstanceVariable(cls, "_timers"),
                  let timers = object_getIvar(timing, ivar) as? NSDictionary
            else {
                ProbeWarnings.once("rn-timing-ivar", "[VelocitySyncProbe] RCTTiming has no _timers dictionary; JS timers not waited for")
                continue
            }
            objc_sync_enter(timers)
            let pairs = timers.map { ($0.key, $0.value) }
            objc_sync_exit(timers)
            for (id, value) in pairs {
                guard let timer = value as? NSObject else { continue }
                // `repeats` first: `target` of a repeating timer is reassigned
                // by -reschedule outside the lock; never touch it.
                guard let repeats = (Self.read(timer, "repeats") as? NSNumber)?.boolValue, !repeats,
                      let interval = (Self.read(timer, "interval") as? NSNumber)?.doubleValue,
                      interval <= limit,
                      let target = Self.read(timer, "target") as? Date,
                      target <= horizon
                else { continue }
                if perpetual(timing, id, now: now) {
                    perpetualSeen = true
                    continue
                }
                count += 1
            }
        }
        lock.lock()
        jsPerpetualSeen = perpetualSeen
        lock.unlock()
        if perpetualSeen {
            ProbeWarnings.once("rn-timers-perpetual", "[VelocitySyncProbe] a JS timer re-armed as it fires (polling, requestAnimationFrame): not waited for")
        }
        return count
    }

    /// `obj.key` through its getter, only when it has one (KVC on a missing
    /// key raises an Objective-C exception Swift can't catch).
    private static func read(_ obj: NSObject, _ key: String) -> Any? {
        guard obj.responds(to: NSSelectorFromString(key)) else { return nil }
        return obj.value(forKey: key)
    }

    // MARK: - RCTMountingManager

    private func hookMounting() {
        guard let cls = NSClassFromString("RCTMountingManager") else {
            ProbeWarnings.once("rn-mounting", "[VelocitySyncProbe] framework react_native: no RCTMountingManager (old architecture?); Fabric mounting not waited for")
            return
        }
        let schedule = NSSelectorFromString("scheduleTransaction:")
        let initiate = NSSelectorFromString("initiateTransaction:")
        guard let scheduleMethod = class_getInstanceMethod(cls, schedule),
              let initiateMethod = class_getInstanceMethod(cls, initiate),
              Self.takesCoordinator(scheduleMethod), Self.takesCoordinator(initiateMethod)
        else {
            ProbeWarnings.once("rn-mounting", "[VelocitySyncProbe] framework react_native: RCTMountingManager's scheduleTransaction:/initiateTransaction: not as expected; Fabric mounting not waited for")
            return
        }
        typealias Fn = @convention(c) (AnyObject, Selector, UnsafeRawPointer?) -> Void
        let callSchedule = unsafeBitCast(method_getImplementation(scheduleMethod), to: Fn.self)
        let callInitiate = unsafeBitCast(method_getImplementation(initiateMethod), to: Fn.self)
        let onSchedule: @convention(block) (AnyObject, UnsafeRawPointer?) -> Void = { [weak self] manager, coordinator in
            self?.transactionScheduled()
            callSchedule(manager, schedule, coordinator)
        }
        let onInitiate: @convention(block) (AnyObject, UnsafeRawPointer?) -> Void = { [weak self] manager, coordinator in
            self?.transactionStarted()
            callInitiate(manager, initiate, coordinator)
        }
        method_setImplementation(scheduleMethod, imp_implementationWithBlock(onSchedule))
        method_setImplementation(initiateMethod, imp_implementationWithBlock(onInitiate))
        lock.lock()
        mountingHooked = true
        lock.unlock()
    }

    /// One argument, passed as a pointer: `const MountingCoordinator &`
    /// (`^`/`r^`), or a `shared_ptr` by value, which the C++ ABI passes by
    /// invisible reference since it isn't trivially copyable.
    static func takesCoordinator(_ method: Method) -> Bool {
        guard method_getNumberOfArguments(method) == 3,
              let raw = method_copyArgumentType(method, 2) else { return false }
        defer { free(raw) }
        let enc = String(cString: raw).drop { "rnNoORV".contains($0) }
        return enc.hasPrefix("^") || enc.hasPrefix("{shared_ptr")
    }

    private func transactionScheduled() {
        lock.lock()
        scheduled.append(Date())
        lock.unlock()
    }

    private func transactionStarted() {
        lock.lock()
        if !scheduled.isEmpty { scheduled.removeFirst() }
        lock.unlock()
    }

    func mountingCount() -> Int {
        let cutoff = Date().addingTimeInterval(-Self.staleTransaction)
        lock.lock()
        let before = scheduled.count
        scheduled.removeAll { $0 < cutoff }
        let dropped = before - scheduled.count
        let n = scheduled.count
        lock.unlock()
        if dropped > 0 {
            ProbeWarnings.once("rn-mounting-stale", "[VelocitySyncProbe] a Fabric transaction was scheduled but never started; no longer waited for")
        }
        return n
    }
}

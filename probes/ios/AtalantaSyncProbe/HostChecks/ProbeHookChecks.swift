import Foundation
import ObjectiveC

// Host checks for the hooks: performSelector:afterDelay: tracking (P2) and
// React Native's RCTTiming / RCTMountingManager (P4), against fake
// Objective-C classes registered under the real names, so the probe's
// NSClassFromString / swizzle / ivar paths really run.

/// Stands in for RN's `_RCTTimer`.
/// `target` is `nonatomic, strong` and reassigned by `-reschedule` for
/// repeating timers outside the lock: the probe must not read it for those.
@objc(_RCTTimer) final class FakeRCTTimer: NSObject {
    static var repeatingTargetReads = 0
    private let _target: Date
    @objc var target: Date {
        if repeats { FakeRCTTimer.repeatingTargetReads += 1 }
        return _target
    }
    @objc let interval: TimeInterval
    @objc let repeats: Bool
    init(target: Date, interval: TimeInterval, repeats: Bool) {
        self._target = target
        self.interval = interval
        self.repeats = repeats
    }
}

/// Stands in for RN's `RCTTiming`: `_timers` keyed by callback id, guarded
/// with @synchronized as RN does.
@objc(RCTTiming) class FakeRCTTiming: NSObject {
    @objc var _timers = NSMutableDictionary()
    @objc dynamic func createTimerForNextFrame(_ callbackID: NSNumber, duration: TimeInterval,
                                               jsSchedulingTime: Date?, repeats: Bool) {
        objc_sync_enter(_timers)
        _timers[callbackID] = FakeRCTTimer(target: Date().addingTimeInterval(duration), interval: duration, repeats: repeats)
        objc_sync_exit(_timers)
    }
    @objc dynamic func deleteTimer(_ callbackID: NSNumber) {
        objc_sync_enter(_timers)
        _timers.removeObject(forKey: callbackID)
        objc_sync_exit(_timers)
    }
}

/// Stands in for RN's `RCTMountingManager`: schedule from any thread, start
/// on the main thread (the coordinator pointer is passed through).
@objc(RCTMountingManager) class FakeMountingManager: NSObject {
    var started = 0
    var lastCoordinator: UnsafeRawPointer?
    @objc dynamic func scheduleTransaction(_ coordinator: UnsafeRawPointer?) {
        DispatchQueue.main.async { self.initiateTransaction(coordinator) }
    }
    @objc dynamic func initiateTransaction(_ coordinator: UnsafeRawPointer?) {
        started += 1
        lastCoordinator = coordinator
    }
    @objc func takesAnObject(_ o: NSObject) {}
}

final class PerformTarget: NSObject {
    var hits = 0
    @objc func tick(_ arg: Any?) { hits += 1 }
}

func hookChecks(port: UInt16) {
    let state = AppStateTracker.shared

    // P2: performSelector:afterDelay:.
    DelayedWorkTracker.install()
    check(DelayedWorkTracker.installedSelectors.count == 4,
          "P2: the performSelector:afterDelay: family is hooked (\(DelayedWorkTracker.installedSelectors))")
    let t = PerformTarget()
    t.perform(#selector(PerformTarget.tick(_:)), with: nil, afterDelay: 0.3)
    check(DelayedWorkTracker.pendingCount(maxInterval: nil) == 1 && !state.isIdle
          && state.pendingReasons.contains("timer:1"),
          "P2: performSelector:afterDelay:0.3 is a pending timer (\(state.pendingReasons))")
    NSObject.cancelPreviousPerformRequests(withTarget: t, selector: #selector(PerformTarget.tick(_:)), object: nil)
    check(DelayedWorkTracker.pendingCount(maxInterval: nil) == 0 && state.isIdle,
          "P2: cancelPreviousPerformRequests(withTarget:selector:object:) drops it")
    t.perform(#selector(PerformTarget.tick(_:)), with: nil, afterDelay: 0.1)
    spin(0.3)
    check(t.hits == 1 && DelayedWorkTracker.pendingCount(maxInterval: nil) == 0 && state.isIdle,
          "P2: it still fires, and is idle after")
    t.perform(#selector(PerformTarget.tick(_:)), with: nil, afterDelay: 5)
    check(DelayedWorkTracker.pendingCount(maxInterval: nil) == 0 && DelayedWorkTracker.pendingCount(maxInterval: 10) == 1,
          "P2: a 5 s delayed perform is past the default horizon, inside max_timer_ms 10000")
    NSObject.cancelPreviousPerformRequests(withTarget: t)
    check(DelayedWorkTracker.pendingCount(maxInterval: 10) == 0, "P2: cancelPreviousPerformRequests(withTarget:) drops it")
    t.perform(#selector(PerformTarget.tick(_:)), with: "x" as NSString, afterDelay: 0.2, inModes: [.common])
    check(DelayedWorkTracker.pendingCount(maxInterval: nil) == 1, "P2: the inModes: variant counts once")
    NSObject.cancelPreviousPerformRequests(withTarget: t, selector: #selector(PerformTarget.tick(_:)), object: "y" as NSString)
    check(DelayedWorkTracker.pendingCount(maxInterval: nil) == 1, "P2: a cancel for another argument leaves it")
    NSObject.cancelPreviousPerformRequests(withTarget: t, selector: #selector(PerformTarget.tick(_:)), object: "x" as NSString)
    check(DelayedWorkTracker.pendingCount(maxInterval: nil) == 0, "P2: a cancel for an equal argument drops it")
    let hitsBefore = t.hits
    spin(0.3)
    check(t.hits == hitsBefore, "P2: the app's own cancel still cancels (the swizzle passes it through)")
    var ran = false
    DelayedWorkTracker.asyncAfter(deadline: .now() + 0.2, queue: .main) { ran = true }
    check(DelayedWorkTracker.pendingCount(maxInterval: nil) == 1, "P2: VelocitySyncProbe.asyncAfter is a pending timer")
    spin(0.4)
    check(ran && DelayedWorkTracker.pendingCount(maxInterval: nil) == 0, "P2: ... until its work ran")
    if let m = class_getInstanceMethod(FakeMountingManager.self, #selector(FakeMountingManager.takesAnObject(_:))) {
        check(!DelayedWorkTracker.argumentsMatch(m, "d") && !ReactNativeTracker.takesCoordinator(m),
              "P2/P4: a method with other argument types is never hooked")
    }

    // P4: React Native, off until config says so. The probe's start() hooks RCTTiming.
    ReactNativeTracker.shared.observe()
    let timing = FakeRCTTiming()
    timing.createTimerForNextFrame(1, duration: 0.5, jsSchedulingTime: nil, repeats: false)
    check(state.isIdle, "P4: RN tracking is off until framework react_native")
    let cfg = askAll(port, [#"{"cmd":"config","framework":"react_native"}"#])
    check(cfg.first?.contains("\"framework\":\"react_native\"") == true && cfg.first?.contains("\"unknown\":[]") == true,
          "P4: config takes framework; both hooks took (\(cfg))")
    check(ReactNativeTracker.shared.enabled, "P4: RN tracking on")
    check(ReactNativeTracker.shared.timerCount(maxInterval: nil) == 1,
          "m7: a JS timer created before config counts once RN tracking is on (no new timer needed)")
    timing.createTimerForNextFrame(2, duration: 0.5, jsSchedulingTime: nil, repeats: false)
    check(ReactNativeTracker.shared.timerCount(maxInterval: nil) == 2 && !state.isIdle
          && state.pendingReasons.contains("rn_timers:2"),
          "P4: setTimeout(500) is pending once RCTTiming is seen (\(state.pendingReasons))")
    check(ReactNativeTracker.shared.timerCount(maxInterval: 0.1) == 0, "P4: max_timer_ms applies to JS timers")
    timing.deleteTimer(1)
    timing.deleteTimer(2)
    timing.createTimerForNextFrame(3, duration: 0.1, jsSchedulingTime: nil, repeats: true)
    FakeRCTTimer.repeatingTargetReads = 0
    check(ReactNativeTracker.shared.timerCount(maxInterval: nil) == 0, "P4: setInterval doesn't count")
    check(FakeRCTTimer.repeatingTargetReads == 0, "m6: a repeating timer's target (reassigned outside the lock) is never read")
    timing.deleteTimer(3)
    timing.createTimerForNextFrame(4, duration: 30, jsSchedulingTime: nil, repeats: false)
    check(ReactNativeTracker.shared.timerCount(maxInterval: nil) == 0, "P4: a 30 s JS timer doesn't count")
    timing.deleteTimer(4)

    let mounting = FakeMountingManager()
    var token = 42
    let pointer = withUnsafePointer(to: &token) { UnsafeRawPointer($0) }
    let scheduled = DispatchSemaphore(value: 0)
    DispatchQueue.global().async {
        mounting.scheduleTransaction(pointer)
        scheduled.signal()
    }
    scheduled.wait()
    check(ReactNativeTracker.shared.mountingCount() == 1 && state.pendingReasons.contains("rn_mounting:1"),
          "P4: a Fabric transaction scheduled but not started is pending (\(state.pendingReasons))")
    spin(0.1)
    check(mounting.started == 1 && mounting.lastCoordinator == pointer && ReactNativeTracker.shared.mountingCount() == 0,
          "P4: started on the main thread, coordinator passed through, then idle")
    ReactNativeTracker.staleTransaction = 0.05
    DispatchQueue.global().sync { _ = ReactNativeTracker.shared } // no-op hop
    let stuck = FakeMountingManager()
    // Schedule without letting the main queue run: then wait past the stale limit.
    let s2 = DispatchSemaphore(value: 0)
    DispatchQueue.global().async { stuck.scheduleTransaction(pointer); s2.signal() }
    s2.wait()
    usleep(100_000)
    check(ReactNativeTracker.shared.mountingCount() == 0 && ProbeWarnings.didWarn("rn-mounting-stale"),
          "P4: a transaction that never starts stops counting, with a warning")
    spin(0.1)
    ReactNativeTracker.staleTransaction = 5
    check(state.isIdle, "P4: idle again (\(state.pendingReasons))")
}

final class Ticker: NSObject {
    var hits = 0
    var again = true
    @objc func tick() {
        hits += 1
        if again { perform(#selector(tick), with: nil, afterDelay: 0.1) }
    }
    @objc func save(_ arg: Any?) { hits += 1 }
}

final class Payload: NSObject {}

func perpetualChecks(port: UInt16) {
    let state = AppStateTracker.shared
    DelayedWorkTracker.reset()

    // M3: a self-rescheduling -tick (the Objective-C poller).
    let ticker = Ticker()
    ticker.perform(#selector(Ticker.tick), with: nil, afterDelay: 0.1)
    check(DelayedWorkTracker.pendingCount(maxInterval: nil) == 1, "M3: a delayed -tick counts")
    var samples: [Int] = []
    for _ in 0..<12 { spin(0.1); samples.append(DelayedWorkTracker.pendingCount(maxInterval: nil)) }
    check(ticker.hits >= 5 && samples.last == 0 && state.isIdle && state.pendingReasons.contains("ignored: perpetual timer"),
          "M3: a performSelector that re-schedules itself as it runs stops counting, reported as ignored (\(samples), \(state.pendingReasons))")
    let idleReply = askAll(port, [#"{"cmd":"wait_idle","timeout_ms":500}"#])
    check(idleReply.first?.contains("\"idle\":true") == true && idleReply.first?.contains("ignored: perpetual timer") == true,
          "M3: wait_idle is idle and says what it ignored (\(idleReply))")
    let oneShot = PerformTarget()
    oneShot.perform(#selector(PerformTarget.tick(_:)), with: nil, afterDelay: 0.3)
    check(DelayedWorkTracker.pendingCount(maxInterval: nil) == 1, "M3: a one-shot perform next to the poller counts")
    NSObject.cancelPreviousPerformRequests(withTarget: oneShot)
    ticker.again = false
    spin(0.3)
    check(DelayedWorkTracker.pendingCount(maxInterval: nil) == 0 && !state.pendingReasons.contains("ignored: perpetual timer"),
          "M3: the poller stopped: nothing ignored any more")

    // A debounce re-armed after it fired (a tap every 0.4 s, 0.1 s save): counts every time.
    let saver = Ticker()
    var always = true
    for _ in 0..<5 {
        saver.perform(#selector(Ticker.save(_:)), with: nil, afterDelay: 0.1)
        always = always && DelayedWorkTracker.pendingCount(maxInterval: nil) == 1
        spin(0.4)
    }
    check(always && saver.hits == 5, "M3: a perform re-armed after it fired (by the next tap) is never a poller")

    // M5: the argument isn't kept alive, and the list is pruned on add, without any sampling.
    weak var weakArg: Payload?
    autoreleasepool {
        let arg = Payload()
        weakArg = arg
        saver.perform(#selector(Ticker.save(_:)), with: arg, afterDelay: 0.01)
    }
    spin(0.1)
    check(weakArg == nil, "M5: a performSelector argument is released once the request ran (the probe holds it weakly)")
    let targets = (0..<600).map { _ in PerformTarget() }
    for t in targets { t.perform(#selector(PerformTarget.tick(_:)), with: nil, afterDelay: 30) }
    check(DelayedWorkTracker.entryCount <= DelayedWorkTracker.maxEntries,
          "M5: the list is capped (\(DelayedWorkTracker.entryCount)) without a host sampling")
    for t in targets { NSObject.cancelPreviousPerformRequests(withTarget: t) }
    for _ in 0..<50 { saver.perform(#selector(Ticker.save(_:)), with: nil, afterDelay: 0.01) }
    spin(0.1)
    saver.perform(#selector(Ticker.save(_:)), with: nil, afterDelay: 0.01)
    check(DelayedWorkTracker.entryCount <= 1, "M5: fired requests are pruned on the next add (\(DelayedWorkTracker.entryCount))")
    spin(0.1)

    // M3: a recursive JS setTimeout poll (a new one-shot as each fires).
    let timing = FakeRCTTiming()
    var id = 100
    timing.createTimerForNextFrame(NSNumber(value: id), duration: 0.1, jsSchedulingTime: nil, repeats: false)
    var js: [Int] = []
    let end = Date().addingTimeInterval(2.8)
    while Date() < end {
        spin(0.02)
        let due = (timing._timers.allValues as? [FakeRCTTimer] ?? []).contains { !$0.repeats && $0.target <= Date() }
        if due {
            timing.deleteTimer(NSNumber(value: id)) // fired
            id += 1
            timing.createTimerForNextFrame(NSNumber(value: id), duration: 0.1, jsSchedulingTime: nil, repeats: false)
        }
        js.append(ReactNativeTracker.shared.timerCount(maxInterval: nil))
    }
    check(js.prefix(20).allSatisfy { $0 == 1 } && js.last == 0 && state.pendingReasons.contains("ignored: perpetual rn_timers"),
          "M3: a recursive setTimeout poll counts at first, then is ignored as perpetual (\(js.suffix(5)))")
    timing.createTimerForNextFrame(9999, duration: 0.5, jsSchedulingTime: nil, repeats: false)
    check(ReactNativeTracker.shared.timerCount(maxInterval: nil) == 1, "M3: a one-shot JS timer next to the poll counts")
    timing.deleteTimer(9999)
    timing.deleteTimer(NSNumber(value: id))

    // M3: per-tracker opt-out.
    let p = PerformTarget()
    p.perform(#selector(PerformTarget.tick(_:)), with: nil, afterDelay: 0.3)
    check(!state.isIdle, "M3: a delayed perform holds the app busy")
    let cfg = askAll(port, [#"{"cmd":"config","ignore_trackers":["timer","nope"]}"#])
    check(cfg.first?.contains("\"unknown_trackers\":[\"nope\"]") == true && state.isIdle,
          "M3: ignore_trackers opts a tracker out (\(cfg))")
    _ = askAll(port, [#"{"cmd":"config","ignore_trackers":[]}"#])
    check(!state.isIdle, "M3: an empty list opts back in")
    NSObject.cancelPreviousPerformRequests(withTarget: p)
    spin(0.1)
}

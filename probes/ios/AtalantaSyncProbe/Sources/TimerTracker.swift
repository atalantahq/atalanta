import Foundation

/// Tracks short, non-repeating timers to detect pending timer-based work.
///
/// Timers are held *weakly* and asked whether they are still valid, the way
/// EarlGrey's `GREYNSTimerIdlingResource` does: a timer that is invalidated
/// (a debounce restarted on every keystroke) or released before it fires
/// stops counting at once. The old counter was only decremented inside the
/// fired block, so every invalidated timer kept the app "busy" forever and
/// each native wait ran to the settle cap.
///
/// Only timers due within `maxTrackableInterval` (1.5 s, EarlGrey's
/// `kGREYConfigKeyNSTimerMaxTrackableInterval` and Detox's default) count:
/// a 60 s session timeout is not work the test should wait for. A host
/// connection can set its own horizon (`config` `max_timer_ms`), so every
/// one-shot timer is remembered with its interval and the horizon is
/// applied when counting.
final class TimerTracker {
    /// Timers due further out than this are ignored.
    static var maxTrackableInterval: TimeInterval {
        get { lock.lock(); defer { lock.unlock() }; return _maxTrackable }
        set { lock.lock(); _maxTrackable = max(0, newValue); lock.unlock() }
    }

    private static let lock = NSLock()
    private static var _maxTrackable: TimeInterval = 1.5
    /// One-shot timers (held weakly) and the interval each was scheduled with.
    private static let timers = NSMapTable<Timer, NSNumber>.weakToStrongObjects()
    private static var installed = false

    private let stateTracker: AppStateTracker

    init(stateTracker: AppStateTracker) {
        self.stateTracker = stateTracker
    }

    func install() {
        Self.lock.lock()
        defer { Self.lock.unlock() }
        // method_exchangeImplementations twice would undo the swizzle.
        guard !Self.installed else { return }
        Self.installed = true
        Self.swizzle(
            cls: Timer.self,
            original: #selector(Timer.scheduledTimer(withTimeInterval:repeats:block:) as (TimeInterval, Bool, @escaping (Timer) -> Void) -> Timer),
            replacement: #selector(Timer.velocity_scheduledTimer(withTimeInterval:repeats:block:))
        )
        // The target/selector API (`scheduledTimer(timeInterval:target:
        // selector:userInfo:repeats:)`) is what Objective-C code and most
        // older Swift code use.
        Self.swizzle(
            cls: Timer.self,
            original: #selector(Timer.scheduledTimer(timeInterval:target:selector:userInfo:repeats:)),
            replacement: #selector(Timer.velocity_scheduledTimer(timeInterval:target:selector:userInfo:repeats:))
        )
    }

    /// Remember `timer` if it is one-shot. Whether it counts is decided
    /// when counting, against the asking connection's horizon.
    static func track(_ timer: Timer, interval: TimeInterval, repeats: Bool) {
        guard !repeats else { return }
        lock.lock()
        timers.setObject(NSNumber(value: interval), forKey: timer)
        lock.unlock()
    }

    /// Tracked timers that are still valid and due within the trackable
    /// window. Invalidated and released timers drop out here.
    static var pendingCount: Int {
        pendingCount(maxInterval: nil)
    }

    /// `pendingCount` with a horizon of `maxInterval` instead of
    /// `maxTrackableInterval` (a connection's `max_timer_ms`): a timer
    /// scheduled further out than it, or not yet due within it, is ignored.
    static func pendingCount(maxInterval: TimeInterval?) -> Int {
        lock.lock()
        defer { lock.unlock() }
        let limit = max(0, maxInterval ?? _maxTrackable)
        let horizon = Date().addingTimeInterval(limit)
        var count = 0
        var dead: [Timer] = []
        let keys = timers.keyEnumerator()
        while let timer = keys.nextObject() as? Timer {
            guard timer.isValid else {
                dead.append(timer)
                continue
            }
            let interval = timers.object(forKey: timer)?.doubleValue ?? 0
            if interval <= limit && timer.fireDate <= horizon {
                count += 1
            }
        }
        for timer in dead { timers.removeObject(forKey: timer) }
        return count
    }

    private static func swizzle(cls: AnyClass, original: Selector, replacement: Selector) {
        guard let originalMethod = class_getClassMethod(cls, original),
              let replacementMethod = class_getClassMethod(cls, replacement)
        else { return }
        method_exchangeImplementations(originalMethod, replacementMethod)
    }
}

extension Timer {
    // After the swizzle these names point at the original implementations,
    // so the calls below are not recursive.
    @objc class func velocity_scheduledTimer(
        withTimeInterval interval: TimeInterval,
        repeats: Bool,
        block: @escaping (Timer) -> Void
    ) -> Timer {
        let timer = velocity_scheduledTimer(withTimeInterval: interval, repeats: repeats, block: block)
        TimerTracker.track(timer, interval: interval, repeats: repeats)
        return timer
    }

    @objc class func velocity_scheduledTimer(
        timeInterval interval: TimeInterval,
        target: Any,
        selector: Selector,
        userInfo: Any?,
        repeats: Bool
    ) -> Timer {
        let timer = velocity_scheduledTimer(
            timeInterval: interval, target: target, selector: selector,
            userInfo: userInfo, repeats: repeats)
        TimerTracker.track(timer, interval: interval, repeats: repeats)
        return timer
    }
}

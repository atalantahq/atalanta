// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

import Foundation
import ObjectiveC

/// Delayed work that is not an `NSTimer` scheduled through
/// `+scheduledTimer...` (see `TimerTracker`): `-performSelector:withObject:
/// afterDelay:` (and `inModes:`), and work posted with
/// `AtalantaSyncProbe.asyncAfter`. Counted like a one-shot timer: while it is
/// due within the horizon (1.5 s, or the connection's `max_timer_ms`) and
/// was scheduled within it, as EarlGrey's `performSelector:afterDelay:`
/// tracking does.
///
/// `performSelector` entries drop out when they are due (the run loop is
/// then about to run them, which the run-loop observer sees), when
/// `+cancelPreviousPerformRequestsWithTarget:` (with or without a selector)
/// cancels them, or when the target is released. The swizzled methods call
/// the originals with the same arguments: the app's own cancel calls still
/// find their requests.
///
/// **`dispatch_after` / `DispatchQueue.asyncAfter` are not tracked.** There
/// is no public API to observe them: they are C functions, not Objective-C
/// methods, so they can't be swizzled, and Swift's `asyncAfter` is
/// implemented in the system's Dispatch overlay, so rebinding the app's own
/// symbol pointers (fishhook-style, which is not public API and writes into
/// the loader's tables) would still miss it. Work the app wants waited for
/// can go through `AtalantaSyncProbe.asyncAfter(deadline:queue:execute:)`
/// (a tracked `asyncAfter`), or mark itself with
/// `AppStateTracker.shared.incrementTimer()` / `decrementTimer()`.
///
/// Memory: entries are pruned on every add (due, cancelled, target or
/// argument gone) and capped at `maxEntries`; the argument is held weakly
/// (`performSelector` itself retains it until the request fires or is
/// cancelled, so a live request's argument is always there), never kept
/// alive by the probe.
///
/// A self-rescheduling `-tick` (the method posts itself again with
/// `performSelector:afterDelay:` as it runs: the classic Objective-C
/// poller) is perpetual after `reposts` re-posts in a row and stops
/// counting (`ignored: perpetual timer` in `pending`). A re-post is the same
/// target and selector scheduled within `linkWindow` of the previous one
/// firing; a debounce (cancelled before it fired, or re-armed by a later
/// tap) is never one.
final class DelayedWorkTracker {
    private final class Entry {
        weak var target: NSObject?
        let selector: Selector?
        weak var argument: AnyObject?
        let hadArgument: Bool
        let fireDate: Date
        let interval: TimeInterval
        /// Re-posts in a row that led to this one.
        var chain = 0
        /// For `asyncAfter`: cleared once the block ran.
        var done = false

        init(target: NSObject?, selector: Selector?, argument: AnyObject?, interval: TimeInterval) {
            self.target = target
            self.selector = selector
            self.argument = argument
            self.hadArgument = argument != nil
            self.interval = interval
            self.fireDate = Date().addingTimeInterval(max(0, interval))
        }
    }

    /// A request that fired (identity only: nothing is retained).
    private struct Fired {
        let target: ObjectIdentifier
        let selector: Selector
        let fireDate: Date
        let chain: Int
    }

    /// Most entries kept; the oldest go first (with one warning).
    static let maxEntries = 512
    /// A re-post must be scheduled this soon after the previous one fired.
    static let linkWindow: TimeInterval = 0.2
    /// Re-posts in a row that make a perpetual poller.
    static let reposts = 2

    private static let lock = NSLock()
    private static var entries: [Entry] = []
    private static var fired: [Fired] = []
    private static var perpetualSeen = false
    private static var installed = false
    /// The swizzles that took (for the host checks and `unknown`).
    private(set) static var installedSelectors: [String] = []
    /// Nested calls (one variant calling another) are tracked once.
    private static let depthKey = "com.atalanta.delayed-perform.depth"

    /// Swizzle the `performSelector:afterDelay:` family once. A method that
    /// isn't there (or has another signature) is skipped with one warning.
    static func install() {
        lock.lock()
        guard !installed else { lock.unlock(); return }
        installed = true
        lock.unlock()

        let perform = NSSelectorFromString("performSelector:withObject:afterDelay:")
        if swizzleInstance(perform, types: ":@d", { original in
            typealias Fn = @convention(c) (NSObject, Selector, Selector, AnyObject?, Double) -> Void
            let call = unsafeBitCast(original, to: Fn.self)
            let block: @convention(block) (NSObject, Selector, AnyObject?, Double) -> Void = { obj, sel, arg, delay in
                DelayedWorkTracker.tracking(obj, sel, arg, delay) { call(obj, perform, sel, arg, delay) }
            }
            return imp_implementationWithBlock(block)
        }) { installedSelectors.append("performSelector:withObject:afterDelay:") }

        let performModes = NSSelectorFromString("performSelector:withObject:afterDelay:inModes:")
        if swizzleInstance(performModes, types: ":@d@", { original in
            typealias Fn = @convention(c) (NSObject, Selector, Selector, AnyObject?, Double, NSArray?) -> Void
            let call = unsafeBitCast(original, to: Fn.self)
            let block: @convention(block) (NSObject, Selector, AnyObject?, Double, NSArray?) -> Void = { obj, sel, arg, delay, modes in
                DelayedWorkTracker.tracking(obj, sel, arg, delay) { call(obj, performModes, sel, arg, delay, modes) }
            }
            return imp_implementationWithBlock(block)
        }) { installedSelectors.append("performSelector:withObject:afterDelay:inModes:") }

        let cancelAll = NSSelectorFromString("cancelPreviousPerformRequestsWithTarget:")
        if swizzleClass(cancelAll, types: "@", { original in
            typealias Fn = @convention(c) (AnyClass, Selector, AnyObject?) -> Void
            let call = unsafeBitCast(original, to: Fn.self)
            // `target` may be nil in Objective-C (a no-op there).
            let block: @convention(block) (AnyClass, AnyObject?) -> Void = { cls, target in
                call(cls, cancelAll, target)
                if let target { DelayedWorkTracker.cancel(target: target, selector: nil, argument: nil, anyArgument: true) }
            }
            return imp_implementationWithBlock(block)
        }) { installedSelectors.append("cancelPreviousPerformRequestsWithTarget:") }

        let cancelOne = NSSelectorFromString("cancelPreviousPerformRequestsWithTarget:selector:object:")
        if swizzleClass(cancelOne, types: "@:@", { original in
            typealias Fn = @convention(c) (AnyClass, Selector, AnyObject?, Selector?, AnyObject?) -> Void
            let call = unsafeBitCast(original, to: Fn.self)
            let block: @convention(block) (AnyClass, AnyObject?, Selector?, AnyObject?) -> Void = { cls, target, sel, arg in
                call(cls, cancelOne, target, sel, arg)
                if let target, let sel {
                    DelayedWorkTracker.cancel(target: target, selector: sel, argument: arg, anyArgument: false)
                }
            }
            return imp_implementationWithBlock(block)
        }) { installedSelectors.append("cancelPreviousPerformRequestsWithTarget:selector:object:") }

        if installedSelectors.count < 4 {
            ProbeWarnings.once("delayed-perform",
                "[AtalantaSyncProbe] performSelector:afterDelay: tracking incomplete (\(installedSelectors)); such delayed work may not be waited for")
        }
    }

    /// Track one `performSelector:afterDelay:` (once, however the variants nest).
    private static func tracking(_ obj: NSObject, _ sel: Selector, _ arg: AnyObject?, _ delay: Double, _ original: () -> Void) {
        let thread = Thread.current.threadDictionary
        let depth = (thread[depthKey] as? Int) ?? 0
        if depth == 0 {
            let entry = Entry(target: obj, selector: sel, argument: arg, interval: delay)
            lock.lock()
            add(entry)
            lock.unlock()
        }
        thread[depthKey] = depth + 1
        defer { thread[depthKey] = depth }
        original()
    }

    private static func cancel(target: AnyObject, selector: Selector?, argument: AnyObject?, anyArgument: Bool) {
        lock.lock()
        defer { lock.unlock() }
        entries.removeAll { e in
            guard e.selector != nil, let t = e.target, t === target else { return false }
            if let selector, e.selector != selector { return false }
            if anyArgument { return true }
            if !e.hadArgument { return argument == nil }
            guard let a = e.argument, let b = argument else { return false }
            return a === b || (a as? NSObject)?.isEqual(b) == true
        }
    }

    /// Work posted with `AtalantaSyncProbe.asyncAfter`.
    static func asyncAfter(deadline: DispatchTime, queue: DispatchQueue, execute work: @escaping () -> Void) {
        let delay = max(0, Double(Int64(deadline.uptimeNanoseconds) - Int64(DispatchTime.now().uptimeNanoseconds)) / 1e9)
        let entry = Entry(target: nil, selector: nil, argument: nil, interval: delay)
        lock.lock()
        add(entry)
        lock.unlock()
        queue.asyncAfter(deadline: deadline) {
            defer {
                lock.lock()
                entry.done = true
                entries.removeAll { $0 === entry }
                lock.unlock()
            }
            work()
        }
    }

    /// Delayed work due within the horizon (`maxInterval`, else
    /// `TimerTracker.maxTrackableInterval`) and scheduled within it.
    static func pendingCount(maxInterval: TimeInterval?) -> Int {
        let limit = max(0, maxInterval ?? TimerTracker.maxTrackableInterval)
        let now = Date()
        let horizon = now.addingTimeInterval(limit)
        lock.lock()
        defer { lock.unlock() }
        prune(now)
        var n = 0
        var perpetual = false
        for e in entries where e.interval <= limit && e.fireDate <= horizon {
            if e.chain >= reposts { perpetual = true } else { n += 1 }
        }
        perpetualSeen = perpetual
        if perpetual {
            ProbeWarnings.once("delayed-perform-perpetual",
                "[AtalantaSyncProbe] a performSelector:afterDelay: that re-schedules itself as it runs (a poller): not waited for")
        }
        return n
    }

    /// Whether the last `pendingCount` skipped a self-rescheduling perform.
    static var ignoredPerpetual: Bool {
        lock.lock()
        defer { lock.unlock() }
        return perpetualSeen
    }

    /// Entries held (for the host checks).
    static var entryCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return entries.count
    }

    /// Add under `lock`: prune first, link a re-post, cap the list.
    private static func add(_ entry: Entry) {
        let now = Date()
        prune(now)
        if let target = entry.target, let selector = entry.selector {
            let id = ObjectIdentifier(target)
            if let i = fired.lastIndex(where: { $0.target == id && $0.selector == selector
                && now.timeIntervalSince($0.fireDate) <= linkWindow }) {
                entry.chain = fired[i].chain + 1
                fired.remove(at: i)
            }
        }
        entries.append(entry)
        if entries.count > maxEntries {
            entries.removeFirst(entries.count - maxEntries)
            ProbeWarnings.once("delayed-perform-cap",
                "[AtalantaSyncProbe] more than \(maxEntries) delayed performs pending; the oldest are no longer tracked")
        }
    }

    /// Under `lock`. performSelector entries leave once due (recorded as
    /// fired, by identity), cancelled, or their target or argument is gone;
    /// asyncAfter ones when their block ran.
    private static func prune(_ now: Date) {
        fired.removeAll { now.timeIntervalSince($0.fireDate) > linkWindow }
        entries.removeAll { e in
            if e.done { return true }
            guard let selector = e.selector else { return false }
            guard let target = e.target else { return true }
            if e.hadArgument && e.argument == nil { return true }
            if e.fireDate <= now {
                if now.timeIntervalSince(e.fireDate) <= linkWindow {
                    fired.append(Fired(target: ObjectIdentifier(target), selector: selector, fireDate: e.fireDate, chain: e.chain))
                }
                return true
            }
            return false
        }
    }

    /// For the host checks.
    static func reset() {
        lock.lock()
        entries.removeAll()
        fired.removeAll()
        perpetualSeen = false
        lock.unlock()
    }

    // MARK: - Swizzling with signature checks

    /// `types`: the argument encodings after self and _cmd, one character
    /// each (`:` selector, `@` object, `d` double). A method whose
    /// arguments differ is left alone: calling it through a mismatched IMP
    /// would corrupt arguments.
    static func argumentsMatch(_ method: Method, _ types: String) -> Bool {
        let expected = Array(types)
        guard method_getNumberOfArguments(method) == UInt32(expected.count + 2) else { return false }
        for (i, want) in expected.enumerated() {
            guard let raw = method_copyArgumentType(method, UInt32(i + 2)) else { return false }
            defer { free(raw) }
            // Strip type qualifiers (r const, n in, o out, N inout, ...).
            let enc = String(cString: raw).drop { "rnNoORV".contains($0) }
            guard enc.first == want else { return false }
        }
        return true
    }

    private static func swizzleInstance(_ sel: Selector, types: String, _ make: (IMP) -> IMP) -> Bool {
        guard let method = class_getInstanceMethod(NSObject.self, sel), argumentsMatch(method, types) else { return false }
        let original = method_getImplementation(method)
        method_setImplementation(method, make(original))
        return true
    }

    private static func swizzleClass(_ sel: Selector, types: String, _ make: (IMP) -> IMP) -> Bool {
        guard let method = class_getClassMethod(NSObject.self, sel), argumentsMatch(method, types) else { return false }
        let original = method_getImplementation(method)
        method_setImplementation(method, make(original))
        return true
    }
}

/// One-time warnings (the probe has no logger of its own).
enum ProbeWarnings {
    private static let lock = NSLock()
    private static var warned = Set<String>()

    @discardableResult
    static func once(_ key: String, _ message: @autoclosure () -> String) -> Bool {
        lock.lock()
        let first = warned.insert(key).inserted
        lock.unlock()
        if first { print(message()) }
        return first
    }

    static func didWarn(_ key: String) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return warned.contains(key)
    }
}

import Foundation

/// Answers "idle" only once idle has *held*: every sample over at least
/// `minSamples` consecutive polls `pollMs` apart, and over the host's
/// `quiet_ms` when it sends one. The probe used to answer on the first
/// instantaneous 5 ms sample, which a run-loop turn between two bursts of
/// work satisfies. This is the off-main-thread analogue of EarlGrey's
/// "idle three times in a row" (`kConsecutiveTimesIdlingResourcesMustBeIdle`).
enum IdleWait {
    static let pollMs = 16
    static let minSamples = 3
    /// The longest hold a host may ask for.
    static let maxQuietMs = 1_000
    /// The longest wait a host may ask for: a client-supplied timeout used
    /// to park a probe thread for as long as it liked.
    static let maxTimeoutMs = 60_000

    struct Outcome {
        let idle: Bool
        let waitedMs: Int
    }

    static func wait(
        timeoutMs: Int,
        quietMs: Int?,
        pollMs: Int = IdleWait.pollMs,
        isIdle: () -> Bool
    ) -> Outcome {
        let timeoutMs = min(max(timeoutMs, 0), maxTimeoutMs)
        let holdMs = max(min(quietMs ?? 0, maxQuietMs), pollMs * (minSamples - 1))
        let start = DispatchTime.now()
        func elapsedMs() -> Int {
            Int((DispatchTime.now().uptimeNanoseconds - start.uptimeNanoseconds) / 1_000_000)
        }
        var idleSince: Int?
        var samples = 0
        while true {
            let now = elapsedMs()
            if isIdle() {
                if idleSince == nil { idleSince = now }
                samples += 1
                if samples >= minSamples, now - idleSince! >= holdMs {
                    return Outcome(idle: true, waitedMs: now)
                }
            } else {
                idleSince = nil
                samples = 0
            }
            if now >= timeoutMs {
                return Outcome(idle: false, waitedMs: now)
            }
            usleep(UInt32(max(1, pollMs)) * 1_000)
        }
    }
}

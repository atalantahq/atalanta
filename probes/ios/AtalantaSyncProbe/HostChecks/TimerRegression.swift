import Foundation

// Uses only API that exists before and after the P3 fix, so the same file
// shows the bug on the old TimerTracker: a debounce timer invalidated before
// it fires must not keep the app busy.
func timerRegression() -> Bool {
    TimerTracker(stateTracker: AppStateTracker.shared).install()
    let timer = Timer.scheduledTimer(withTimeInterval: 0.3, repeats: false) { _ in }
    let busyWhileScheduled = !AppStateTracker.shared.isIdle
    timer.invalidate()
    // Give the run loop a turn: the timer can never fire now.
    RunLoop.current.run(until: Date().addingTimeInterval(0.5))
    let idleAfterInvalidate = AppStateTracker.shared.isIdle
    print("timer regression: busy while scheduled=\(busyWhileScheduled), idle after invalidate=\(idleAfterInvalidate), pending=\(AppStateTracker.shared.pendingReasons)")
    return busyWhileScheduled && idleAfterInvalidate
}

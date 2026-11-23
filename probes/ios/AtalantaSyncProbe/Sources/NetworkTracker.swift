// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

import Foundation

/// Tracks in-flight URLSession data tasks to detect pending network requests.
final class NetworkTracker {
    private let stateTracker: AppStateTracker

    init(stateTracker: AppStateTracker) {
        self.stateTracker = stateTracker
    }

    func install() {
        // Swizzle URLSessionTask.resume to intercept network request starts
        Self.swizzle(
            cls: URLSessionTask.self,
            original: #selector(URLSessionTask.resume),
            replacement: #selector(URLSessionTask.atalanta_resume)
        )
    }

    /// Whether a task is a persistent connection that must not hold the
    /// app busy: a WebSocket, or a URL on the blocklist.
    static func isIgnored(_ task: URLSessionTask) -> Bool {
        if #available(iOS 13.0, macOS 10.15, *), task is URLSessionWebSocketTask {
            return true
        }
        guard let url = task.originalRequest?.url ?? task.currentRequest?.url else {
            return false
        }
        return URLBlocklist.shared.isIgnored(url.absoluteString)
    }

    static func taskDidStart() {
        AppStateTracker.shared.incrementNetwork()
    }

    static func taskDidFinish() {
        AppStateTracker.shared.decrementNetwork()
    }

    private static func swizzle(cls: AnyClass, original: Selector, replacement: Selector) {
        guard let originalMethod = class_getInstanceMethod(cls, original),
              let replacementMethod = class_getInstanceMethod(cls, replacement)
        else { return }
        method_exchangeImplementations(originalMethod, replacementMethod)
    }
}

extension URLSessionTask {
    @objc func atalanta_resume() {
        // resume() may legally be called more than once on the same task.
        // Track only the first call: repeat calls would over-increment the
        // pending counter (app reported busy forever) and replace the
        // associated observer, deallocating an object still registered for
        // KVO — an NSKVODeallocate crash inside the host app.
        if objc_getAssociatedObject(self, &TaskStateObserver.key) == nil,
           !NetworkTracker.isIgnored(self) {
            NetworkTracker.taskDidStart()

            // Observe task completion via KVO on state
            let observer = TaskStateObserver()
            objc_setAssociatedObject(
                self, &TaskStateObserver.key, observer, .OBJC_ASSOCIATION_RETAIN)
            addObserver(observer, forKeyPath: "state", options: [.new], context: nil)
        }

        atalanta_resume() // calls original (swizzled)
    }
}

private class TaskStateObserver: NSObject {
    static var key: UInt8 = 0
    private var didFinish = false

    override func observeValue(
        forKeyPath keyPath: String?,
        of object: Any?,
        change: [NSKeyValueChangeKey: Any]?,
        context: UnsafeMutableRawPointer?
    ) {
        guard keyPath == "state",
              let task = object as? URLSessionTask,
              !didFinish
        else { return }

        if task.state == .completed || task.state == .canceling {
            didFinish = true
            NetworkTracker.taskDidFinish()
            task.removeObserver(self, forKeyPath: "state")
        }
    }
}

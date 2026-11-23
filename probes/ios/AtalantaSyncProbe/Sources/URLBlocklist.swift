import Foundation

/// URLs whose requests never count as pending work. A WebSocket or a
/// long-poll stays open for the life of the screen, so counting it kept the
/// app "busy" on every step and each native wait ran to the settle cap.
/// EarlGrey (`kGREYConfigKeyBlockedURLRegex`) and DetoxSync (`URLBlacklist`)
/// both ship the same escape hatch.
public final class URLBlocklist {
    public static let shared = URLBlocklist()

    /// Persistent-connection patterns ignored by default (regular
    /// expressions, case-insensitive, matched anywhere in the URL).
    public static let defaultPatterns: [String] = [
        "^wss?://",
        "/socket\\.io/",
        "/sockjs(-node)?/",
        "/signalr/",
        "[?&]transport=(polling|longpoll|websocket)",
        "long[-_]?poll",
        "/cable$",
    ]

    private let lock = NSLock()
    private var patterns: [NSRegularExpression] = []

    public init(patterns: [String] = URLBlocklist.defaultPatterns) {
        _ = set(patterns)
    }

    /// Replace the list. Returns the patterns that failed to compile.
    @discardableResult
    public func set(_ newPatterns: [String]) -> [String] {
        let (compiled, bad) = Self.compile(newPatterns)
        lock.lock()
        patterns = compiled
        lock.unlock()
        return bad
    }

    /// Add patterns to the list. Returns the ones that failed to compile.
    /// A pattern already on the list is not added again (the host sends
    /// the suite's list on every connection).
    @discardableResult
    public func add(_ more: [String]) -> [String] {
        let (compiled, bad) = Self.compile(more)
        lock.lock()
        for re in compiled where !patterns.contains(where: { $0.pattern == re.pattern }) {
            patterns.append(re)
        }
        lock.unlock()
        return bad
    }

    /// How many patterns are on the list.
    public var count: Int {
        lock.lock()
        defer { lock.unlock() }
        return patterns.count
    }

    public func isIgnored(_ url: String) -> Bool {
        lock.lock()
        let current = patterns
        lock.unlock()
        let range = NSRange(url.startIndex..., in: url)
        return current.contains { $0.firstMatch(in: url, range: range) != nil }
    }

    private static func compile(_ list: [String]) -> ([NSRegularExpression], [String]) {
        var compiled: [NSRegularExpression] = []
        var bad: [String] = []
        for pattern in list {
            if let re = try? NSRegularExpression(pattern: pattern, options: [.caseInsensitive]) {
                compiled.append(re)
            } else {
                bad.append(pattern)
            }
        }
        return (compiled, bad)
    }
}

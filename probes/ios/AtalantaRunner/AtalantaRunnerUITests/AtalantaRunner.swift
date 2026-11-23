// Atalanta's own iOS runner: an XCUITest bundle that serves the subset of
// WebDriverAgent's HTTP API Atalanta's client uses, over Network.framework,
// with public XCTest APIs only. Atalanta's WDA client and iOS driver work
// against it unchanged (`ATALANTA_IOS_RUNNER=atalanta`).
//
// Why: WDA is a large Objective-C project (a 31 s build, private-API
// dependencies that break across Xcode releases). This builds in seconds and
// does only what Atalanta needs.
//
// Routes (JSON bodies, WDA's `{ "value": … }` envelope):
//   GET  /status
//   POST /session                       { capabilities.alwaysMatch.{bundleId, environment} }
//   GET|DELETE /session/:id
//   GET  [/session/:id]/source           XML in WDA's format
//   POST /session/:id/appium/settings    defaultActiveApplication, snapshotMaxDepth are honoured
//   POST /session/:id/actions            W3C pointer taps / double taps / long press
//   POST /session/:id/wda/dragfromtoforduration
//   POST /session/:id/wda/keys           { value: [chars] }
//   POST /session/:id/wda/pressButton    { name: home }
//   GET  [/session/:id]/screenshot       base64 PNG
//   GET  /session/:id/window/size
//   POST /session/:id/wda/apps/launch | terminate   { bundleId[, environment] }
//   POST [/session/:id]/wda/apps/state  { bundleId } → XCUIApplication.State (1…4)
//   POST /session/:id/url                { url }
//   POST /session/:id/element(s)         not supported: empty / no such element
//   GET  /wda/screen                     statusBarSize (SpringBoard's status bar)
//   POST /session/:id/atalanta/hitTest   { x, y } → what is drawn there (D-38, private API)
//
// Any XCTest issue an action records (typeText with nothing focused, a tap
// that can't be synthesised, a launch that fails) turns that request's
// answer into a 500 `unknown error` naming the issues: it used to be a 200.
//
// Every request, `/status` included, must carry `Authorization: Bearer
// <ATALANTA_RUNNER_TOKEN>` (a random token Atalanta makes per launch), a
// `Host` naming this runner on loopback (`127.0.0.1`, `localhost` or `[::1]`
// with its own port), and no `Origin` or cross-site `Sec-Fetch-Site`: a web
// page in a browser on the Mac (or, through the usbmux forward, a phone's)
// can neither send it blind nor read answers through DNS rebinding (R-1).

import Foundation
import Network
import UIKit
import XCTest

final class AtalantaRunnerUITests: XCTestCase {
    /// XCTest failures land here instead of failing the test: the request
    /// that caused them answers 500 with their text (N1: a typeText with
    /// nothing focused answered 200). Not `super`, so the runner never
    /// "fails" and keeps serving.
    override func record(_ issue: XCTIssue) {
        Issues.add(issue.compactDescription)
    }

    func testRunner() throws {
        // An XCTest failure inside a handler (typeText with no focus) must
        // not end the test method, and with it the runner (IOS-N6).
        continueAfterFailure = true
        let env = ProcessInfo.processInfo.environment
        let port = UInt16(env["USE_PORT"] ?? env["ATALANTA_RUNNER_PORT"] ?? "8100") ?? 8100
        // R-1: no token, no runner. Atalanta always passes one.
        guard Auth.token != nil else {
            print("AtalantaRunner: ATALANTA_RUNNER_TOKEN is not set; refusing to serve without a token")
            fflush(stdout)
            exit(1)
        }
        Guard.port = port
        Guard.boundHost = env["USE_IP"]
        if env["ATALANTA_RUNNER_WAIT_FOR_IDLE"] != "1" {
            print("AtalantaRunner: XCTest idle waits \(Quiescence.disable() ? "off" : "on (hook not found)")")
        }
        // `USE_IP` (WDA's variable) binds one address: Atalanta passes
        // 127.0.0.1 on simulators, which share the Mac's network (VEL-152).
        let server = try HTTPServer(port: port, host: env["USE_IP"], router: Router())
        // Prints the ready line once the port is bound, or exits (VEL-303).
        server.start()
        // XCTest UI calls must run on the main thread: requests are handled
        // on the main queue, which this run loop drains.
        while true {
            _ = RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(1))
        }
    }
}

// MARK: - Issues

/// The XCTest issues recorded while the current request ran. Requests run
/// one at a time on the main thread; the lock covers an issue XCTest
/// records from another thread.
enum Issues {
    private static let lock = NSLock()
    private static var list: [String] = []

    static func add(_ s: String) {
        lock.lock()
        list.append(s)
        lock.unlock()
    }

    /// The issues so far, emptying the list.
    static func take() -> [String] {
        lock.lock()
        defer { lock.unlock() }
        let out = list
        list.removeAll()
        return out
    }

    /// `response`, or a 500 naming `issues` when there are any.
    static func answer(_ response: Response, issues: [String]) -> Response {
        guard !issues.isEmpty else { return response }
        return .error(500, "unknown error", issues.joined(separator: "; "))
    }
}

// MARK: - Quiescence

/// XCTest waits for the app to go idle before and after every launch, tap
/// and query (≈1 s a tap on an animated screen). Atalanta runs its own
/// settle, so turn that wait into a no-op — the one private symbol this
/// runner touches (WebDriverAgent does the same, as its
/// `waitForIdleTimeout = 0` setting). If the method isn't there, nothing
/// changes and XCTest keeps waiting.
enum Quiescence {
    static func disable() -> Bool {
        guard let cls = NSClassFromString("XCUIApplicationProcess") else { return false }
        let one: @convention(block) (AnyObject, Bool) -> Void = { _, _ in }
        let two: @convention(block) (AnyObject, Bool, Bool) -> Void = { _, _, _ in }
        if let m = class_getInstanceMethod(cls, NSSelectorFromString("waitForQuiescenceIncludingAnimationsIdle:")) {
            method_setImplementation(m, imp_implementationWithBlock(one))
            return true
        }
        if let m = class_getInstanceMethod(cls, NSSelectorFromString("waitForQuiescenceIncludingAnimationsIdle:isPreEvent:")) {
            method_setImplementation(m, imp_implementationWithBlock(two))
            return true
        }
        return false
    }
}

// MARK: - Routing

/// The build Atalanta launched (`atalanta-runner-<source hash>`), so it can
/// tell this runner from a stale one still listening on the port.
let runnerBuild = ProcessInfo.processInfo.environment["ATALANTA_RUNNER_BUILD"] ?? "atalanta-runner-1"

final class Router {
    private var sessionId: String? {
        didSet {
            statusLock.lock()
            statusSession = sessionId
            statusLock.unlock()
        }
    }
    /// `sessionId` as `/status` reports it, readable off the main thread.
    private let statusLock = NSLock()
    private var statusSession: String?
    private var activeBundle: String?
    /// Installed apps, sent by Atalanta for sessions without an app (the
    /// inspector, the REPL before `launchApp`): whichever is in front is
    /// the screen, as WDA's private frontmost-app lookup would say.
    private var candidates: [String] = []
    private var lastFront: String?
    /// A full candidate scan costs one state query per app: at most this often.
    private var nextScan = Date.distantPast
    private let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
    /// When SpringBoard last had no alert (checking costs a snapshot).
    private var alertCheck = AlertCheck()

    private func activeApp() -> XCUIApplication {
        for b in [activeBundle, lastFront].compactMap({ $0 }) {
            let app = XCUIApplication(bundleIdentifier: b)
            if app.state == .runningForeground {
                return app
            }
        }
        if !candidates.isEmpty, Date() >= nextScan {
            nextScan = Date().addingTimeInterval(2)
            for b in candidates where b != activeBundle && b != lastFront {
                let app = XCUIApplication(bundleIdentifier: b)
                if app.state == .runningForeground {
                    lastFront = b
                    nextScan = .distantPast
                    return app
                }
            }
        }
        return springboard
    }

    /// `/status`. Safe on any thread: the server answers it on its own
    /// queue, so a health check never waits behind a slow handler on the
    /// main thread (VEL-306: a launch made the runner look dead).
    func status() -> Response {
        statusLock.lock()
        let sid = statusSession
        statusLock.unlock()
        return .ok(["ready": true, "message": "AtalantaRunner ready",
                    "build": ["version": runnerBuild]], session: sid)
    }

    func handle(_ req: Request) -> Response {
        let parts = req.path.split(separator: "/").map(String.init)
        // Drop "/session/<id>" so session routes share the plain ones.
        var rest = parts
        if parts.first == "session", parts.count >= 2 {
            if parts.count == 2 {
                switch req.method {
                case "GET":
                    return parts[1] == sessionId
                        ? .ok(["sessionId": sessionId!, "capabilities": [String: Any]()])
                        : .error(404, "invalid session id", "no such session")
                case "DELETE":
                    sessionId = nil
                    return .ok(NSNull())
                default: break
                }
            }
            rest = Array(parts.dropFirst(2))
        }
        let route = rest.joined(separator: "/")
        switch (req.method, route) {
        case ("GET", "status"):
            return status()
        case ("POST", "session") where parts.count == 1:
            let caps = (req.json["capabilities"] as? [String: Any])?["alwaysMatch"] as? [String: Any]
            if let bundle = caps?["bundleId"] as? String, !bundle.isEmpty {
                activeBundle = bundle
                let app = XCUIApplication(bundleIdentifier: bundle)
                // WDA's `environment` capability: the host's per-simulator
                // ATALANTA_SYNC_PORT for the in-app sync probe.
                if let env = caps?["environment"] as? [String: String] { app.launchEnvironment = env }
                app.launch()
            }
            let id = UUID().uuidString
            sessionId = id
            return .ok(["sessionId": id, "capabilities": [String: Any]()], session: id)
        case ("GET", "source"):
            // A failed snapshot is an error, never an empty screen (IOS-N1):
            // the host would read "nothing here" and pass assertNotVisible.
            do {
                return .ok(try Source.xml(of: activeApp(), springboard: springboard, alerts: &alertCheck))
            } catch {
                return .error(500, "unknown error", "snapshot failed: \(error)")
            }
        case ("GET", "wda/screen"):
            // SpringBoard owns the status bar; zero when it is hidden.
            let bar = springboard.statusBars.firstMatch
            let size = bar.exists ? bar.frame.size : .zero
            return .ok(["statusBarSize": ["width": Source.int(size.width), "height": Source.int(size.height)]])
        case ("POST", "appium/settings"):
            if let s = req.json["settings"] as? [String: Any],
               let b = s["defaultActiveApplication"] as? String, !b.isEmpty {
                activeBundle = b
            }
            if let s = req.json["settings"] as? [String: Any],
               let d = Self.number(s["snapshotMaxDepth"]), d >= 1, d <= 1_000 {
                SnapshotDepth.fallback = Int(d)
            }
            if let s = req.json["settings"] as? [String: Any],
               let c = s["atalantaCandidates"] as? [String] {
                candidates = c
                nextScan = .distantPast
            }
            return .ok([String: Any]())
        case ("POST", "actions"):
            return pointerActions(req.json)
        case ("POST", "wda/dragfromtoforduration"):
            // Missing coordinates used to drag from (0, 0) (VEL-307).
            guard ["fromX", "fromY", "toX", "toY"].allSatisfy({ Self.number(req.json[$0]) != nil }) else {
                return .error(400, "invalid argument", "fromX, fromY, toX and toY are required numbers")
            }
            let app = activeApp()
            let from = point(app, req.json["fromX"], req.json["fromY"])
            let to = point(app, req.json["toX"], req.json["toY"])
            let duration = (req.json["duration"] as? Double) ?? 0
            if req.json["fromX"] as? Double == req.json["toX"] as? Double,
               req.json["fromY"] as? Double == req.json["toY"] as? Double {
                duration > 0 ? from.press(forDuration: duration) : from.tap()
            } else {
                from.press(forDuration: 0.05, thenDragTo: to)
            }
            return .ok(NSNull())
        case ("POST", "wda/keys"):
            let chars = (req.json["value"] as? [String]) ?? []
            activeApp().typeText(chars.joined())
            return .ok(NSNull())
        case ("POST", "wda/setPasteboard"):
            // WDA's route: base64 plain text onto the device's pasteboard.
            let b64 = (req.json["content"] as? String) ?? ""
            UIPasteboard.general.string = Data(base64Encoded: b64).flatMap {
                String(data: $0, encoding: .utf8)
            } ?? ""
            return .ok(NSNull())
        case ("POST", "wda/getPasteboard"):
            let text = UIPasteboard.general.string ?? ""
            return .ok(Data(text.utf8).base64EncodedString())
        case ("POST", "wda/element/0/keyboardInput"):
            // WDA's route, for the active app: keys with modifier flags
            // (Atalanta sends Cmd+V to paste).
            for item in (req.json["keys"] as? [Any]) ?? [] {
                if let k = item as? String {
                    // WDA's key names; Atalanta uses F13 as a no-op.
                    let key = k == "XCUIKeyboardKeyF13" ? XCUIKeyboardKey.F13.rawValue : k
                    activeApp().typeKey(key, modifierFlags: [])
                } else if let d = item as? [String: Any], let k = d["key"] as? String {
                    // Clamped: a negative value trapped the runner (VEL-304).
                    let flags = XCUIElement.KeyModifierFlags(
                        rawValue: UInt(clamping: (d["modifierFlags"] as? Int) ?? 0))
                    activeApp().typeKey(k, modifierFlags: flags)
                }
            }
            return .ok(NSNull())
        case ("POST", "wda/pressButton"):
            switch req.json["name"] as? String {
            case "home": XCUIDevice.shared.press(.home)
            default: return .error(400, "invalid argument", "only home exists on a simulator")
            }
            return .ok(NSNull())
        case ("GET", "screenshot"):
            let png = XCUIScreen.main.screenshot().pngRepresentation
            return .ok(png.base64EncodedString())
        case ("GET", "window/size"):
            let f = activeApp().frame
            return .ok(["width": f.width, "height": f.height])
        case ("POST", "wda/apps/launch"):
            guard let b = req.json["bundleId"] as? String else {
                return .error(400, "invalid argument", "bundleId required")
            }
            activeBundle = b
            // Like WDA: bring a running app forward; only start a stopped
            // one. (`launch()` always kills and cold-starts it: ~3 s.)
            let app = XCUIApplication(bundleIdentifier: b)
            if let env = req.json["environment"] as? [String: String] { app.launchEnvironment = env }
            switch app.state {
            case .runningForeground: break
            case .runningBackground, .runningBackgroundSuspended: app.activate()
            default: app.launch()
            }
            return .ok(NSNull())
        case ("POST", "wda/apps/state"):
            // WDA's route and values (1 not running … 4 foreground): the
            // host's liveness check on devices, which have no process table.
            guard let b = req.json["bundleId"] as? String, !b.isEmpty else {
                return .error(400, "invalid argument", "bundleId required")
            }
            return .ok(Int(XCUIApplication(bundleIdentifier: b).state.rawValue))
        case ("POST", "wda/apps/terminate"):
            if let b = req.json["bundleId"] as? String {
                XCUIApplication(bundleIdentifier: b).terminate()
            }
            return .ok(true)
        case ("POST", "url"):
            guard let s = req.json["url"] as? String, let url = URL(string: s) else {
                return .error(400, "invalid argument", "url required")
            }
            if #available(iOS 16.4, *) {
                XCUIDevice.shared.system.open(url)
                return .ok(NSNull())
            }
            return .error(500, "unsupported operation", "opening URLs needs iOS 16.4+")
        case ("POST", "atalanta/hitTest"):
            // D-38 (ios.verifyTap): what is drawn at a point. Opt-in on the
            // host; private API, so "unavailable" is an answer, not an error.
            guard let x = Self.number(req.json["x"]), let y = Self.number(req.json["y"]) else {
                return .error(400, "invalid argument", "x and y are required numbers")
            }
            return .ok(HitTest.at(CGPoint(x: x, y: y), app: activeApp(), springboard: springboard))
        case ("POST", "elements"):
            return .ok([Any]())
        case ("POST", "element"):
            return .error(404, "no such element", "AtalantaRunner matches from /source")
        default:
            return .error(404, "unknown command", "\(req.method) \(req.path)")
        }
    }

    /// A finite JSON number.
    static func number(_ v: Any?) -> Double? {
        guard let d = (v as? Double) ?? (v as? Int).map(Double.init), d.isFinite else { return nil }
        return d
    }

    private func point(_ app: XCUIApplication, _ x: Any?, _ y: Any?) -> XCUICoordinate {
        let dx = Self.number(x) ?? 0
        let dy = Self.number(y) ?? 0
        return app.coordinate(withNormalizedOffset: .zero).withOffset(CGVector(dx: dx, dy: dy))
    }

    /// W3C pointer actions as Atalanta sends them: move, then one or more
    /// down/pause/up presses.
    private func pointerActions(_ body: [String: Any]) -> Response {
        guard let seq = (body["actions"] as? [[String: Any]])?.first,
              let steps = seq["actions"] as? [[String: Any]] else {
            return .error(400, "invalid argument", "actions required")
        }
        let app = activeApp()
        var at: XCUICoordinate?
        var atXY: (Double, Double)?
        var downAt: XCUICoordinate?
        var downXY: (Double, Double)?
        var downs = 0
        var held = 0.0
        var isDown = false
        // The stroke while the finger is down: its length, its time, and the
        // last segment's speed (a slow last segment means "stop there").
        var moved = 0.0
        var moveSecs = 0.0
        var lastSpeed = 0.0
        var firstSpeed = 0.0
        for s in steps {
            switch s["type"] as? String {
            case "pointerMove":
                let x = (s["x"] as? Double) ?? 0, y = (s["y"] as? Double) ?? 0
                let secs = ((s["duration"] as? Double) ?? 0) / 1000
                if isDown, let (px, py) = atXY {
                    let d = ((x - px) * (x - px) + (y - py) * (y - py)).squareRoot()
                    moved += d
                    moveSecs += secs
                    let speed = secs > 0 ? d / secs : 10_000
                    if firstSpeed == 0 { firstSpeed = speed }
                    lastSpeed = speed
                }
                at = point(app, s["x"], s["y"])
                atXY = (x, y)
            case "pointerDown":
                downs += 1
                isDown = true
                downAt = at
                downXY = atXY
            case "pause":
                if isDown { held += ((s["duration"] as? Double) ?? 0) / 1000 }
            case "pointerUp":
                isDown = false
            default: break
            }
        }
        guard let target = at else { return .error(400, "invalid argument", "no pointerMove") }
        // A stroke (swipe or scroll): it used to keep only the last point and
        // tap there, so every swipe and scroll through /actions was a tap.
        if let start = downAt, downXY != nil, moved > 10 {
            let atalanta = moveSecs > 0 ? max(200, moved / moveSecs) : 1_500
            // Ending slowly (Atalanta's scroll stroke) = hold before lifting,
            // so the content stops where the finger does; a swipe flings.
            let hold = (firstSpeed > 0 && lastSpeed < firstSpeed / 3) ? 0.1 : 0
            start.press(forDuration: 0.05,
                        thenDragTo: target,
                        withAtalanta: XCUIGestureAtalanta(atalanta),
                        thenHoldForDuration: hold)
            return .ok(NSNull())
        }
        if downs >= 2 {
            target.doubleTap()
        } else if held >= 0.5 {
            target.press(forDuration: held)
        } else {
            target.tap()
        }
        return .ok(NSNull())
    }
}

// MARK: - Source

/// Throttles the SpringBoard alert check: a "no alert" answer is reused
/// for a second.
struct AlertCheck {
    var clearUntil = Date.distantPast
}

enum Source {
    static let typeNames: [String] = [
        "Any", "Other", "Application", "Group", "Window", "Sheet", "Drawer", "Alert", "Dialog",
        "Button", "RadioButton", "RadioGroup", "CheckBox", "DisclosureTriangle", "PopUpButton",
        "ComboBox", "MenuButton", "ToolbarButton", "Popover", "Keyboard", "Key", "NavigationBar",
        "TabBar", "TabGroup", "Toolbar", "StatusBar", "Table", "TableRow", "TableColumn", "Outline",
        "OutlineRow", "Browser", "CollectionView", "Slider", "PageIndicator", "ProgressIndicator",
        "ActivityIndicator", "SegmentedControl", "Picker", "PickerWheel", "Switch", "Toggle", "Link",
        "Image", "Icon", "SearchField", "ScrollView", "ScrollBar", "StaticText", "TextField",
        "SecureTextField", "DatePicker", "TextView", "Menu", "MenuItem", "MenuBar", "MenuBarItem",
        "Map", "WebView", "IncrementArrow", "DecrementArrow", "Timeline", "RatingIndicator",
        "ValueIndicator", "SplitGroup", "Splitter", "RelevanceIndicator", "ColorWell", "HelpTag",
        "Matte", "DockItem", "Ruler", "RulerMarker", "Grid", "LevelIndicator", "Cell", "LayoutArea",
        "LayoutItem", "Handle", "Stepper", "Tab", "TouchBar", "StatusItem",
    ]

    static func xml(of app: XCUIApplication, springboard: XCUIApplication, alerts: inout AlertCheck) throws -> String {
        var out = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"
        // System alerts (permissions) live in SpringBoard, not the app.
        var extra: [XCUIElementSnapshot] = []
        if app != springboard, Date() >= alerts.clearUntil {
            if springboard.alerts.firstMatch.exists,
               let alert = try? springboard.alerts.firstMatch.snapshot() {
                extra.append(alert)
            } else {
                alerts.clearUntil = Date().addingTimeInterval(1)
            }
        }
        let snap: XCUIElementSnapshot
        do {
            snap = try app.snapshot()
        } catch where SnapshotDepth.tooDeep(error) {
            try partial(of: app, extra: extra, into: &out, after: error)
            return out
        }
        write(snap, into: &out, extra: extra, appFrame: snap.frame)
        return out
    }

    /// A tree too deep for one AX snapshot (kAXErrorIllegalArgument on big
    /// screens): the front window again with XCTest's snapshot depth capped
    /// at `SnapshotDepth.fallback`, plus the keyboard and alerts read on
    /// their own, under an Application root marked `truncated="true"`
    /// (Maestro's recovery). Throws rather than answer an empty tree.
    static func partial(of app: XCUIApplication, extra: [XCUIElementSnapshot],
                        into out: inout String, after error: Error) throws {
        guard SnapshotDepth.install() else { throw error }
        SnapshotDepth.cap = SnapshotDepth.fallback
        defer { SnapshotDepth.cap = nil }
        let window = try app.windows.firstMatch.snapshot()
        var parts = [window]
        for q in [app.keyboards.firstMatch, app.alerts.firstMatch] where q.exists {
            if let s = try? q.snapshot() { parts.append(s) }
        }
        let f = app.frame
        let type = "XCUIElementTypeApplication"
        out += "<\(type) type=\"\(type)\""
        if !app.label.isEmpty { out += " name=\"\(esc(app.label))\" label=\"\(esc(app.label))\"" }
        out += " enabled=\"true\" visible=\"true\" truncated=\"true\""
        out += " x=\"\(int(f.origin.x))\" y=\"\(int(f.origin.y))\" width=\"\(int(f.width))\" height=\"\(int(f.height))\">"
        for p in parts + extra { write(p, into: &out, appFrame: f) }
        out += "</\(type)>"
    }

    static func write(_ s: XCUIElementSnapshot, into out: inout String, extra: [XCUIElementSnapshot] = [],
                      appFrame: CGRect) {
        let raw = Int(s.elementType.rawValue)
        let type = "XCUIElementType" + (raw < typeNames.count ? typeNames[raw] : "Other")
        let f = s.frame
        let name = s.identifier.isEmpty ? s.label : s.identifier
        out += "<\(type) type=\"\(type)\""
        if !name.isEmpty { out += " name=\"\(esc(name))\"" }
        if !s.label.isEmpty { out += " label=\"\(esc(s.label))\"" }
        if let v = s.value {
            let text = (v as? String) ?? "\(v)"
            if !text.isEmpty { out += " value=\"\(esc(text))\"" }
        }
        // An empty field reports its placeholder as its value: the host
        // clears it when the two match (VEL-354; this path never sent it).
        if let p = s.placeholderValue, !p.isEmpty { out += " placeholderValue=\"\(esc(p))\"" }
        out += " enabled=\"\(s.isEnabled)\""
        // WDA's cheap visibility: a frame with an area, on the app's frame.
        out += " visible=\"\(visible(f, in: appFrame))\""
        if s.isSelected { out += " selected=\"true\"" }
        if let o = s as? NSObject {
            // Private snapshot attributes, read only where they exist
            // (WDA's FBXCElementSnapshot, Maestro's ViewHierarchyHandler).
            if o.responds(to: NSSelectorFromString("hasKeyboardFocus")),
               (o.value(forKey: "hasKeyboardFocus") as? Bool) == true {
                out += " focused=\"true\""
            }
            if o.responds(to: NSSelectorFromString("visibleFrame")),
               let r = (o.value(forKey: "visibleFrame") as? NSValue)?.cgRectValue,
               r.origin.x.isFinite, r.origin.y.isFinite, r.width.isFinite, r.height.isFinite,
               !r.isEmpty, r != f {
                out += " visibleFrame=\"\(int(r.origin.x)),\(int(r.origin.y)),\(int(r.width)),\(int(r.height))\""
            }
        }
        out += " x=\"\(int(f.origin.x))\" y=\"\(int(f.origin.y))\" width=\"\(int(f.width))\" height=\"\(int(f.height))\""
        let children = s.children + extra
        if children.isEmpty {
            out += "/>"
            return
        }
        out += ">"
        for c in children { write(c, into: &out, appFrame: appFrame) }
        out += "</\(type)>"
    }

    /// A frame with an area that overlaps the app's (WDA's cheap test, as
    /// the snapshot has no `isVisible`). An unknown app frame hides nothing.
    static func visible(_ f: CGRect, in app: CGRect) -> Bool {
        guard f.origin.x.isFinite, f.origin.y.isFinite, f.width.isFinite, f.height.isFinite,
              f.width > 0, f.height > 0 else { return false }
        guard app.width > 0, app.height > 0, app.origin.x.isFinite, app.origin.y.isFinite else { return true }
        return f.intersects(app)
    }

    /// A frame value as an integer: `Int(.infinity)` traps (VEL-153), and
    /// an off-screen element's frame can be infinite.
    static func int(_ v: CGFloat) -> Int {
        v.isFinite ? Int(max(min(v, 1e7), -1e7).rounded()) : 0
    }

    static func esc(_ s: String) -> String {
        var r = ""
        r.reserveCapacity(s.count)
        for c in s.unicodeScalars {
            switch c {
            case "&": r += "&amp;"
            case "<": r += "&lt;"
            case ">": r += "&gt;"
            case "\"": r += "&quot;"
            case "\n": r += "&#10;"
            default:
                if c.value >= 0x20 || c == "\t" { r.unicodeScalars.append(c) }
            }
        }
        return r
    }
}

// MARK: - Hit test (D-38)

/// What is drawn at a point, for `ios.verifyTap` and the status-bar rule.
///
/// XCTest's own hit test, `_XCT_requestElementAtPoint:reply:` on the test
/// runner's daemon proxy (WDA's `FBXCTestDaemonsProxy`), says which
/// process owns the point: SpringBoard (status bar, system alert) or the
/// app. Private: every selector is checked with `respondsToSelector:`, and
/// when one is missing the answer is `available: false` and the host keeps
/// its frame-only rule. The AX hit result is "the element VoiceOver would
/// focus", often a container, so like idb (`AccessibilityService`) the
/// answer descends from the owner's snapshot to the deepest element whose
/// frame holds the point, the later sibling winning (drawn on top).
enum HitTest {
    static func at(_ point: CGPoint, app: XCUIApplication, springboard: XCUIApplication) -> [String: Any] {
        guard point.x.isFinite, point.y.isFinite else {
            return ["available": false, "reason": "the point is not finite"]
        }
        let pid: Int32
        let axHit: NSObject
        switch ownerPid(at: point) {
        case .success(let p): (pid, axHit) = p
        case .failure(let why): return ["available": false, "reason": why.text]
        }
        let owner: XCUIApplication
        let ownerName: String
        if let sb = processId(of: springboard), sb == pid {
            owner = springboard
            ownerName = "springboard"
        } else if let ap = processId(of: app), ap == pid {
            owner = app
            ownerName = "app"
        } else if processId(of: app) == nil {
            // The app's pid can't be read: the point is the app's unless it
            // is SpringBoard's.
            owner = app
            ownerName = "app"
        } else {
            return ["available": true, "owner": "other", "pid": Int(pid)]
        }
        var out: [String: Any] = ["available": true, "owner": ownerName, "pid": Int(pid)]
        if let snap = try? owner.snapshot() {
            // fi56: the element XCTest's hit test returned, found in the
            // snapshot by its accessibility element, decides; its path and
            // chain let the host place it in its own tree.
            if let chain = find(axHit, in: snap) {
                out["hit"] = describe(chain)
            }
            if let chain = deepest(snap, at: point) {
                out["element"] = describe(chain)
            }
        }
        return out
    }

    /// One node: type, identifier, label, frame, and the touch flags the
    /// snapshot exposes (private; read only where they exist).
    static func node(_ s: XCUIElementSnapshot) -> [String: Any] {
        let f = s.frame
        let raw = Int(s.elementType.rawValue)
        var d: [String: Any] = [
            "type": raw < Source.typeNames.count ? Source.typeNames[raw] : "Other",
            "identifier": s.identifier,
            "label": s.label,
            "x": Source.int(f.origin.x), "y": Source.int(f.origin.y),
            "width": Source.int(f.width), "height": Source.int(f.height),
        ]
        if let o = s as? NSObject {
            for (key, sel) in [("accessible", "isAccessibilityElement"),
                               ("interactive", "isUserInteractionEnabled"),
                               ("hittable", "isHittable")] {
                if o.responds(to: NSSelectorFromString(sel)),
                   let b = o.value(forKey: sel) as? Bool {
                    d[key] = b
                }
            }
        }
        return d
    }

    /// The last node of `chain` with its `path` (child indices from the
    /// root) and `chain` (root first).
    static func describe(_ chain: [(XCUIElementSnapshot, Int)]) -> [String: Any] {
        var d = node(chain[chain.count - 1].0)
        d["path"] = chain.dropFirst().map { $0.1 }
        d["chain"] = chain.map { node($0.0) }
        return d
    }

    /// The snapshot node whose accessibility element is `ax` (XCTest's
    /// private `accessibilityElement`, compared with `isEqual:`), with its
    /// ancestors; nil when the snapshot doesn't expose it or holds no such
    /// node.
    static func find(_ ax: NSObject, in root: XCUIElementSnapshot) -> [(XCUIElementSnapshot, Int)]? {
        let sel = NSSelectorFromString("accessibilityElement")
        func walk(_ s: XCUIElementSnapshot, _ index: Int, _ above: [(XCUIElementSnapshot, Int)]) -> [(XCUIElementSnapshot, Int)]? {
            let here = above + [(s, index)]
            if let o = s as? NSObject, o.responds(to: sel),
               let mine = o.value(forKey: "accessibilityElement") as? NSObject, mine.isEqual(ax) {
                return here
            }
            for (i, c) in s.children.enumerated() {
                if let hit = walk(c, i, here) { return hit }
            }
            return nil
        }
        return walk(root, 0, [])
    }

    struct Why: Error { let text: String }

    /// The pid of the process XCTest's hit test finds at `point`, and the
    /// accessibility element it returned.
    static func ownerPid(at point: CGPoint) -> Result<(Int32, NSObject), Why> {
        guard let cls = NSClassFromString("XCTRunnerDaemonSession") as? NSObject.Type else {
            return .failure(Why(text: "XCTRunnerDaemonSession is missing"))
        }
        let shared = NSSelectorFromString("sharedSession")
        guard cls.responds(to: shared),
              let session = cls.perform(shared)?.takeUnretainedValue() as? NSObject else {
            return .failure(Why(text: "XCTRunnerDaemonSession.sharedSession is missing"))
        }
        let proxySel = NSSelectorFromString("daemonProxy")
        guard session.responds(to: proxySel),
              let proxy = session.perform(proxySel)?.takeUnretainedValue() as? NSObjectProtocol else {
            return .failure(Why(text: "the daemon proxy is missing"))
        }
        let sel = NSSelectorFromString("_XCT_requestElementAtPoint:reply:")
        guard proxy.responds(to: sel), let proxyClass = object_getClass(proxy) else {
            return .failure(Why(text: "_XCT_requestElementAtPoint:reply: is missing"))
        }
        typealias Reply = @convention(block) (AnyObject?, NSError?) -> Void
        typealias Request = @convention(c) (AnyObject, Selector, CGPoint, Reply) -> Void
        guard let imp = class_getMethodImplementation(proxyClass, sel) else {
            return .failure(Why(text: "_XCT_requestElementAtPoint:reply: has no implementation"))
        }
        let call = unsafeBitCast(imp, to: Request.self)
        let done = DispatchSemaphore(value: 0)
        let box = Box()
        let reply: Reply = { element, error in
            box.element = element
            box.error = error
            done.signal()
        }
        call(proxy, sel, point, reply)
        // The reply may come on XCTest's queue or on this (main) thread's
        // run loop: keep the run loop turning while waiting.
        let deadline = Date().addingTimeInterval(2)
        var answered = false
        while !answered, Date() < deadline {
            answered = done.wait(timeout: .now()) == .success
            if !answered { _ = RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.01)) }
        }
        guard answered else {
            return .failure(Why(text: "the hit test did not answer in 2 s"))
        }
        if let error = box.error {
            return .failure(Why(text: "the hit test failed: \(error.localizedDescription)"))
        }
        guard let element = box.element as? NSObject else {
            return .failure(Why(text: "the hit test found nothing"))
        }
        let pidSel = NSSelectorFromString("processIdentifier")
        guard element.responds(to: pidSel),
              let pid = (element.value(forKey: "processIdentifier") as? NSNumber)?.int32Value,
              pid > 0 else {
            return .failure(Why(text: "the hit element has no processIdentifier"))
        }
        return .success((pid, element))
    }

    final class Box {
        var element: AnyObject?
        var error: NSError?
    }

    /// `XCUIApplication.processID` (private; WDA reads it the same way).
    static func processId(of app: XCUIApplication) -> Int32? {
        guard app.responds(to: NSSelectorFromString("processID")),
              let n = app.value(forKey: "processID") as? NSNumber, n.int32Value > 0 else { return nil }
        return n.int32Value
    }

    /// The deepest node holding `point`, the last such sibling at each
    /// level, with its ancestors (and each one's index among its siblings).
    /// Only a node the snapshot says has user interaction off is passed
    /// through (an earlier sibling under it is tried, and it never answers
    /// itself). Not being an accessibility element or not "hittable" to
    /// XCTest is no such sign: a plain `UIView` dimming overlay is both, and
    /// it swallows the touch.
    static func deepest(_ s: XCUIElementSnapshot, at point: CGPoint, index: Int = 0,
                        above: [(XCUIElementSnapshot, Int)] = []) -> [(XCUIElementSnapshot, Int)]? {
        guard holds(s.frame, point) else { return nil }
        let here = above + [(s, index)]
        for (i, c) in s.children.enumerated().reversed() {
            if let hit = deepest(c, at: point, index: i, above: here) { return hit }
        }
        let flags = node(s)
        let passThrough = (flags["interactive"] as? Bool) == false
        return passThrough && !above.isEmpty ? nil : here
    }

    static func holds(_ f: CGRect, _ p: CGPoint) -> Bool {
        f.origin.x.isFinite && f.origin.y.isFinite && f.width.isFinite && f.height.isFinite
            && f.width > 0 && f.height > 0 && f.contains(p)
    }
}

// MARK: - Snapshot depth

/// XCTest's AX snapshot depth, capped only while recovering from a tree
/// too deep to snapshot at once (P7; Maestro's `AxClientSwizzler`). One more
/// private symbol, the same kind of risk as `Quiescence`: when it isn't
/// there, the recovery is skipped and the snapshot error stands.
enum SnapshotDepth {
    /// The depth used for the recovery read: 60 (Maestro's), or the
    /// session's `snapshotMaxDepth` setting (D-37: the host sends the same
    /// value to WDA, so both runners cut a deep tree at the same depth).
    static var fallback = 60
    /// The depth to impose, or nil for XCTest's own default.
    static var cap: Int?
    private static var installed = false

    /// The error a too-deep tree raises.
    static func tooDeep(_ error: Error) -> Bool {
        let text = "\(error) \((error as NSError).localizedDescription)"
        return text.contains("kAXErrorIllegalArgument") || text.contains("kAXErrorInvalidUIElement")
    }

    /// Route `XCAXClient_iOS.defaultParameters` through `cap`. Main thread.
    static func install() -> Bool {
        if installed { return true }
        guard let cls = NSClassFromString("XCAXClient_iOS") else { return false }
        let sel = NSSelectorFromString("defaultParameters")
        guard let m = class_getInstanceMethod(cls, sel) else { return false }
        typealias Original = @convention(c) (AnyObject, Selector) -> NSDictionary
        let original = unsafeBitCast(method_getImplementation(m), to: Original.self)
        let replaced: @convention(block) (AnyObject) -> NSDictionary = { me in
            let params = original(me, sel)
            guard let depth = SnapshotDepth.cap else { return params }
            let out = NSMutableDictionary(dictionary: params)
            out["maxDepth"] = depth
            return out
        }
        method_setImplementation(m, imp_implementationWithBlock(replaced))
        installed = true
        return true
    }
}

// MARK: - HTTP

struct Request {
    let method: String
    let path: String
    let json: [String: Any]
    /// The `Authorization` header, if any (the bearer token).
    var authorization: String? = nil
    /// `Host`, `Origin` and `Sec-Fetch-Site`, if sent (R-1).
    var host: String? = nil
    var origin: String? = nil
    var fetchSite: String? = nil
}

/// R-1: every request must carry `Authorization: Bearer <token>`, the
/// token Atalanta passes as `ATALANTA_RUNNER_TOKEN` (random per launch, or
/// `ios.runnerToken`). The runner doesn't start without one. Compared in
/// constant time.
enum Auth {
    static let token: String? = {
        let t = ProcessInfo.processInfo.environment["ATALANTA_RUNNER_TOKEN"] ?? ""
        return t.isEmpty ? nil : t
    }()

    static func allows(_ header: String?) -> Bool {
        guard let token else { return false }
        let want = Array("Bearer \(token)".utf8)
        let got = Array((header ?? "").utf8)
        var diff = UInt8(want.count == got.count ? 0 : 1)
        for i in 0..<want.count {
            diff |= want[i] ^ (i < got.count ? got[i] : 0)
        }
        return diff == 0
    }
}

/// R-1: what a browser sends and Atalanta never does. A page's blind
/// `fetch()` carries `Origin` (and `Sec-Fetch-Site: cross-site`); DNS
/// rebinding names the attacker's host in `Host`. Atalanta's client sends
/// `Host: 127.0.0.1:<port>`, through a usbmux forward the runner's own port.
enum Guard {
    /// The port this runner listens on.
    static var port: UInt16 = 8100
    /// `USE_IP`, when it binds one address: a request may name it too.
    static var boundHost: String?

    /// Why `req` is refused (403), or nil.
    static func refusal(_ req: Request) -> String? {
        if req.origin != nil {
            return "cross-origin requests are refused"
        }
        if let site = req.fetchSite?.lowercased(), site != "none", site != "same-origin" {
            return "cross-site requests are refused"
        }
        guard let host = req.host, allowed(host: host) else {
            return "Host must be this runner's loopback address and port"
        }
        return nil
    }

    /// `127.0.0.1`, `localhost` or `[::1]` (or `USE_IP`), with this
    /// runner's port.
    static func allowed(host: String) -> Bool {
        let h = host.lowercased()
        guard let colon = h.range(of: ":", options: .backwards),
              !h.hasSuffix("]"), UInt16(h[colon.upperBound...]) == port else { return false }
        let name = String(h[..<colon.lowerBound])
        var names = ["127.0.0.1", "localhost", "[::1]"]
        if let b = boundHost?.lowercased(), !b.isEmpty {
            names.append(b.contains(":") && !b.hasPrefix("[") ? "[\(b)]" : b)
        }
        return names.contains(name)
    }
}

struct Response {
    let status: Int
    let body: Data

    static func ok(_ value: Any, session: String? = nil) -> Response {
        var env: [String: Any] = ["value": value]
        if let s = session { env["sessionId"] = s }
        return Response(status: 200, body: (try? JSONSerialization.data(withJSONObject: env)) ?? Data())
    }

    static func error(_ status: Int, _ error: String, _ message: String) -> Response {
        let env: [String: Any] = ["value": ["error": error, "message": message]]
        return Response(status: status, body: (try? JSONSerialization.data(withJSONObject: env)) ?? Data())
    }
}

final class HTTPServer {
    private let listener: NWListener
    private let router: Router
    private let queue = DispatchQueue(label: "atalanta.runner.http")

    private let port: UInt16

    /// Listens on `host` only when given (`127.0.0.1`), else on every
    /// interface as before. Atalanta passes it on simulators, and on a
    /// cabled device only with `ATALANTA_IOS_DEVICE_LOOPBACK=1` until a wired
    /// smoke run shows usbmuxd delivers to the device's loopback (P5).
    init(port: UInt16, host: String?, router: Router) throws {
        let params = NWParameters.tcp
        params.allowLocalEndpointReuse = true
        let nwPort = NWEndpoint.Port(rawValue: port)!
        if let host, !host.isEmpty {
            params.requiredLocalEndpoint = .hostPort(host: NWEndpoint.Host(host), port: nwPort)
            listener = try NWListener(using: params)
        } else {
            listener = try NWListener(using: params, on: nwPort)
        }
        self.port = port
        self.router = router
    }

    func start() {
        listener.newConnectionHandler = { [weak self] conn in
            guard let self else { return }
            conn.start(queue: self.queue)
            self.receive(conn, buffer: Data())
        }
        // The ready line means the port is bound: it used to be printed
        // before, and a bind failure left Atalanta talking to whatever else
        // held the port (VEL-303).
        let port = self.port
        var announced = false
        listener.stateUpdateHandler = { state in
            switch state {
            case .ready:
                // `.ready` can come again after `.waiting`: one ready line.
                guard !announced else { return }
                announced = true
                print("AtalantaRunner ServerURLHere->http://127.0.0.1:\(port)<-ServerURLHere")
                fflush(stdout)
            case .failed(let error):
                print("AtalantaRunner: listening on port \(port) failed: \(error)")
                exit(1)
            case .waiting(let error):
                // Some OS versions report a port that can't be bound as
                // `.waiting`, not `.failed`: without this there was no ready
                // line and no exit, and the host waited out its launch
                // timeout. Other waits (no network yet) may still resolve.
                if case .posix(let code) = error,
                   [.EADDRINUSE, .EADDRNOTAVAIL, .EACCES].contains(code) {
                    print("AtalantaRunner: listening on port \(port) failed: \(error)")
                    exit(1)
                }
                print("AtalantaRunner: listener waiting on port \(port): \(error)")
                fflush(stdout)
            default:
                break
            }
        }
        listener.start(queue: queue)
    }

    /// Read one request (headers, then Content-Length bytes), answer it on
    /// the main thread (`/status` on this queue), and keep the connection
    /// for the next one. The main thread's work is handed over, not waited
    /// for: a `main.sync` held this serial queue, so every other connection
    /// (health checks included) waited behind a slow handler (VEL-306).
    private func receive(_ conn: NWConnection, buffer: Data) {
        conn.receive(minimumIncompleteLength: 1, maximumLength: 1 << 20) { [weak self] data, _, done, error in
            guard let self else { return }
            var buf = buffer
            if let d = data { buf.append(d) }
            switch Self.parse(&buf) {
            case .request(let req):
                if let why = Guard.refusal(req) {
                    self.send(conn, .error(403, "forbidden", why)) {
                        conn.cancel()
                    }
                    return
                }
                if !Auth.allows(req.authorization) {
                    self.send(conn, .error(401, "unauthorized", "a bearer token is required")) {
                        conn.cancel()
                    }
                    return
                }
                if req.method == "GET" && req.path == "/status" {
                    self.send(conn, self.router.status()) {
                        self.receive(conn, buffer: buf)
                    }
                    return
                }
                DispatchQueue.main.async {
                    // Issues belong to the request that recorded them.
                    _ = Issues.take()
                    let response = Issues.answer(self.router.handle(req), issues: Issues.take())
                    self.queue.async {
                        self.send(conn, response) {
                            self.receive(conn, buffer: buf)
                        }
                    }
                }
                return
            case .bad(let why):
                // Answer and close: the rest of the stream can't be framed
                // (VEL-151: a malformed head wedged the connection, and a
                // negative Content-Length trapped the runner).
                self.send(conn, .error(400, "invalid argument", why)) { conn.cancel() }
                return
            case .incomplete:
                break
            }
            if done || error != nil {
                conn.cancel()
                return
            }
            self.receive(conn, buffer: buf)
        }
    }

    private func send(_ conn: NWConnection, _ r: Response, then next: @escaping () -> Void) {
        let reason = r.status == 200 ? "OK" : "Error"
        var head = "HTTP/1.1 \(r.status) \(reason)\r\n"
        head += "Content-Type: application/json;charset=UTF-8\r\n"
        head += "Content-Length: \(r.body.count)\r\n"
        head += "Connection: keep-alive\r\n\r\n"
        var out = Data(head.utf8)
        out.append(r.body)
        conn.send(content: out, completion: .contentProcessed { _ in next() })
    }

    enum Parsed {
        case request(Request)
        /// Wait for more bytes.
        case incomplete
        /// Unframeable: answer 400 and close.
        case bad(String)
    }

    /// Largest body accepted (WDA's own limit is of this order).
    static let maxBody = 64 << 20
    /// Largest request head accepted.
    static let maxHead = 64 << 10

    /// A complete request at the front of `buf`, removed from it.
    static func parse(_ buf: inout Data) -> Parsed {
        guard let headEnd = buf.range(of: Data("\r\n\r\n".utf8)) else {
            return buf.count > maxHead ? .bad("request head too large") : .incomplete
        }
        let head = String(decoding: buf[buf.startIndex..<headEnd.lowerBound], as: UTF8.self)
        let lines = head.components(separatedBy: "\r\n")
        let first = lines.first?.split(separator: " ").map(String.init) ?? []
        guard first.count >= 2 else { return .bad("malformed request line") }
        var length = 0
        var authorization: String? = nil
        var host: String? = nil
        var origin: String? = nil
        var fetchSite: String? = nil
        for l in lines.dropFirst() {
            let kv = l.split(separator: ":", maxSplits: 1).map { $0.trimmingCharacters(in: .whitespaces) }
            guard kv.count == 2 else { continue }
            switch kv[0].lowercased() {
            case "authorization": authorization = kv[1]
            // A second Host (or Origin) can't hide behind the first.
            case "host": host = host == nil ? kv[1] : ""
            case "origin": origin = kv[1]
            case "sec-fetch-site": fetchSite = kv[1]
            case "content-length":
                guard let n = Int(kv[1]), (0...maxBody).contains(n) else {
                    return .bad("bad Content-Length: \(kv[1])")
                }
                length = n
            default: break
            }
        }
        let bodyStart = headEnd.upperBound
        guard buf.distance(from: bodyStart, to: buf.endIndex) >= length else { return .incomplete }
        let bodyEnd = buf.index(bodyStart, offsetBy: length)
        let body = buf[bodyStart..<bodyEnd]
        buf = Data(buf[bodyEnd...])
        let path = first[1].split(separator: "?", maxSplits: 1).first.map(String.init) ?? first[1]
        let json = (try? JSONSerialization.jsonObject(with: body)) as? [String: Any] ?? [:]
        return .request(Request(method: first[0], path: path, json: json, authorization: authorization,
                                host: host, origin: origin, fetchSite: fetchSite))
    }
}

//! Velocity's own iOS runner (`probes/ios/VelocityRunner`, reached through
//! the `runner` symlink so the files ship inside the published crate), embedded so it
//! can be built on any Mac with Xcode, without the repository or xcodegen.

/// (path inside the project directory, contents)
pub const FILES: &[(&str, &str)] = &[
    (
        "VelocityRunner.xcodeproj/project.pbxproj",
        include_str!("../runner/VelocityRunner.xcodeproj/project.pbxproj"),
    ),
    (
        "VelocityRunner.xcodeproj/project.xcworkspace/contents.xcworkspacedata",
        include_str!(
            "../runner/VelocityRunner.xcodeproj/project.xcworkspace/contents.xcworkspacedata"
        ),
    ),
    (
        "VelocityRunner.xcodeproj/xcshareddata/xcschemes/VelocityRunner.xcscheme",
        include_str!(
            "../runner/VelocityRunner.xcodeproj/xcshareddata/xcschemes/VelocityRunner.xcscheme"
        ),
    ),
    (
        "VelocityRunnerUITests/VelocityRunner.swift",
        include_str!("../runner/VelocityRunnerUITests/VelocityRunner.swift"),
    ),
];

/// A content hash, so a changed runner gets its own cache entry.
pub fn hash() -> String {
    let h = FILES.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, (p, c)| {
        p.bytes().chain(c.bytes()).fold(h, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    });
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    /// The runner's Swift can't be built or run here (it needs Xcode and a
    /// simulator): pin the source contract the host relies on instead.
    #[test]
    fn the_runner_reports_issues_app_state_and_the_attributes_the_parser_reads() {
        let swift = super::FILES
            .iter()
            .find(|(p, _)| p.ends_with(".swift"))
            .unwrap()
            .1;
        for needle in [
            // P1 / N1: XCTest issues turn the request into a 500.
            "override func record(_ issue: XCTIssue)",
            "Issues.answer(self.router.handle(req), issues: Issues.take())",
            // P6: the liveness route (D-50 step 1) and focus/selection.
            "case (\"POST\", \"wda/apps/state\"):",
            " focused=\\\"true\\\"",
            " selected=\\\"true\\\"",
            // ui-frameworks P2 / N1-N2: the empty-field placeholder (VEL-354)
            // and visibility.
            " placeholderValue=\\\"",
            " visible=\\\"",
            // P7: a too-deep tree is read again with a capped depth.
            "SnapshotDepth.tooDeep(error)",
            " truncated=\\\"true\\\"",
            // The per-simulator sync probe port reaches the app through the
            // launch environment, at session creation and launch alike.
            "caps?[\"environment\"] as? [String: String]",
            "req.json[\"environment\"] as? [String: String]",
            "app.launchEnvironment = env",
            // D-37: the session's snapshotMaxDepth caps the deep-tree read.
            "Self.number(s[\"snapshotMaxDepth\"])",
            "SnapshotDepth.fallback = Int(d)",
            // D-38: the private hit test, every selector guarded.
            "case (\"POST\", \"velocity/hitTest\"):",
            "NSSelectorFromString(\"_XCT_requestElementAtPoint:reply:\")",
            "guard proxy.responds(to: sel)",
            "guard cls.responds(to: shared)",
            "guard session.responds(to: proxySel)",
            "guard element.responds(to: pidSel)",
            "app.responds(to: NSSelectorFromString(\"processID\"))",
            "static func deepest(",
            // R-1: the bearer token is checked on every request, and the
            // runner doesn't start without one; Host, Origin and
            // Sec-Fetch-Site are checked before it.
            "environment[\"VELOCITY_RUNNER_TOKEN\"]",
            "if !Auth.allows(req.authorization)",
            "guard let token else { return false }",
            "guard Auth.token != nil else {",
            "if let why = Guard.refusal(req) {",
            "if req.origin != nil {",
            "site != \"none\", site != \"same-origin\"",
            "var names = [\"127.0.0.1\", \"localhost\", \"[::1]\"]",
        ] {
            assert!(
                swift.contains(needle),
                "VelocityRunner.swift lacks {needle}"
            );
        }
    }
}

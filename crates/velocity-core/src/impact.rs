use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use tracing::{debug, info, warn};
use velocity_common::{Result, VelocityError};

/// Configuration for test impact analysis.
#[derive(Debug, Clone)]
pub struct ImpactConfig {
    /// Whether impact analysis is enabled.
    pub enabled: bool,
    /// Git base ref to diff against (e.g., "main", "origin/main", "HEAD~5").
    pub base_ref: String,
}

impl Default for ImpactConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_ref: "main".to_string(),
        }
    }
}

/// Mapping from source files to test flow IDs that exercise them.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ImpactMapping {
    /// Map of flow ID -> list of source file patterns that the flow exercises.
    #[serde(default)]
    pub flows: HashMap<String, Vec<String>>,
    /// Map of test name -> source file patterns it exercises.
    #[serde(default)]
    pub tests: HashMap<String, Vec<String>>,
    /// Extra patterns whose change means "run everything" (added to the
    /// built-in list: manifests, plists, resources, build files).
    #[serde(default, rename = "runAll", alias = "run_all")]
    pub run_all: Vec<String>,
    /// `impact:` options of the map file.
    #[serde(default)]
    pub impact: ImpactOptions,
}

/// The `impact:` section of `velocity-impact.yaml`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ImpactOptions {
    /// Tests with no mapping (no `tests:` entry and no mapped flow among
    /// the flows they run): `run` (default) always selects them, since
    /// nothing says a change can't affect them (D-24); `skip` leaves them
    /// to the history rules (failed recently, new, stale).
    #[serde(default)]
    pub unmapped: Unmapped,
}

/// `impact.unmapped`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Unmapped {
    #[default]
    Run,
    Skip,
}

impl ImpactMapping {
    /// Load impact mapping from a YAML file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            VelocityError::Config(format!(
                "Failed to read impact mapping at {}: {e}",
                path.display()
            ))
        })?;

        serde_yaml::from_str(&content)
            .map_err(|e| VelocityError::Config(format!("Failed to parse impact mapping: {e}")))
    }

    /// Create an empty mapping.
    pub fn empty() -> Self {
        Self {
            flows: HashMap::new(),
            tests: HashMap::new(),
            run_all: Vec::new(),
            impact: ImpactOptions::default(),
        }
    }

    /// Build a reverse index: source file pattern -> set of flow IDs.
    fn reverse_index(&self) -> HashMap<String, HashSet<String>> {
        let mut index: HashMap<String, HashSet<String>> = HashMap::new();
        for (flow_id, patterns) in &self.flows {
            for pattern in patterns {
                index
                    .entry(pattern.clone())
                    .or_default()
                    .insert(flow_id.clone());
            }
        }
        index
    }
}

/// Test impact analyzer that determines which test flows are affected by code changes.
pub struct ImpactAnalyzer {
    config: ImpactConfig,
    mapping: ImpactMapping,
}

impl ImpactAnalyzer {
    pub fn new(config: ImpactConfig, mapping: ImpactMapping) -> Self {
        Self { config, mapping }
    }

    /// Get the list of files changed since the base ref.
    pub fn changed_files(&self) -> Result<Vec<String>> {
        self.changed_files_in(Path::new("."))
    }

    /// [`Self::changed_files`] for the repository at `dir`.
    fn changed_files_in(&self, dir: &Path) -> Result<Vec<String>> {
        // A ref starting with `-` would be read by git as an option
        // (`--output=<file>..HEAD` truncates a file).
        if self.config.base_ref.starts_with('-') {
            return Err(VelocityError::Config(format!(
                "impact base ref {:?} must not start with `-`",
                self.config.base_ref
            )));
        }
        // `-z`: NUL-separated, unquoted. Without it git writes a path with
        // a non-ASCII character as `"caf\303\251.txt"` in quotes, which no
        // mapping pattern matches.
        let output = Command::new("git")
            .current_dir(dir)
            .args([
                "diff",
                "--name-only",
                "-z",
                &format!("{}..HEAD", self.config.base_ref),
            ])
            .output()
            .map_err(|e| VelocityError::Config(format!("Failed to run git diff: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(VelocityError::Config(format!("git diff failed: {stderr}")));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let files: Vec<String> = stdout
            .split('\0')
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();

        debug!(count = files.len(), base = %self.config.base_ref, "detected changed files");
        Ok(files)
    }

    /// Determine which flow IDs are affected by the current changes.
    ///
    /// Returns `None` if no mapping exists or impact analysis is disabled,
    /// meaning "run all tests" (safe fallback).
    pub fn affected_flows(&self) -> Result<Option<Vec<String>>> {
        if !self.config.enabled {
            return Ok(None);
        }

        if self.mapping.flows.is_empty() {
            info!("no impact mapping configured, running all tests");
            return Ok(None);
        }

        let changed_files = self.changed_files()?;
        if changed_files.is_empty() {
            info!(
                "no files changed since {}, skipping all tests",
                self.config.base_ref
            );
            return Ok(Some(Vec::new()));
        }

        let reverse_index = self.mapping.reverse_index();
        let mut affected: HashSet<String> = HashSet::new();

        for changed_file in &changed_files {
            for (pattern, flow_ids) in &reverse_index {
                if file_matches_pattern(changed_file, pattern) {
                    for flow_id in flow_ids {
                        affected.insert(flow_id.clone());
                    }
                }
            }
        }

        if affected.is_empty() {
            warn!(
                changed_files = changed_files.len(),
                "no flows matched changed files — consider running all tests"
            );
            // Safe fallback: run all tests if no mappings matched
            return Ok(None);
        }

        let mut result: Vec<String> = affected.into_iter().collect();
        result.sort();

        info!(
            affected = result.len(),
            total_changed = changed_files.len(),
            "impact analysis complete"
        );

        Ok(Some(result))
    }

    /// Filter a list of flow IDs to only those affected by changes.
    /// Returns the original list if impact analysis is disabled or no mapping exists.
    pub fn filter_flows(&self, all_flows: &[String]) -> Result<Vec<String>> {
        match self.affected_flows()? {
            Some(affected) => {
                let affected_set: HashSet<&str> = affected.iter().map(|s| s.as_str()).collect();
                Ok(all_flows
                    .iter()
                    .filter(|f| affected_set.contains(f.as_str()))
                    .cloned()
                    .collect())
            }
            None => Ok(all_flows.to_vec()),
        }
    }
}

/// Check if a file path matches a pattern: a path, a directory (`src/profile`
/// or `src/profile/`), or a glob with `*`, `?`, `**` (any number of
/// directories, anywhere in the pattern) and `{a,b}` alternatives. A pattern
/// without `/` matches a file's name in any directory.
///
/// `[` and `]` are literal, never a character class: file-based routers put
/// them in real paths (Expo Router's `app/[id].tsx`, Next.js'
/// `pages/[slug].tsx`), D-34. A leading `./` is ignored: git paths never
/// have one, so `./src/auth/` used to match nothing.
pub fn file_matches_pattern(file: &str, pattern: &str) -> bool {
    let mut pattern = pattern;
    while let Some(rest) = pattern.strip_prefix("./") {
        pattern = rest.trim_start_matches('/');
    }
    if pattern.is_empty() {
        return false;
    }
    // Exact match
    if file == pattern {
        return true;
    }

    // Directory prefix match ("src/screens/" or "src/screens" matches
    // "src/screens/Login.tsx").
    let dir = pattern.trim_end_matches('/');
    if !dir.is_empty()
        && !dir.contains(['*', '?', '{'])
        && file
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
    {
        return true;
    }

    if pattern.contains(['*', '?', '{']) {
        return glob_match(pattern, file);
    }

    // Pattern is just a filename — match against file's basename
    if !pattern.contains('/') {
        if let Some(basename) = file.rsplit('/').next() {
            return basename == pattern;
        }
    }

    false
}

/// Glob matching on path segments. A pattern without `/` is matched against
/// the file name only (`*.kt` matches `app/src/Cart.kt`).
fn glob_match(pattern: &str, text: &str) -> bool {
    expand_braces(pattern).iter().any(|p| {
        if p.contains('/') {
            let pat: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
            let txt: Vec<&str> = text.split('/').filter(|s| !s.is_empty()).collect();
            segments_match(&pat, &txt)
        } else if p == "**" {
            true
        } else {
            simple_glob_match(p, text.rsplit('/').next().unwrap_or(text))
        }
    })
}

/// `**` matches zero or more whole segments; any other segment matches one.
fn segments_match(pat: &[&str], txt: &[&str]) -> bool {
    match pat.split_first() {
        None => txt.is_empty(),
        Some((&"**", rest)) => (0..=txt.len()).any(|i| segments_match(rest, &txt[i..])),
        Some((p, rest)) => {
            !txt.is_empty() && simple_glob_match(p, txt[0]) && segments_match(rest, &txt[1..])
        }
    }
}

/// `{a,b}` alternatives, expanded into one pattern each (nesting allowed).
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_string()];
    };
    let mut depth = 0;
    let mut close = None;
    let mut commas = Vec::new();
    for (i, c) in pattern[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + i);
                    break;
                }
            }
            ',' if depth == 1 => commas.push(open + i),
            _ => {}
        }
    }
    let Some(close) = close else {
        return vec![pattern.to_string()];
    };
    let (head, tail) = (&pattern[..open], &pattern[close + 1..]);
    let mut bounds = vec![open];
    bounds.extend(&commas);
    bounds.push(close);
    bounds
        .windows(2)
        .flat_map(|w| expand_braces(&format!("{head}{}{tail}", &pattern[w[0] + 1..w[1]])))
        .collect()
}

/// One segment against `*` (any run of characters) and `?` (one character).
fn simple_glob_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    // Iterative wildcard match with backtracking to the last `*`.
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository in a temp dir with a commit, then a second adding a
    /// file with a non-ASCII name. Returns the dir and the first commit.
    fn repo_with_unicode_change() -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!(
            "velocity-impact-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(&dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["-c", "commit.gpgsign=false", "-c", "core.quotePath=true"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "one"]);
        let first = git(&["rev-parse", "HEAD"]);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/caf\u{e9}.txt"), "b").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "two"]);
        (dir, first)
    }

    #[test]
    fn a_changed_file_with_a_non_ascii_name_is_listed_as_written() {
        let (dir, first) = repo_with_unicode_change();
        let analyzer = ImpactAnalyzer::new(
            ImpactConfig {
                enabled: true,
                base_ref: first,
            },
            ImpactMapping::empty(),
        );
        let files = analyzer.changed_files_in(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(files, vec!["src/caf\u{e9}.txt".to_string()]);
    }

    #[test]
    fn a_base_ref_that_looks_like_an_option_is_refused() {
        let analyzer = ImpactAnalyzer::new(
            ImpactConfig {
                enabled: true,
                base_ref: "--output=velocity-impact-must-not-exist".to_string(),
            },
            ImpactMapping::empty(),
        );
        let e = analyzer.changed_files_in(Path::new(".")).unwrap_err();
        assert!(e.to_string().contains("must not start with"), "{e}");
        assert!(!Path::new("velocity-impact-must-not-exist..HEAD").exists());
    }

    #[test]
    fn test_file_matches_exact() {
        assert!(file_matches_pattern("src/Login.tsx", "src/Login.tsx"));
    }

    #[test]
    fn test_file_matches_directory_prefix() {
        assert!(file_matches_pattern(
            "src/screens/Login.tsx",
            "src/screens/"
        ));
        assert!(!file_matches_pattern("src/api/auth.ts", "src/screens/"));
    }

    #[test]
    fn test_file_matches_wildcard() {
        assert!(file_matches_pattern(
            "src/screens/Login.tsx",
            "src/screens/*.tsx"
        ));
        assert!(!file_matches_pattern(
            "src/screens/Login.ts",
            "src/screens/*.tsx"
        ));
    }

    #[test]
    fn test_file_matches_double_star() {
        assert!(file_matches_pattern(
            "src/screens/Login.tsx",
            "src/**/*.tsx"
        ));
        assert!(file_matches_pattern(
            "src/deep/nested/File.tsx",
            "src/**/*.tsx"
        ));
    }

    #[test]
    fn test_file_matches_basename() {
        assert!(file_matches_pattern("src/utils/auth.ts", "auth.ts"));
    }

    // D-34: brackets are literal (router paths), and a leading `./` is
    // the repository root.
    #[test]
    fn brackets_are_literal_and_a_leading_dot_slash_is_ignored() {
        assert!(file_matches_pattern("app/[id].tsx", "app/[id].tsx"));
        assert!(file_matches_pattern("app/[id]/edit.tsx", "app/[id]/*.tsx"));
        assert!(file_matches_pattern("app/[id]/edit.tsx", "app/[id]/"));
        assert!(file_matches_pattern("pages/[slug].tsx", "[slug].tsx"));
        assert!(!file_matches_pattern("a.kt", "[ab].kt"));
        assert!(!file_matches_pattern("app/i.tsx", "app/[id].tsx"));
        assert!(file_matches_pattern("src/auth/A.kt", "./src/auth/"));
        assert!(file_matches_pattern("src/auth/A.kt", "./src/**/*.kt"));
        assert!(file_matches_pattern("src/auth/A.kt", "./src/auth/A.kt"));
        assert!(!file_matches_pattern("src/auth/A.kt", "./"));
    }

    #[test]
    fn test_reverse_index() {
        let mapping = ImpactMapping {
            flows: HashMap::from([
                (
                    "login_test".to_string(),
                    vec!["src/screens/Login.tsx".to_string()],
                ),
                (
                    "checkout_test".to_string(),
                    vec![
                        "src/screens/Login.tsx".to_string(),
                        "src/api/cart.ts".to_string(),
                    ],
                ),
            ]),
            ..ImpactMapping::empty()
        };

        let index = mapping.reverse_index();
        let login_flows = index.get("src/screens/Login.tsx").unwrap();
        assert!(login_flows.contains("login_test"));
        assert!(login_flows.contains("checkout_test"));

        let cart_flows = index.get("src/api/cart.ts").unwrap();
        assert!(cart_flows.contains("checkout_test"));
        assert!(!cart_flows.contains("login_test"));
    }

    #[test]
    fn test_filter_flows_disabled() {
        let config = ImpactConfig {
            enabled: false,
            base_ref: "main".to_string(),
        };
        let analyzer = ImpactAnalyzer::new(config, ImpactMapping::empty());
        let all = vec!["flow1".to_string(), "flow2".to_string()];
        let result = analyzer.filter_flows(&all).unwrap();
        assert_eq!(result, all); // all flows returned when disabled
    }

    #[test]
    fn test_filter_flows_empty_mapping_returns_all() {
        let config = ImpactConfig {
            enabled: true,
            base_ref: "main".to_string(),
        };
        let analyzer = ImpactAnalyzer::new(config, ImpactMapping::empty());
        let all = vec!["flow1".to_string(), "flow2".to_string()];
        let result = analyzer.filter_flows(&all).unwrap();
        assert_eq!(result, all);
    }

    #[test]
    fn test_glob_match_double_star() {
        assert!(glob_match("src/**/*.tsx", "src/screens/Login.tsx"));
        assert!(glob_match("**/*.ts", "src/api/auth.ts"));
        assert!(!glob_match("src/**/*.tsx", "lib/other.tsx"));
    }

    #[test]
    fn test_simple_glob_match() {
        assert!(simple_glob_match("*.tsx", "Login.tsx"));
        assert!(!simple_glob_match("*.tsx", "Login.ts"));
        assert!(simple_glob_match("Login*", "LoginScreen.tsx"));
    }

    #[test]
    fn globs_match_mid_path_double_star_braces_question_and_dirs() {
        // VEL-007: each of these never matched, so the mapped test was skipped.
        for (p, f) in [
            ("app/**/checkout/*.kt", "app/src/main/checkout/Cart.kt"),
            ("src/**/auth/**", "src/main/auth/Login.kt"),
            ("**/auth/*.kt", "src/auth/Login.kt"),
            ("**/auth/*.kt", "auth/Login.kt"),
            ("src/**/screens/Login*.tsx", "src/app/screens/LoginView.tsx"),
            ("src/**/*.{ts,tsx}", "src/app/Home.tsx"),
            ("src/**/*.{ts,tsx}", "src/Home.ts"),
            ("src/ap?/Home.ts", "src/app/Home.ts"),
            ("src/profile", "src/profile/Edit.kt"),
            ("*.kt", "app/src/Cart.kt"),
        ] {
            assert!(file_matches_pattern(f, p), "{p} should match {f}");
        }
        for (p, f) in [
            ("app/**/checkout/*.kt", "app/src/main/payment/Cart.kt"),
            ("app/**/checkout/*.kt", "app/checkout/deep/Cart.kt"),
            ("src/**/*.{ts,tsx}", "src/app/Home.js"),
            ("src/ap?/Home.ts", "src/apps/Home.ts"),
            ("src/profile", "src/profile_old/Edit.kt"),
            ("src/*.ts", "src/a/b.ts"),
        ] {
            assert!(!file_matches_pattern(f, p), "{p} should not match {f}");
        }
    }
}

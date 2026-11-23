use velocity_common::Result;

/// Lightweight test header extracted without full YAML parsing.
/// Used for fast test discovery, filtering, and sharding decisions.
#[derive(Debug, Clone)]
pub struct TestHeader {
    pub name: String,
    pub tags: Vec<String>,
    pub isolated: bool,
    pub step_count: usize,
    /// Byte offset in the original YAML for lazy step parsing.
    pub byte_offset: usize,
}

/// Fast-path parser that extracts test metadata without deserializing
/// the full YAML structure. O(n) single pass over the file, no allocations
/// for steps unless needed.
pub fn parse_headers(yaml: &str) -> Result<Vec<TestHeader>> {
    let mut headers = Vec::new();
    let mut in_tests_block = false;
    let mut current_header: Option<PartialHeader> = None;
    let mut step_depth = false;
    let mut byte_offset = 0usize;
    // Where the tests' own `- name:` lines sit: a `- name:` deeper in
    // (a block-style `name:` selector in a `mask:` list) is no test.
    let mut test_indent: Option<usize> = None;

    for raw in yaml.split_inclusive('\n') {
        let offset = byte_offset;
        // The real length, newline included: `lines()` drops a `\r\n`'s two
        // bytes, and counting one put every offset after the first line of a
        // CRLF file too early.
        byte_offset += raw.len();
        let line = raw.trim_end_matches(['\n', '\r']);
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start().len();

        // Detect "tests:" top-level key
        if trimmed == "tests:" {
            in_tests_block = true;
            step_depth = false;
            test_indent = None;
            continue;
        }

        if !in_tests_block {
            continue;
        }

        // Detect start of a new test case "- name: ..."
        if trimmed.starts_with("- name:") && test_indent.is_none_or(|i| i == indent) {
            test_indent = Some(indent);
            // Flush previous header
            if let Some(h) = current_header.take() {
                headers.push(h.finalize());
            }
            let name = extract_yaml_string_value(trimmed, "- name:");
            current_header = Some(PartialHeader {
                name,
                tags: Vec::new(),
                isolated: false,
                step_count: 0,
                byte_offset: offset,
            });
            step_depth = false;
            continue;
        }

        if let Some(ref mut header) = current_header {
            // Detect tags line
            if let Some(stripped) = trimmed.strip_prefix("tags:") {
                let tag_part = stripped.trim();
                if tag_part.starts_with('[') && tag_part.ends_with(']') {
                    let inner = &tag_part[1..tag_part.len() - 1];
                    header.tags = inner
                        .split(',')
                        .map(|t| t.trim().trim_matches('"').trim_matches('\'').to_string())
                        .filter(|t| !t.is_empty())
                        .collect();
                }
                continue;
            }

            if trimmed == "isolated: true" {
                header.isolated = true;
                continue;
            }

            // Detect "steps:" block
            if trimmed == "steps:" {
                step_depth = true;
                continue;
            }

            // Count step entries (lines starting with "- " inside steps)
            if step_depth && trimmed.starts_with("- ") {
                header.step_count += 1;
            }

            // Exit test block if we hit a non-indented line that's not a continuation
            if !line.starts_with(' ') && !line.starts_with('\t') && !trimmed.is_empty() {
                in_tests_block = false;
                step_depth = false;
            }
        }
    }

    // Flush final header
    if let Some(h) = current_header.take() {
        headers.push(h.finalize());
    }

    Ok(headers)
}

struct PartialHeader {
    name: String,
    tags: Vec<String>,
    isolated: bool,
    step_count: usize,
    byte_offset: usize,
}

impl PartialHeader {
    fn finalize(self) -> TestHeader {
        TestHeader {
            name: self.name,
            tags: self.tags,
            isolated: self.isolated,
            step_count: self.step_count,
            byte_offset: self.byte_offset,
        }
    }
}

fn extract_yaml_string_value(line: &str, prefix: &str) -> String {
    let raw = line[prefix.len()..].trim();
    // Strip surrounding quotes if present
    // At least two characters: a lone `"` both starts and ends with a quote
    // and panicked on the slice (VEL-199).
    if raw.len() >= 2
        && ((raw.starts_with('"') && raw.ends_with('"'))
            || (raw.starts_with('\'') && raw.ends_with('\'')))
    {
        raw[1..raw.len() - 1].to_string()
    } else {
        raw.to_string()
    }
}

/// Parse mode: controls whether to use fast-path streaming or
/// full serde_yaml deserialization.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParserMode {
    /// Full validation via serde_yaml. Used in CI validation mode.
    Strict,
    /// Fast header-only parsing. Used for test discovery and sharding.
    HeadersOnly,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_headers_basic() {
        let yaml = r#"
appId: com.example.app
tests:
  - name: "login test"
    tags: [smoke, auth]
    isolated: true
    steps:
      - tap: { id: "btn" }
      - inputText: { selector: { id: "email" }, text: "test" }
      - assertVisible: { text: "Welcome" }
  - name: checkout flow
    tags: [e2e]
    steps:
      - tap: { id: "shop" }
      - tap: { id: "cart" }
"#;

        let headers = parse_headers(yaml).unwrap();
        assert_eq!(headers.len(), 2);

        assert_eq!(headers[0].name, "login test");
        assert_eq!(headers[0].tags, vec!["smoke", "auth"]);
        assert!(headers[0].isolated);
        assert_eq!(headers[0].step_count, 3);

        assert_eq!(headers[1].name, "checkout flow");
        assert_eq!(headers[1].tags, vec!["e2e"]);
        assert!(!headers[1].isolated);
        assert_eq!(headers[1].step_count, 2);
    }

    #[test]
    fn offsets_are_byte_positions_in_a_crlf_file() {
        let yaml = "appId: a\r\ntests:\r\n  - name: one\r\n    steps:\r\n      - tap: { id: x }\r\n  - name: two\r\n";
        let headers = parse_headers(yaml).unwrap();
        assert_eq!(headers.len(), 2);
        for h in &headers {
            assert!(
                yaml[h.byte_offset..].trim_start().starts_with("- name:"),
                "{} at {}",
                h.name,
                h.byte_offset
            );
        }
    }

    #[test]
    fn a_name_selector_in_a_step_is_not_a_test() {
        let yaml = "appId: a\ntests:\n  - name: shot\n    steps:\n      - assertScreenshot:\n          name: home\n          mask:\n            - name: Clock\n";
        let names: Vec<String> = parse_headers(yaml)
            .unwrap()
            .into_iter()
            .map(|h| h.name)
            .collect();
        assert_eq!(names, ["shot"]);
    }

    #[test]
    fn a_lone_quote_does_not_panic() {
        // VEL-199.
        assert_eq!(extract_yaml_string_value("- name: \"", "- name:"), "\"");
        assert_eq!(extract_yaml_string_value("- name: ''", "- name:"), "");
    }
}

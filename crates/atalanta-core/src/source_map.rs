//! Where each test and step is written in a suite file.
//!
//! serde_yaml parses the suite but keeps no spans, so this module makes a
//! second pass over the same text with libyaml (serde_yaml's own backend,
//! `unsafe-libyaml`) and records the start mark of every test and step. The
//! parser zips these with the deserialized suite by index, which is safe
//! because both passes read the same sequences in the same order.
//!
//! Only the structure matters here, so the tree built below keeps scalars,
//! sequences, mappings and their marks, nothing else.

use std::mem::MaybeUninit;

use unsafe_libyaml as unsafe_sys;

/// A 1-based line and column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    pub line: usize,
    pub column: usize,
}

/// A test or flow and the start of each of its steps, in file order.
#[derive(Debug, Clone, Default)]
pub struct ItemLocation {
    pub at: Option<Mark>,
    pub steps: Vec<Mark>,
}

/// Locations for one suite file.
#[derive(Debug, Clone, Default)]
pub struct SuiteLocations {
    pub tests: Vec<ItemLocation>,
    pub flows: Vec<ItemLocation>,
}

/// Locate the tests and steps of a suite. `None` if libyaml can't read the
/// text, in which case serde_yaml's own error (with its location) is the one
/// to report.
pub fn locate(yaml: &str) -> Option<SuiteLocations> {
    let events = read_events(yaml)?;
    let mut iter = events.into_iter().peekable();
    // Skip stream and document starts to the root node.
    while matches!(iter.peek(), Some(Ev::Other)) {
        iter.next();
    }
    let root = build(&mut iter, 0)?;
    let items = |key: &str| -> Vec<ItemLocation> {
        root.get(key)
            .map(|seq| {
                seq.items()
                    .iter()
                    .map(|item| ItemLocation {
                        at: Some(item.mark()),
                        steps: item
                            .get("steps")
                            .map(|s| s.items().iter().map(Node::mark).collect())
                            .unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    Some(SuiteLocations {
        tests: items("tests"),
        flows: items("flows"),
    })
}

enum Ev {
    Scalar(String, Mark),
    SeqStart(Mark),
    SeqEnd,
    MapStart(Mark),
    MapEnd,
    Alias(Mark),
    /// Stream and document boundaries.
    Other,
}

enum Node {
    Scalar(String, Mark),
    Seq(Vec<Node>, Mark),
    Map(Vec<(Node, Node)>, Mark),
    Alias(Mark),
}

impl Node {
    fn mark(&self) -> Mark {
        match self {
            Node::Scalar(_, m) | Node::Seq(_, m) | Node::Map(_, m) | Node::Alias(m) => *m,
        }
    }

    fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Node::Map(pairs, _) => pairs
                .iter()
                .find(|(k, _)| matches!(k, Node::Scalar(s, _) if s == key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    fn items(&self) -> &[Node] {
        match self {
            Node::Seq(items, _) => items,
            _ => &[],
        }
    }
}

/// Nesting deeper than this gets no positions (the build recursed once per
/// level, and a deeply nested YAML overflowed the stack: VEL-059).
const MAX_DEPTH: usize = 256;

fn build(events: &mut std::iter::Peekable<std::vec::IntoIter<Ev>>, depth: usize) -> Option<Node> {
    if depth > MAX_DEPTH {
        return None;
    }
    match events.next()? {
        Ev::Scalar(s, m) => Some(Node::Scalar(s, m)),
        Ev::Alias(m) => Some(Node::Alias(m)),
        Ev::SeqStart(m) => {
            let mut items = Vec::new();
            loop {
                if matches!(events.peek(), Some(Ev::SeqEnd)) {
                    events.next();
                    return Some(Node::Seq(items, m));
                }
                items.push(build(events, depth + 1)?);
            }
        }
        Ev::MapStart(m) => {
            let mut pairs = Vec::new();
            loop {
                if matches!(events.peek(), Some(Ev::MapEnd)) {
                    events.next();
                    return Some(Node::Map(pairs, m));
                }
                let k = build(events, depth + 1)?;
                let v = build(events, depth + 1)?;
                pairs.push((k, v));
            }
        }
        Ev::SeqEnd | Ev::MapEnd | Ev::Other => None,
    }
}

/// libyaml's event stream for `yaml`, up to the end of the first document.
fn read_events(yaml: &str) -> Option<Vec<Ev>> {
    // SAFETY: this follows serde_yaml's own use of the same API
    // (serde_yaml/src/libyaml/parser.rs). The parser lives in a Box so it
    // never moves after `yaml_parser_set_input_string` (libyaml keeps a
    // pointer to it as the read handler's data); `yaml` outlives every call;
    // each event is deleted after it is copied out, and the parser is
    // deleted on every path once initialized.
    unsafe {
        let mut parser: Box<MaybeUninit<unsafe_sys::yaml_parser_t>> =
            Box::new(MaybeUninit::uninit());
        let p = parser.as_mut_ptr();
        if !unsafe_sys::yaml_parser_initialize(p).ok {
            return None;
        }
        unsafe_sys::yaml_parser_set_encoding(p, unsafe_sys::YAML_UTF8_ENCODING);
        unsafe_sys::yaml_parser_set_input_string(p, yaml.as_ptr(), yaml.len() as _);

        let mut out = Vec::new();
        let mut depth = 0usize;
        let mut ok = true;
        loop {
            let mut event = MaybeUninit::<unsafe_sys::yaml_event_t>::uninit();
            let e = event.as_mut_ptr();
            if !unsafe_sys::yaml_parser_parse(p, e).ok {
                ok = false;
                break;
            }
            let mark = Mark {
                line: (*e).start_mark.line as usize + 1,
                column: (*e).start_mark.column as usize + 1,
            };
            let (ev, done) = match (*e).type_ {
                unsafe_sys::YAML_SCALAR_EVENT => {
                    let s = (*e).data.scalar;
                    let text = if s.value.is_null() {
                        String::new()
                    } else {
                        String::from_utf8_lossy(std::slice::from_raw_parts(
                            s.value,
                            s.length as usize,
                        ))
                        .into_owned()
                    };
                    (Ev::Scalar(text, mark), false)
                }
                unsafe_sys::YAML_SEQUENCE_START_EVENT => {
                    depth += 1;
                    (Ev::SeqStart(mark), false)
                }
                unsafe_sys::YAML_SEQUENCE_END_EVENT => {
                    depth = depth.saturating_sub(1);
                    (Ev::SeqEnd, false)
                }
                unsafe_sys::YAML_MAPPING_START_EVENT => {
                    depth += 1;
                    (Ev::MapStart(mark), false)
                }
                unsafe_sys::YAML_MAPPING_END_EVENT => {
                    depth = depth.saturating_sub(1);
                    (Ev::MapEnd, false)
                }
                unsafe_sys::YAML_ALIAS_EVENT => (Ev::Alias(mark), false),
                unsafe_sys::YAML_DOCUMENT_END_EVENT | unsafe_sys::YAML_STREAM_END_EVENT => {
                    (Ev::Other, true)
                }
                _ => (Ev::Other, false),
            };
            unsafe_sys::yaml_event_delete(e);
            out.push(ev);
            if done && depth == 0 {
                break;
            }
        }
        unsafe_sys::yaml_parser_delete(p);
        ok.then_some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tests_and_steps_get_their_lines() {
        let yaml = "\
appId: com.example
# a comment
flows:
  - id: login
    steps:
      - tap: { text: \"Log in\" }
tests:
  - name: first
    steps:
      - launchApp
      - tap:
          text: \"Pay\"
      - \"Tap the cart\"
  - name: second
    tags: [smoke]
    steps: [ { tap: { id: a } }, { runFlow: login } ]
";
        let loc = locate(yaml).unwrap();
        assert_eq!(loc.flows.len(), 1);
        assert_eq!(loc.flows[0].at.unwrap().line, 4);
        assert_eq!(loc.flows[0].steps[0].line, 6);
        assert_eq!(loc.tests.len(), 2);
        assert_eq!(loc.tests[0].at.unwrap().line, 8);
        let lines: Vec<usize> = loc.tests[0].steps.iter().map(|m| m.line).collect();
        assert_eq!(lines, vec![10, 11, 13]);
        assert_eq!(loc.tests[1].at.unwrap().line, 14);
        assert_eq!(loc.tests[1].steps.len(), 2);
        assert!(loc.tests[1].steps.iter().all(|m| m.line == 16));
        assert_eq!(loc.tests[0].steps[1].column, 9);
    }

    #[test]
    fn block_scalars_and_quoted_lines_do_not_shift_later_steps() {
        let yaml = "\
appId: x
tests:
  - name: t
    steps:
      - inputText:
          id: notes
          text: |
            line one
            - not a step
      - \"Check the
        long sentence\"
      - tap: { text: ok }
";
        let loc = locate(yaml).unwrap();
        let lines: Vec<usize> = loc.tests[0].steps.iter().map(|m| m.line).collect();
        assert_eq!(lines, vec![5, 10, 12]);
    }

    #[test]
    fn unreadable_yaml_gives_none() {
        assert!(locate("tests: [ unclosed").is_none());
    }

    #[test]
    fn deep_nesting_gives_no_positions_instead_of_overflowing() {
        // VEL-059: the recursive build overflowed the stack and aborted.
        let deep = format!("tests: {}{}", "[".repeat(50_000), "]".repeat(50_000));
        assert!(locate(&deep).is_none());
    }
}

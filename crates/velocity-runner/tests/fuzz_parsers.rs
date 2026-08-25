//! C8: property tests with hand-rolled random input (a fixed seed, so any
//! failure reproduces; no fuzzing crate). They check that the suite YAML
//! parser and `${}` interpolation never panic on any input, and that the
//! properties their docs promise hold for random values. The JUnit timings
//! parser's checks are next to it (`history.rs`), the probe's JSON parser's
//! in the probe's host checks (`HostChecks.miniJsonFuzz`).

use std::collections::HashMap;

/// xorshift64*: small, deterministic, good enough to pick characters.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick(&mut self, chars: &[char]) -> char {
        chars[self.below(chars.len())]
    }
    fn string(&mut self, chars: &[char], max: usize) -> String {
        let len = self.below(max + 1);
        (0..len).map(|_| self.pick(chars)).collect()
    }
}

const SUITE: &str = r#"appId: com.example.app
config:
  platform: android
  sync: { intervalMs: 300, stabilityCount: 3 }
flows:
  - id: login
    steps:
      - tap: { text: "Log in" }
tests:
  - name: checkout
    tags: [smoke]
    steps:
      - runFlow: { flowId: login }
      - inputText: { selector: { id: email }, text: "${EMAIL:-a@b.c}" }
      - assertVisible: { textContains: "Total" }
      - swipe: { direction: up }
      - wait: { ms: 10 }
"#;

/// Random edits to a valid suite, and random YAML-ish text: the parser
/// returns Ok or Err, it never panics (a panic is exit 5, not "your YAML
/// is wrong").
#[test]
fn the_suite_parser_never_panics() {
    let mut rng = Rng(0xC8_5EED);
    let yamlish: Vec<char> = "{}[]:-,\"' \n\t#&*!|>%@`$?0123abcxyz\u{e9}\u{0}"
        .chars()
        .collect();
    let base: Vec<char> = SUITE.chars().collect();
    for n in 0..400 {
        let input: String = if n % 4 == 3 {
            rng.string(&yamlish, 200)
        } else {
            let mut s = base.clone();
            for _ in 0..1 + rng.below(6) {
                if s.is_empty() {
                    break;
                }
                let at = rng.below(s.len());
                match rng.below(4) {
                    0 => {
                        s.remove(at);
                    }
                    1 => s.insert(at, rng.pick(&yamlish)),
                    2 => s.truncate(at),
                    _ => {
                        // Duplicate a slice: repeated keys, deeper nesting.
                        let end = (at + rng.below(40)).min(s.len());
                        let piece: Vec<char> = s[at..end].to_vec();
                        s.splice(at..at, piece);
                    }
                }
            }
            s.into_iter().collect()
        };
        let r = std::panic::catch_unwind(|| velocity_core::parser::parse_suite_from_str(&input));
        assert!(r.is_ok(), "the parser panicked on case {n}:\n{input}");
    }
    // Deep nesting is refused, not a stack overflow.
    for open in ["[", "{a: "] {
        let deep = format!("appId: x\ntests: {}", open.repeat(5000));
        let r = std::panic::catch_unwind(|| velocity_core::parser::parse_suite_from_str(&deep));
        assert!(matches!(r, Ok(Err(_))), "{open} x5000");
    }
}

fn interpolate(s: &str, vars: &HashMap<String, String>) -> velocity_common::Result<String> {
    velocity_core::env::interpolate(s, vars)
}

/// `${}` interpolation on random input: no panic; text without `${` is
/// returned as is; `$${…}` is a literal `${…}`; a defined variable is
/// replaced by its value and an unset one with a default by the default.
#[test]
fn interpolation_properties_hold_for_random_input() {
    let mut rng = Rng(0xC8_1E7);
    let any: Vec<char> = "${}:-?ABC_ab 1\n\u{e9}".chars().collect();
    let plain: Vec<char> = "abcXYZ 019_-:.?/\u{e9}{".chars().collect();
    let name_chars: Vec<char> = "ABCDEFGHIJ_".chars().collect();
    let mut vars = HashMap::new();
    for n in 0..1500 {
        // No panic, whatever the input.
        let s = rng.string(&any, 30);
        let r = std::panic::catch_unwind(|| interpolate(&s, &HashMap::new()));
        assert!(r.is_ok(), "interpolate panicked on {s:?}");

        // Literal text is untouched.
        let lit = rng.string(&plain, 20);
        assert_eq!(interpolate(&lit, &vars).unwrap(), lit, "case {n}");

        // `$${x}` is the literal `${x}`.
        let inner: String = rng.string(&plain, 10).replace(['{', '}'], "");
        assert_eq!(
            interpolate(&format!("{lit}$${{{inner}}}"), &vars).unwrap(),
            format!("{lit}${{{inner}}}"),
            "case {n}"
        );

        // A defined variable is replaced by its value, even one holding
        // `${` (values are not expanded again).
        let name = format!("C8_{}", rng.string(&name_chars, 8));
        let value = format!("{}${{X}}", rng.string(&plain, 10));
        vars.insert(name.clone(), value.clone());
        let pre = rng.string(&plain, 6).replace('{', "");
        assert_eq!(
            interpolate(&format!("{pre}${{{name}}}!"), &vars).unwrap(),
            format!("{pre}{value}!"),
            "case {n}"
        );

        // An unset one with a default gets the default.
        let default: String = rng.string(&plain, 10).replace(['{', '}'], "");
        let unset = format!("C8_UNSET_{n}");
        assert_eq!(
            interpolate(&format!("${{{unset}:-{default}}}"), &vars).unwrap(),
            default,
            "case {n}"
        );
        // And without one it is an error naming it, not a panic.
        let err = interpolate(&format!("${{{unset}}}"), &vars).unwrap_err();
        assert!(err.to_string().contains(&unset), "{err}");
    }
}

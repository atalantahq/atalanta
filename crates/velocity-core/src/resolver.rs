use std::collections::HashMap;

use velocity_common::{Action, Flow, Result, Step, TestCase, TestSuite, VelocityError};

pub fn resolve_flows(suite: &TestSuite) -> Result<Vec<TestCase>> {
    let flow_map: HashMap<&str, &Flow> = suite.flows.iter().map(|f| (f.id.as_str(), f)).collect();

    suite
        .tests
        .iter()
        .map(|test| resolve_test_case(test, &flow_map))
        .collect()
}

fn resolve_test_case(test: &TestCase, flow_map: &HashMap<&str, &Flow>) -> Result<TestCase> {
    let resolved_steps = resolve_steps(&test.steps, flow_map, &test.name, &mut Vec::new(), &mut 0)?;
    Ok(TestCase {
        name: test.name.clone(),
        tags: test.tags.clone(),
        isolated: test.isolated,
        depends_on: test.depends_on.clone(),
        steps: resolved_steps,
        // Kept: the executor reads both from the resolved test, so dropping
        // them made a test's `verify:` never run (a silent pass).
        description: test.description.clone(),
        verify: test.verify,
    })
}

/// Deepest runFlow nesting accepted.
const MAX_FLOW_DEPTH: usize = 32;
/// Most steps one test may expand to (a 94-line suite of nested flows
/// expanded to tens of millions and needed 1.5 GB: VEL-209).
const MAX_EXPANDED_STEPS: usize = 100_000;

/// Inline `runFlow` steps. `stack` holds the flows being expanded: a flow
/// that (indirectly) runs itself is an error, not a stack overflow.
/// `blocks` numbers the test's optional `runFlow` blocks.
fn resolve_steps<'a>(
    steps: &[Step],
    flow_map: &HashMap<&str, &'a Flow>,
    test_name: &str,
    stack: &mut Vec<&'a str>,
    blocks: &mut u32,
) -> Result<Vec<Step>> {
    let mut resolved = Vec::new();

    for step in steps {
        match &step.action {
            Action::RunFlow { flow_id } => {
                let flow = *flow_map.get(flow_id.as_str()).ok_or_else(|| {
                    VelocityError::UnknownFlowRef {
                        flow_id: flow_id.clone(),
                        test_name: test_name.to_string(),
                    }
                })?;
                if stack.contains(&flow.id.as_str()) {
                    let mut cycle: Vec<&str> = stack.clone();
                    cycle.push(&flow.id);
                    return Err(VelocityError::Config(format!(
                        "runFlow cycle in test '{test_name}': {}",
                        cycle.join(" -> ")
                    )));
                }
                if stack.len() >= MAX_FLOW_DEPTH {
                    return Err(VelocityError::Config(format!(
                        "runFlow nesting in test '{test_name}' is deeper than {MAX_FLOW_DEPTH} \
                         ({} -> {})",
                        stack.join(" -> "),
                        flow.id
                    )));
                }
                stack.push(&flow.id);
                let mut inner = resolve_steps(&flow.steps, flow_map, test_name, stack, blocks)?;
                stack.pop();
                // `optional: true` on runFlow: the inlined steps are one
                // block, abandoned at its first failure (D-12, M2: making
                // each step optional ran the rest of a flow that wasn't
                // there against the real screen).
                let block = step.modifiers.optional.then(|| {
                    *blocks += 1;
                    (*blocks, flow.id.clone())
                });
                // The runFlow step's own `timeoutMs` and `settle:` reach the
                // steps it inlines, unless a step sets its own (VEL-216:
                // they were accepted and silently dropped).
                for s in &mut inner {
                    if s.timeout_ms.is_none() {
                        s.timeout_ms = step.timeout_ms;
                    }
                    if s.settle.is_none() {
                        s.settle = step.settle.clone();
                    }
                    if let Some(b) = &block {
                        // Outermost first: nested blocks were added already.
                        s.modifiers.optional_flows.insert(0, b.clone());
                    }
                }
                if resolved.len() + inner.len() > MAX_EXPANDED_STEPS {
                    return Err(VelocityError::Config(format!(
                        "test '{test_name}' expands to more than {MAX_EXPANDED_STEPS} steps \
                         through runFlow (VEL-209: a few nested flows that each run the \
                         next several times multiply)"
                    )));
                }
                resolved.extend(inner);
            }
            _ => {
                resolved.push(step.clone());
            }
        }
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use velocity_common::{Selector, SuiteConfig};

    fn step(action: Action) -> Step {
        Step {
            action,
            timeout_ms: None,
            settle: None,
            source: None,
            modifiers: Default::default(),
        }
    }

    #[test]
    fn resolve_inlines_flow_steps() {
        let suite = TestSuite {
            app_id: "com.test".to_string(),
            config: SuiteConfig::default(),
            flows: vec![Flow {
                id: "login".to_string(),
                steps: vec![
                    step(Action::Tap {
                        selector: Selector::Id("username".to_string()),
                    }),
                    step(Action::InputText {
                        selector: Selector::Id("username".to_string()),
                        text: "user".to_string(),
                    }),
                    step(Action::Tap {
                        selector: Selector::Id("submit".to_string()),
                    }),
                ],
            }],
            tests: vec![TestCase {
                name: "test1".to_string(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![
                    step(Action::LaunchApp {
                        app_id: "com.test".to_string(),
                        clear_state: true,
                    }),
                    step(Action::RunFlow {
                        flow_id: "login".to_string(),
                    }),
                    step(Action::AssertVisible {
                        selector: Selector::Text("Dashboard".to_string()),
                    }),
                ],
                description: None,
                verify: None,
            }],
        };

        let resolved = resolve_flows(&suite).unwrap();
        assert_eq!(resolved.len(), 1);
        // 1 launchApp + 3 login steps + 1 assertVisible = 5
        assert_eq!(resolved[0].steps.len(), 5);
        // No RunFlow actions remain
        for step in &resolved[0].steps {
            assert!(!matches!(step.action, Action::RunFlow { .. }));
        }
    }

    #[test]
    fn resolving_keeps_a_tests_verify_and_description() {
        let suite = TestSuite {
            app_id: "a".to_string(),
            config: SuiteConfig::default(),
            flows: vec![],
            tests: vec![TestCase {
                name: "t".to_string(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![step(Action::Tap {
                    selector: Selector::Text("Go".to_string()),
                })],
                description: Some("the order is paid".to_string()),
                verify: Some(velocity_common::VerifyLevel::Strict),
            }],
        };
        let resolved = resolve_flows(&suite).unwrap();
        assert_eq!(
            resolved[0].verify,
            Some(velocity_common::VerifyLevel::Strict)
        );
        assert_eq!(
            resolved[0].description.as_deref(),
            Some("the order is paid")
        );
    }

    #[test]
    fn an_optional_run_flow_makes_its_steps_one_optional_block() {
        let mut run = step(Action::RunFlow {
            flow_id: "promo".to_string(),
        });
        run.modifiers.optional = true;
        let suite = TestSuite {
            app_id: "a".to_string(),
            config: SuiteConfig::default(),
            flows: vec![Flow {
                id: "promo".to_string(),
                steps: vec![step(Action::Tap {
                    selector: Selector::Text("Close".to_string()),
                })],
            }],
            tests: vec![TestCase {
                name: "t".to_string(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![run],
                description: None,
                verify: None,
            }],
        };
        let resolved = resolve_flows(&suite).unwrap();
        // M2: a block, not steps that are each optional.
        let m = &resolved[0].steps[0].modifiers;
        assert!(!m.optional);
        assert_eq!(m.optional_flows, vec![(1, "promo".to_string())]);
    }

    #[test]
    fn nested_optional_flows_are_separate_blocks_outermost_first() {
        let opt = |id: &str| {
            let mut s = step(Action::RunFlow {
                flow_id: id.to_string(),
            });
            s.modifiers.optional = true;
            s
        };
        let tap = |t: &str| {
            step(Action::Tap {
                selector: Selector::Text(t.to_string()),
            })
        };
        let suite = TestSuite {
            app_id: "a".to_string(),
            config: SuiteConfig::default(),
            flows: vec![
                Flow {
                    id: "outer".to_string(),
                    steps: vec![tap("A"), opt("inner"), tap("C")],
                },
                Flow {
                    id: "inner".to_string(),
                    steps: vec![tap("B")],
                },
            ],
            tests: vec![TestCase {
                name: "t".to_string(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![opt("outer"), opt("inner"), tap("D")],
                description: None,
                verify: None,
            }],
        };
        let steps = &resolve_flows(&suite).unwrap()[0].steps;
        let blocks: Vec<Vec<u32>> = steps
            .iter()
            .map(|s| s.modifiers.optional_flows.iter().map(|b| b.0).collect())
            .collect();
        // outer = 2 (numbered after the inner block it holds), the inner
        // run inside it = 1, the second inner run = 3.
        assert_eq!(blocks, vec![vec![2], vec![2, 1], vec![2], vec![3], vec![]]);
    }

    #[test]
    fn resolve_nested_flows() {
        let suite = TestSuite {
            app_id: "com.test".to_string(),
            config: SuiteConfig::default(),
            flows: vec![
                Flow {
                    id: "setup".to_string(),
                    steps: vec![step(Action::LaunchApp {
                        app_id: "com.test".to_string(),
                        clear_state: true,
                    })],
                },
                Flow {
                    id: "login".to_string(),
                    steps: vec![
                        step(Action::RunFlow {
                            flow_id: "setup".to_string(),
                        }),
                        step(Action::Tap {
                            selector: Selector::Id("login".to_string()),
                        }),
                    ],
                },
            ],
            tests: vec![TestCase {
                name: "test1".to_string(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![step(Action::RunFlow {
                    flow_id: "login".to_string(),
                })],
                description: None,
                verify: None,
            }],
        };

        let resolved = resolve_flows(&suite).unwrap();
        assert_eq!(resolved[0].steps.len(), 2);
        assert!(matches!(
            resolved[0].steps[0].action,
            Action::LaunchApp { .. }
        ));
        assert!(matches!(resolved[0].steps[1].action, Action::Tap { .. }));
    }

    #[test]
    fn resolve_unknown_flow_fails() {
        let suite = TestSuite {
            app_id: "com.test".to_string(),
            config: SuiteConfig::default(),
            flows: vec![],
            tests: vec![TestCase {
                name: "test1".to_string(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![step(Action::RunFlow {
                    flow_id: "missing".to_string(),
                })],
                description: None,
                verify: None,
            }],
        };

        let err = resolve_flows(&suite).unwrap_err();
        assert!(matches!(err, VelocityError::UnknownFlowRef { .. }));
    }

    #[test]
    fn resolve_preserves_non_flow_steps() {
        let suite = TestSuite {
            app_id: "com.test".to_string(),
            config: SuiteConfig::default(),
            flows: vec![],
            tests: vec![TestCase {
                name: "test1".to_string(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![
                    step(Action::Wait { ms: 100 }),
                    step(Action::Screenshot { filename: None }),
                ],
                description: None,
                verify: None,
            }],
        };

        let resolved = resolve_flows(&suite).unwrap();
        assert_eq!(resolved[0].steps.len(), 2);
    }

    #[test]
    fn resolve_preserves_metadata() {
        let suite = TestSuite {
            app_id: "com.test".to_string(),
            config: SuiteConfig::default(),
            flows: vec![],
            tests: vec![TestCase {
                name: "my test".to_string(),
                tags: vec!["smoke".to_string()],
                isolated: true,
                depends_on: vec![],
                steps: vec![step(Action::Wait { ms: 100 })],
                description: None,
                verify: None,
            }],
        };

        let resolved = resolve_flows(&suite).unwrap();
        assert_eq!(resolved[0].name, "my test");
        assert_eq!(resolved[0].tags, vec!["smoke"]);
        assert!(resolved[0].isolated);
    }

    #[test]
    fn a_flow_that_runs_itself_is_an_error_not_a_crash() {
        let yaml = "appId: x\nflows:\n  - id: a\n    steps:\n      - runFlow: b\n  - id: b\n    steps:\n      - runFlow: a\ntests:\n  - name: t\n    steps:\n      - runFlow: a\n";
        let suite = crate::parser::parse_suite_from_str(yaml).unwrap();
        let err = resolve_flows(&suite).unwrap_err().to_string();
        assert!(err.contains("a -> b -> a"), "{err}");
    }

    #[test]
    fn run_flow_modifiers_reach_the_inlined_steps() {
        // VEL-216 / P8.
        let tap = |t: &str| {
            step(Action::Tap {
                selector: Selector::Text(t.into()),
            })
        };
        let mut own = tap("B");
        own.timeout_ms = Some(9);
        let flow = Flow {
            id: "f".into(),
            steps: vec![tap("A"), own],
        };
        let mut run = step(Action::RunFlow {
            flow_id: "f".into(),
        });
        run.timeout_ms = Some(5000);
        run.settle = Some(velocity_common::StepSettle {
            skip: true,
            ..Default::default()
        });
        let suite = TestSuite {
            app_id: "a".into(),
            config: SuiteConfig::default(),
            flows: vec![flow],
            tests: vec![TestCase {
                name: "t".into(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![run],
                description: None,
                verify: None,
            }],
        };
        let steps = &resolve_flows(&suite).unwrap()[0].steps;
        assert_eq!(steps[0].timeout_ms, Some(5000));
        assert!(steps[0].settle.as_ref().unwrap().skip);
        assert_eq!(steps[1].timeout_ms, Some(9));
    }

    #[test]
    fn a_runaway_expansion_is_an_error() {
        // VEL-209: 7 flows each running the next 10 times = 10^6 steps.
        let mut flows = vec![Flow {
            id: "f0".into(),
            steps: vec![step(Action::Wait { ms: 1 })],
        }];
        for i in 1..7 {
            flows.push(Flow {
                id: format!("f{i}"),
                steps: (0..10)
                    .map(|_| {
                        step(Action::RunFlow {
                            flow_id: format!("f{}", i - 1),
                        })
                    })
                    .collect(),
            });
        }
        let suite = TestSuite {
            app_id: "a".into(),
            config: SuiteConfig::default(),
            flows,
            tests: vec![TestCase {
                name: "t".into(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![step(Action::RunFlow {
                    flow_id: "f6".into(),
                })],
                description: None,
                verify: None,
            }],
        };
        let e = resolve_flows(&suite).unwrap_err();
        assert!(e.to_string().contains("100000"), "{e}");
    }
}

use std::collections::HashMap;
use indexmap::IndexMap;
use base64;

use super::common::{Error, CompileOptions, CompileResult};
use super::interpreter::{RuleKey, Interpreted, Expression};
use super::analysis::{parse_scope};
use crate::sublime_syntax;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ContextKey<'a> {
    rule_key: &'a RuleKey<'a>,
    context: Context<'a>,
    branch_point: Option<String>,
}

#[derive(Debug)]
struct ContextCacheEntry {
    name: String,
    capture: bool,
}

impl ContextCacheEntry {
    fn new(name: String) -> ContextCacheEntry {
        ContextCacheEntry { name, capture: false }
    }
}

#[derive(Debug)]
struct Rule {
    context_count: usize,
    branch_point_count: usize,
}

struct State<'a> {
    rules: HashMap<&'a RuleKey<'a>, Rule>,
    context_cache: HashMap<ContextKey<'a>, ContextCacheEntry>,
    contexts: HashMap<String, sublime_syntax::Context>,

    _errors: Vec<Error<'a>>,
    _warnings: Vec<Error<'a>>,
}

pub fn codegen<'a>(_options: &CompileOptions<'a>, interpreted: Interpreted<'a>) -> CompileResult<'a, sublime_syntax::Syntax> {
    let mut state = State {
        rules: HashMap::new(),
        context_cache: HashMap::new(),
        contexts: HashMap::new(),

        _errors: vec!(),
        _warnings: vec!(),
    };

    for rule_key in &interpreted.entry_points {
        gen_rule(&mut state, &interpreted, rule_key);
    }


    // All rules should be compiled
    // assert!(state.rules.len() == interpreted.rules.len() || !state.errors.is_empty());

    let syntax = sublime_syntax::Syntax {
        name: interpreted.metadata.name.clone(),
        file_extensions: interpreted.metadata.file_extensions.clone(),
        first_line_match: interpreted.metadata.first_line_match.clone(),
        scope: interpreted.metadata.scope.clone(),
        hidden: interpreted.metadata.hidden,
        variables: HashMap::new(),
        contexts: state.contexts,
    };

    CompileResult::new(Ok(syntax), vec!())
}

fn gen_rule<'a>(state: &mut State<'a>, interpreted: &'a Interpreted<'a>, rule_key: &'a RuleKey<'a>) {
    let rule = &interpreted.rules[rule_key];

    let context = collect_context_nodes(interpreted, &rule.expression);

    let context_key = ContextKey {
        rule_key,
        context,
        branch_point: None,
    };

    let name = rule_key.name.to_string();

    let old_entry = state.context_cache.insert(context_key.clone(), ContextCacheEntry { name: name.clone(), capture: true });
    assert!(old_entry.is_none());

    gen_contexts(state, interpreted, vec!((name, context_key)));
}

// Compile a set of contexts following a branch point. If no branch point exists
// there should only be one context.
fn gen_contexts<'a>(state: &mut State<'a>, interpreted: &'a Interpreted<'a>, contexts: Vec<(String, ContextKey<'a>)>) {
    assert!(!contexts.is_empty());
    let branch_point = contexts[0].1.branch_point.clone();

    // If we've got more than one context we must have a branch point
    assert!(contexts.len() == 1 || branch_point.is_some());

    // Collect unique regexes and unique matches in each context. We use
    // unique regexes to determine which contexts need to continue, and we
    // use unique matches per context to determine when a branch needs to be
    // made.
    let mut regexes: HashMap<&'a str, usize> = HashMap::new();
    let mut context_maps: Vec<IndexMap<&'a str, Vec<&MatchStack<'a>>>> = vec!();

    for (_, context_key) in &contexts {
        let mut map: IndexMap<&'a str, Vec<&MatchStack<'a>>> = IndexMap::new();

        for match_stack in &context_key.context.matches {
            let regex = match_stack[0].get_regex();

            if let Some(c) = regexes.get_mut(regex) {
                *c += 1;
            } else {
                regexes.insert(regex, 1);
            }

            if let Some(m) = map.get_mut(regex) {
                m.push(match_stack);
            } else {
                map.insert(regex, vec!(match_stack));
            }
        }

        context_maps.push(map);
    };

    assert!(context_maps.len() == contexts.len());

    let mut next_contexts: Vec<(String, ContextKey<'a>)> = vec!();

    for (i, ((name, context_key), matches_map)) in contexts.iter().zip(context_maps.iter()).enumerate() {
        let rule_key = context_key.rule_key;

        let is_last = i == contexts.len() - 1;

        let mut patterns = vec!();

        for (regex, matches) in matches_map {
            let continue_branch = *regexes.get(regex).unwrap() > 1;

            if matches.len() == 1 {
                let match_ = matches[0];

                // let rule_name = match_.rule(rule.name);

                // Continue branch
                if continue_branch {
                    let scope = scope_for_match_stack(interpreted, Some(rule_key), match_);

                    let exit =
                        if let Some(context) = advance_context_stack(interpreted, match_) {
                            let next_key = ContextKey {
                                rule_key,
                                context: context.clone(),
                                branch_point: branch_point.clone(),
                            };

                            let name =
                                if let Some(name) = state.context_cache.get(&next_key).map(|c| &c.name) {
                                    name.clone()
                                } else {
                                    let name = create_context_name(state, next_key.clone());

                                    next_contexts.push((name.clone(), next_key));
                                    name
                                };
                            sublime_syntax::ContextChange::Set(vec!(name))
                        } else {
                            sublime_syntax::ContextChange::Pop(1)
                        };

                    patterns.push(gen_terminal(state, name, context_key, scope, match_[0].expression, exit));
                } else {
                    // End points of branch points need to use the current
                    // rule as a scope.
                    let scope = if branch_point.is_some() {
                            scope_for_match_stack(interpreted, Some(rule_key), match_)
                        } else {
                            scope_for_match_stack(interpreted, None, match_)
                        };

                    patterns.push(gen_simple_match(state, interpreted, name, context_key, scope, match_));
                }
            } else {
                // TOOD: Handle continue_branch?

                // Start new branch
                let bp_name = create_branch_point(state, rule_key);
                let branch_point = Some(bp_name.clone());

                let lookahead = format!("(?={})", regex);

                let mut branches = vec!();

                for match_ in matches {
                    // Determine if the branch is a simple repetition. If so
                    // we can ignore the repetition in the branch. This
                    // avoids context stack leaks in most cases.
                    let branch_match =
                        if match_[match_.len() - 1].is_repetition() && collect_context_nodes_concatenation(interpreted, &match_[match_.len() - 1].remaining) == context_key.context {
                            &match_[..match_.len() - 1]
                        } else {
                            &match_
                        };

                    let branch_rule_key = rule_for_match_stack(rule_key, branch_match);

                    let branch_key = ContextKey {
                        rule_key: branch_rule_key,
                        context: Context {
                            matches: vec!(branch_match.to_vec()),
                            end: ContextEnd::Illegal,
                            maybe_empty: false,
                        },
                        branch_point: branch_point.clone(),
                    };

                    let ctx_name = create_context_name(state, branch_key.clone());
                    branches.push(ctx_name.clone());

                    let next_name =
                        if let Some(context) = advance_context_stack(interpreted, branch_match) {
                            let next_key = ContextKey {
                                rule_key,
                                context,
                                branch_point: branch_point.clone(),
                            };

                            if let Some(name) = state.context_cache.get(&next_key).map(|c| &c.name) {
                                Some(name.clone())
                            } else {
                                let name = create_context_name(state, next_key.clone());

                                next_contexts.push((name.clone(), next_key));
                                Some(name)
                            }
                        } else {
                            None
                        };

                    let scope = scope_for_match_stack(interpreted, None, branch_match);

                    let exit = if let Some(name) = next_name {
                            let push =
                                if context_key.context.end == ContextEnd::None || match_stack_is_repetition(match_) {
                                    vec!(gen_pop(state, 2), name)
                                } else {
                                    vec!(gen_pop(state, 3), name)
                                };

                            // Using set in branch_point is broken, so we
                            // have to use push.
                            sublime_syntax::ContextChange::Push(push)
                        } else {
                            sublime_syntax::ContextChange::Pop(2)
                        };

                    let terminal_match = gen_terminal(state, &ctx_name, &branch_key, scope, branch_match[0].expression, exit);

                    state.contexts.insert(ctx_name, sublime_syntax::Context {
                        meta_scope: sublime_syntax::Scope::empty(),
                        meta_content_scope: sublime_syntax::Scope::empty(),
                        meta_include_prototype: false,
                        clear_scopes: sublime_syntax::ScopeClear::Amount(0),
                        matches: vec!(terminal_match),
                    });
                }

                patterns.push(sublime_syntax::ContextPattern::Match(sublime_syntax::Match {
                    pattern: sublime_syntax::Pattern::new(lookahead),
                    scope: sublime_syntax::Scope::empty(),
                    captures: HashMap::new(),
                    change_context: sublime_syntax::ContextChange::Branch(bp_name, branches),
                }));
            }
        }

        let meta_content_scope: sublime_syntax::Scope;
        let meta_include_prototype: bool;
        let capture: bool;
        {
            // Branch points have an "invalid" rule at the top of the stack
            let rule = interpreted.rules.get(rule_key).unwrap();

            meta_content_scope = if branch_point.is_none() {
                    rule.options.scope.clone()
                } else {
                    sublime_syntax::Scope::empty()
                };

            meta_include_prototype = rule.options.include_prototype;
            capture = rule.options.capture && state.context_cache[context_key].capture;
        }

        // Need to add the meta_content_scope to all patterns that pop. This
        // matches expected behaviour in that the rule scope applies to all
        // matches in this context.
        for p in &mut patterns {
            match p {
                sublime_syntax::ContextPattern::Match(sublime_syntax::Match {
                    scope,
                    change_context: sublime_syntax::ContextChange::Pop(_),
                    ..
                }) => {
                    scope.scopes = meta_content_scope.scopes.iter().chain(scope.scopes.iter()).cloned().collect::<Vec<_>>();
                },
                _ => {},
            }
        }

        if let Some(pattern) = gen_end_match(interpreted, context_key, is_last, capture) {
            patterns.push(pattern);
        }

        assert!(state.contexts.get(name).is_none());
        state.contexts.insert(name.clone(), sublime_syntax::Context {
            meta_scope: sublime_syntax::Scope::empty(),
            meta_content_scope,
            meta_include_prototype,
            clear_scopes: sublime_syntax::ScopeClear::Amount(0),
            matches: patterns,
        });
    }

    if !next_contexts.is_empty() {
        gen_contexts(state, interpreted, next_contexts);
    }
}

fn gen_pop<'a>(state: &mut State<'a>, amount: u16) -> String {
    let name = format!("pop-{}", amount);

    if state.contexts.get(&name).is_some() {
        return name;
    }

    state.contexts.insert(name.clone(), sublime_syntax::Context {
        meta_scope: sublime_syntax::Scope::empty(),
        meta_content_scope: sublime_syntax::Scope::empty(),
        meta_include_prototype: false,
        clear_scopes: sublime_syntax::ScopeClear::Amount(0),
        matches: vec!(
            sublime_syntax::ContextPattern::Match(sublime_syntax::Match {
                pattern: sublime_syntax::Pattern::from_str(""),
                scope: sublime_syntax::Scope::empty(),
                captures: HashMap::new(),
                change_context: sublime_syntax::ContextChange::Pop(amount),
            }),
        ),
    });

    name
}

fn gen_end_match<'a>(interpreted: &'a Interpreted<'a>, context_key: &ContextKey<'a>, is_last: bool, capture: bool) -> Option<sublime_syntax::ContextPattern> {
    match context_key.context.end {
        ContextEnd::Illegal => Some(
            if context_key.context.maybe_empty && !capture {
                sublime_syntax::Match {
                    pattern: sublime_syntax::Pattern::from_str(r#"(?=\S)"#),
                    scope: sublime_syntax::Scope::empty(),
                    captures: HashMap::new(),
                    change_context: sublime_syntax::ContextChange::Pop(1),
                }
            } else if context_key.branch_point.is_some() && !is_last {
                sublime_syntax::Match {
                    pattern: sublime_syntax::Pattern::from_str(r#"\S"#),
                    scope: sublime_syntax::Scope::empty(),
                    captures: HashMap::new(),
                    change_context: sublime_syntax::ContextChange::Fail(context_key.branch_point.as_ref().unwrap().clone()),
                }
            } else {
                let exit = if capture {
                        sublime_syntax::ContextChange::None
                    } else {
                        sublime_syntax::ContextChange::Pop(1)
                    };

                sublime_syntax::Match {
                    pattern: sublime_syntax::Pattern::from_str(r#"\S"#),
                    scope: parse_scope(&interpreted.metadata, "invalid.illegal"),
                    captures: HashMap::new(),
                    change_context: exit,
                }
            }),
        ContextEnd::None => None,
        ContextEnd::Push(_) => panic!(),
    }.map(&sublime_syntax::ContextPattern::Match)
}

fn gen_terminal<'a>(_state: &mut State<'a>, context_name: &str, _context_key: &ContextKey<'a>, scope: sublime_syntax::Scope, terminal: &'a Expression<'a>, mut exit: sublime_syntax::ContextChange) -> sublime_syntax::ContextPattern {
    let (regex, options) =
        match terminal {
            Expression::Terminal { regex: r, options: o, .. } => (r, o),
            _ => panic!(),
        };

    // TODO: embed
    /*if let Some(embed) = options.embed {
        // if let Some(prototype_key) = options.prototype {
        // }

        // If the exit is pop: 1, we don't need to create an embed context.
        // Otherwise we do.
        if exit == sublime_syntax::ContextChange::Pop(1) || exit == sublime_syntax::ContextChange::None {
            let use_push = exit == sublime_syntax::ContextChange::None;

            exit = sublime_syntax::ContextChange::IncludeEmbed(
                sublime_syntax::IncludeEmbed {
                    path: embed.to_string(),
                    use_push,
                    with_prototype: vec!(),
                });
        } else {
            match &mut exit {
                sublime_syntax::ContextChange::Pop(_pop_amount) => {
                    // Create a pop and embed context, and set them both
                    panic!("TODO");
                },
                sublime_syntax::ContextChange::Push(ref mut contexts)
                | sublime_syntax::ContextChange::Set(ref mut contexts) => {
                    // Create an embed context that does the actual embedding in
                    // a set, then add that to the contexts.
                    let embed_context = create_context_name(state, key);

                    state.contexts.insert(embed_context.clone(), sublime_syntax::Context {
                        meta_scope: sublime_syntax::Scope::empty(),
                        meta_content_scope: sublime_syntax::Scope::empty(),
                        meta_include_prototype: false,
                        clear_scopes: sublime_syntax::ScopeClear::Amount(0),
                        matches: vec!(sublime_syntax::ContextPattern::Match(sublime_syntax::Match {
                            pattern: sublime_syntax::Pattern::from_str(""),
                            scope: sublime_syntax::Scope::empty(),
                            captures: HashMap::new(),
                            change_context: sublime_syntax::ContextChange::IncludeEmbed(
                                sublime_syntax::IncludeEmbed {
                                    path: embed.to_string(),
                                    use_push: false,
                                    with_prototype: vec!(),
                                }
                            ),
                        })),
                    });

                    contexts.push(embed_context);
                },
                _ => panic!(),
            }
        }
    }*/

    // Translate Set into Push/Pop if we're setting back to the same context
    match &exit {
        sublime_syntax::ContextChange::Set(sets) => {
            if sets[0] == context_name {
                if sets.len() > 1 {
                    exit = sublime_syntax::ContextChange::Push(sets[1..].to_vec());
                } else {
                    exit = sublime_syntax::ContextChange::Pop(1);
                }
            }
        },
        _ => {}
    }

    sublime_syntax::ContextPattern::Match(sublime_syntax::Match {
        pattern: sublime_syntax::Pattern::new(regex.to_string()),
        scope: scope,
        captures: options.captures.clone(),
        change_context: exit,
    })
}

fn gen_simple_match<'a>(state: &mut State<'a>, interpreted: &'a Interpreted<'a>, context_name: &str, context_key: &ContextKey<'a>, scope: sublime_syntax::Scope, match_stack: &MatchStack<'a>) -> sublime_syntax::ContextPattern {
    let contexts =
        if match_stack[match_stack.len() - 1].is_repetition() {
            let last_match = &match_stack[match_stack.len() - 1];
            assert!(!last_match.remaining.is_empty());
            let context = collect_context_nodes_concatenation(interpreted, &last_match.remaining);

            let mut contexts = gen_simple_match_contexts(state, interpreted, context_key.rule_key, &match_stack[..match_stack.len() - 1]);

            if context == context_key.context {
                // If the remaining of a top-level repetition leads to the same
                // context, then we have a simple repetition. We can just push
                // the child match.
                let exit =
                    if contexts.is_empty() {
                        sublime_syntax::ContextChange::None
                    } else {
                        sublime_syntax::ContextChange::Push(contexts)
                    };

                return gen_terminal(state, context_name, context_key, scope, match_stack[0].expression, exit);
            } else {
                // Otherwise we have a complex repetition, which behaves the
                // same way as a regular match.
                let repetition_context_key = ContextKey {
                    rule_key: context_key.rule_key,
                    context,
                    branch_point: None,
                };

                if let Some(name) = state.context_cache.get(&repetition_context_key).map(|c| &c.name) {
                    contexts.insert(0, name.to_string());
                } else {
                    let name = create_context_name(state, repetition_context_key.clone());
                    gen_contexts(state, interpreted, vec!((name.clone(), repetition_context_key)));

                    contexts.insert(0, name);
                }
            }

            contexts
        } else {
            gen_simple_match_contexts(state, interpreted, context_key.rule_key, match_stack)
        };

    let exit =
        if contexts.is_empty() {
            sublime_syntax::ContextChange::Pop(1)
        } else {
            sublime_syntax::ContextChange::Set(contexts)
        };

    gen_terminal(state, context_name, context_key, scope, match_stack[0].expression, exit)
}

fn gen_simple_match_contexts<'a>(state: &mut State<'a>, interpreted: &'a Interpreted<'a>, mut rule_key: &'a RuleKey<'a>, match_stack: &[Match<'a>]) -> Vec<String> {
    // Find the first significant match from the match stack. This makes it so
    // that we skip creating meta contexts when they wouldn't have any effect.
    let first = match_stack.iter().enumerate()
        .find(|(_, match_)| !match_.remaining.is_empty()).map_or(match_stack.len(), |m| m.0);

    // Create a context for each item in the match stack that needs it.
    let mut contexts = vec!();

    for match_ in match_stack[first..].iter().rev() {
        if match_.remaining.is_empty() && match_.is_variable() {
            // If a match has no remaining nodes it can generally be ignored,
            // unless it has a meta scope and there are child matches that were
            // not ignored. In those cases we create a special meta context.
            // Meta scopes are ignored for transparent rules and repetitions.
            let rule_key = match &match_.expression {
                    Expression::Variable { key, .. } => key,
                    _ => panic!(),
                };

            let meta_content_scope = interpreted.rules[rule_key].options.scope.clone();

            if !meta_content_scope.is_empty() {
                let rule_meta_ctx_name = format!("{}|meta", build_rule_key_name(rule_key));

                if !state.contexts.contains_key(&rule_meta_ctx_name) {
                    state.contexts.insert(rule_meta_ctx_name.clone(), sublime_syntax::Context {
                        meta_scope: sublime_syntax::Scope::empty(),
                        meta_content_scope,
                        meta_include_prototype: true,
                        clear_scopes: sublime_syntax::ScopeClear::Amount(0),
                        matches: vec!(sublime_syntax::ContextPattern::Match(sublime_syntax::Match{
                            pattern: sublime_syntax::Pattern::from_str(""),
                            scope: sublime_syntax::Scope::empty(),
                            captures: HashMap::new(),
                            change_context: sublime_syntax::ContextChange::Pop(1),
                        })),
                    });
                }

                contexts.push(rule_meta_ctx_name);
            }
        } else if !match_.remaining.is_empty() {
            let context = collect_context_nodes_concatenation(interpreted, &match_.remaining);

            let key = ContextKey {
                rule_key: rule_key,
                context,
                branch_point: None,
            };

            if let Some(name) = state.context_cache.get(&key).map(|c| &c.name) {
                contexts.push(name.to_string());
            } else {
                let name = create_context_name(state, key.clone());
                gen_contexts(state, interpreted, vec!((name.clone(), key)));

                contexts.push(name);
            }
        }

        match &match_.expression {
            Expression::Variable { key, .. } => {
                rule_key = key;
            },
            _ => {},
        }
    }

    contexts
}

fn build_rule_key_name<'a>(rule_key: &RuleKey<'a>) -> String {
    let mut result = rule_key.name.to_string();

    // Encode arguments
    if !rule_key.arguments.is_empty() {
        result.push('@');

        // Arguments can be in any format, so convert them to a string
        // representation first and then base-64 encode them to make them safe
        // to use in a context name.
        let mut s = format!("[{}", rule_key.arguments[0]);
        for arg in &rule_key.arguments[1..] {
            s.push_str(&format!(", {}", arg));
        }
        s.push_str("]");

        let encoded = base64::encode_config(s.as_bytes(), base64::URL_SAFE_NO_PAD);
        result.push_str(&encoded);
    }

    result
}

// Generate a unique name for a context key
fn create_context_name<'a>(state: &mut State<'a>, key: ContextKey<'a>) -> String {
    let mut result = build_rule_key_name(key.rule_key);

    // Add inner context count to prevent context name collisions in inner contexts
    let index =
        if let Some(rule) = state.rules.get_mut(key.rule_key) {
            let i = rule.context_count;
            rule.context_count += 1;
            i
        } else {
            state.rules.insert(key.rule_key, Rule {
                context_count: 1,
                branch_point_count: 0,
            });
            0
        };
    result.push_str(&format!("|{}", index));

    // Add optional branch point
    if let Some(branch_point) = &key.branch_point {
        result.push('|');
        result.push_str(&branch_point);
    }

    let old_entry = state.context_cache.insert(key, ContextCacheEntry::new(result.clone()));
    assert!(old_entry.is_none());

    result
}

// Generate a new branch point for a rule
fn create_branch_point<'a>(state: &mut State<'a>, key: &'a RuleKey<'a>) -> String {
    let index =
        if let Some(rule) = state.rules.get_mut(key) {
            rule.branch_point_count += 1;
            rule.branch_point_count
        } else {
            state.rules.insert(key, Rule {
                context_count: 0,
                branch_point_count: 1,
            });
            1
        };

    format!("{}@{}", key.name, index)
}

fn match_stack_is_repetition<'a>(match_stack: &MatchStack<'a>) -> bool {
    for match_ in match_stack.iter().rev() {
        match match_.expression {
            Expression::Repetition { .. } => {
                return true;
            },
            Expression::Variable { .. } => {
                return false;
            },
            _ => {}
        }
    }

    return false;
}

fn rule_for_match_stack<'a>(rule_key: &'a RuleKey<'a>, match_stack: &[Match<'a>]) -> &'a RuleKey<'a> {
    for match_ in match_stack {
        match match_.expression {
            Expression::Variable { key, .. } => {
                return &key;
            },
            _ => {},
        }
    }

    rule_key
}

fn scope_for_match_stack<'a>(interpreted: &'a Interpreted<'a>, rule_key: Option<&'a RuleKey<'a>>, match_stack: &[Match<'a>]) -> sublime_syntax::Scope {
    let mut scopes = vec!();

    if let Some(rule_key) = rule_key {
        scopes = interpreted.rules[rule_key].options.scope.scopes.clone();
    }

    for match_ in match_stack.iter().rev() {
        match &match_.expression {
            Expression::Variable { key, .. } => {
                let rule_options = &interpreted.rules[key].options;

                scopes.extend(rule_options.scope.scopes.iter().cloned());
            },
            Expression::Terminal { options, .. } => {
                scopes.extend(options.scope.scopes.iter().cloned());
            },
            Expression::Repetition { .. } => {},
            _ => panic!()
        }
    }

    sublime_syntax::Scope::new(scopes)
}

// Collect the next context following the context stack
fn advance_context_stack<'a>(interpreted: &'a Interpreted<'a>, match_stack: &[Match<'a>]) -> Option<Context<'a>> {
    // Find the top most match with remaining expressions, collect the
    // concatenation of those expressions and add the rest of the stack onto the
    // back of the resulting match stacks.
    let result = match_stack.iter().enumerate().find(|(_, m)| !m.remaining.is_empty());

    if let Some((index, match_)) = result {
        // TODO: When the resulting context maybe empty then we should continue collecting down the stack
        let remaining_stack = &match_stack[index + 1..];

        let mut context = collect_context_nodes_concatenation(interpreted, &match_.remaining);

        for stack in &mut context.matches {
            stack.extend(remaining_stack.iter().cloned());
        }

        Some(context)
    } else {
        None
    }
}

// Determines how a context should end, ie. what the last match in a context
// should be.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ContextEnd<'a> {
    // Reaching the end of the context is illegal
    Illegal,
    // Ignore things that don't match, ie. don't do anything at the end of the context
    None,
    // If the end of the context is reached, push another context
    Push(Box<Context<'a>>),
}

type MatchStack<'a> = Vec<Match<'a>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Match<'a> {
    expression: &'a Expression<'a>,
    remaining: Vec<&'a Expression<'a>>,
}

impl<'a> Match<'a> {
    fn get_regex(&self) -> &'a str {
        match &self.expression {
            Expression::Terminal { regex, .. } => regex,
            _ => panic!()
        }
    }

    fn is_variable(&self) -> bool {
        match &self.expression {
            Expression::Variable { .. } => true,
            _ => false,
        }
    }

    fn is_repetition(&self) -> bool {
        match &self.expression {
            Expression::Repetition { .. } => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Context<'a> {
    matches: Vec<MatchStack<'a>>,
    end: ContextEnd<'a>,
    maybe_empty: bool,
}

impl<'a> Context<'a> {
    fn append(&mut self, mut other: Context<'a>) {
        self.matches.append(&mut other.matches);

        self.maybe_empty = self.maybe_empty || other.maybe_empty;

        match self.end {
            ContextEnd::Illegal => {
                self.end = other.end;
            },
            ContextEnd::None => {
                self.end =
                    match other.end {
                        ContextEnd::Illegal
                        | ContextEnd::None => ContextEnd::None,
                        ContextEnd::Push(ref mut _push) => {
                            // push.append_update_end(ContextEnd::None);
                            other.end
                        },
                    };
            },
            ContextEnd::Push(ref mut self_push) => {
                match other.end {
                    ContextEnd::Illegal | ContextEnd::None => {
                        // self_push.append_update_end(ContextEnd::None);
                    },
                    ContextEnd::Push(other_push) => {
                        self_push.append(*other_push);
                    },
                }
            }
        }
    }
}

// Transform and collect matches that the context for the expression needs to match
fn collect_context_nodes<'a>(interpreted: &'a Interpreted<'a>, expression: &'a Expression<'a>) -> Context<'a> {
    match expression {
        Expression::Variable { key, .. } => {
            let rule = interpreted.rules.get(key).unwrap();

            let mut context = collect_context_nodes(interpreted, &rule.expression);

            // Add the variable to the rule stack
            for match_stack in &mut context.matches {
                match_stack.push(Match {
                    expression,
                    remaining: vec!(),
                })
            }

            let end =
                match context.end {
                    ContextEnd::Illegal => ContextEnd::Illegal,
                    ContextEnd::None => ContextEnd::Push(Box::new(context.clone())),
                    ContextEnd::Push(_) => context.end.clone(),
                };

            Context {
                matches: context.matches,
                end,
                maybe_empty: context.maybe_empty,
            }
        },
        // A terminal is simply a context that matches the terminal's regex
        Expression::Terminal { .. } => {
            let mut match_ = MatchStack::new();
            match_.push(Match { expression, remaining: vec!() });

            Context {
                matches: vec!(match_),
                end: ContextEnd::Illegal,
                maybe_empty: false,
            }
        },
        // A passive context is one that doesn't match the end
        Expression::Passive { expression: child, .. } => {
            let context = collect_context_nodes(interpreted, &child);

            match context.end {
                ContextEnd::Illegal => {},
                // TODO
                _ => panic!(),
            }

            Context {
                matches: context.matches,
                end: ContextEnd::None,
                maybe_empty: context.maybe_empty,
            }
        },
        // A repeating context repeats each match
        Expression::Repetition { expression: child, .. } => {
            let mut context = collect_context_nodes(interpreted, &child);

            // Add the repetition to the front of each match stack
            for match_ in &mut context.matches {
                match_.push(Match { expression, remaining: vec!(expression) });
            }

            let end =
                match context.end {
                    ContextEnd::Illegal => ContextEnd::Illegal,
                    ContextEnd::None => ContextEnd::None,
                    _ => panic!(),
                };

            Context {
                matches: context.matches,
                end,
                maybe_empty: true,
            }
        },
        // An optional context is one which may match nothing
        Expression::Optional { expression: child, .. } => {
            let context = collect_context_nodes(interpreted, &child);

            match context.end {
                ContextEnd::Illegal => {
                    Context {
                        matches: context.matches,
                        end: context.end,
                        maybe_empty: true,
                    }
                },
                ContextEnd::None | ContextEnd::Push(_) => {
                    Context {
                        matches: vec!(),
                        end: ContextEnd::Push(Box::new(context)),
                        maybe_empty: true,
                    }
                },
            }
        },
        // An alternating context is a sum of matches
        Expression::Alternation { expressions, .. } => {
            let mut context = Context {
                matches: vec!(),
                end: ContextEnd::Illegal,
                maybe_empty: false,
            };

            for expression in expressions {
                context.append(collect_context_nodes(interpreted, &expression));
            }

            context
        },
        Expression::Concatenation { expressions, .. } => {
            let e = expressions.iter().collect::<Vec<_>>();

            collect_context_nodes_concatenation(interpreted, &e)
        },
    }
}

// A concatenation of contexts is the first context that can't be empty, with those before being alternations
fn collect_context_nodes_concatenation<'a>(interpreted: &'a Interpreted<'a>, expressions: &[&'a Expression<'a>]) -> Context<'a> {
    // Recursively consider the first expression until it may not be empty

    assert!(expressions.len() >= 1);

    // Base case
    if expressions.len() == 1 {
        return collect_context_nodes(interpreted, expressions[0]);
    }

    let first = expressions[0];
    let rest = &expressions[1..];

    let mut context = collect_context_nodes(interpreted, first);

    // Add the rest of the expressions onto the remaining expressions for each match
    for match_ in &mut context.matches {
        let last = match_.len() - 1;
        match_[last].remaining.extend(rest.iter().map(|n| *n));
    }

    if !context.maybe_empty {
        return context;
    }

    let mut next = collect_context_nodes_concatenation(interpreted, rest);

    context.end =
        match context.end {
            ContextEnd::Illegal => {
                match next.end {
                    ContextEnd::Illegal => ContextEnd::Illegal,
                    ContextEnd::None => {
                        ContextEnd::Push(Box::new(Context {
                            matches: next.matches.clone(),
                            end: ContextEnd::None,
                            maybe_empty: next.maybe_empty,
                        }))
                    },
                    _ => panic!(),
                }
            },
            ContextEnd::None => {
                match next.end {
                    ContextEnd::Illegal => {
                        ContextEnd::Push(Box::new(Context {
                            matches: context.matches.clone(),
                            end: ContextEnd::None,
                            maybe_empty: next.maybe_empty,
                        }))
                    },
                    ContextEnd::None => ContextEnd::None,
                    _ => panic!(),
                }
            },
            _ => panic!(),
        };

    context.matches.append(&mut next.matches);
    context.maybe_empty = next.maybe_empty;
    context
}

#[cfg(test)]
mod tests {
    extern crate matches;
    use matches::assert_matches;

    use crate::compiler::codegen::*;
    use crate::sbnf;
    use crate::compiler::{analysis, interpreter};

    fn collect_node<F>(source: &str, rule: &str, fun: F) where F: Fn(Context) -> () {
        let grammar = sbnf::parse(source).unwrap();

        let options = CompileOptions {
            name_hint: Some("test"),
            arguments: vec!(),
            debug_contexts: false,
            entry_points: vec!("m"),
        };

        let analysis_result = analysis::analyze(&options, &grammar);
        assert!(analysis_result.warnings.is_empty());

        let interpreter_result = interpreter::interpret(&options, analysis_result.result.unwrap());
        assert!(interpreter_result.warnings.is_empty());

        let interpreted = interpreter_result.result.as_ref().unwrap();
        let rule = &interpreted.rules[&interpreter::RuleKey { name: rule, arguments: vec!() }];
        let cn = collect_context_nodes(interpreted, &rule.expression);
        fun(cn);
    }

    #[derive(Debug, Clone)]
    enum TestExpression {
        Variable(&'static str),
        Terminal(&'static str),
        Passive(Box<TestExpression>),
        Repetition(Box<TestExpression>),
        Optional(Box<TestExpression>),
        Alternation(Vec<TestExpression>),
        Concatenation(Vec<TestExpression>),
    }

    impl<'a> PartialEq<TestExpression> for Expression<'a> {
        fn eq(&self, other: &TestExpression) -> bool {
            match (self, other) {
                (Expression::Variable { key, .. }, TestExpression::Variable(name))
                    => key.arguments.is_empty() && key.name == *name,

                (Expression::Terminal { regex, .. }, TestExpression::Terminal(oregex))
                    => {
                        regex == oregex
                    },

                (Expression::Passive { expression, .. }, TestExpression::Passive(child))
                | (Expression::Repetition { expression, .. }, TestExpression::Repetition(child))
                | (Expression::Optional { expression, .. }, TestExpression::Optional(child))
                    => **expression == **child,

                (Expression::Alternation { expressions, .. }, TestExpression::Alternation(children))
                | (Expression::Concatenation { expressions, .. }, TestExpression::Concatenation(children))
                    => expressions == children,

                _ => false,
            }
        }
    }

    fn variable(s: &'static str) -> TestExpression { TestExpression::Variable(s) }
    fn term(s: &'static str) -> TestExpression { TestExpression::Terminal(s) }
    fn passive(n: TestExpression) -> TestExpression {
        TestExpression::Passive(Box::new(n))
    }
    fn repetition(n: TestExpression) -> TestExpression {
        TestExpression::Repetition(Box::new(n))
    }
    fn optional(n: TestExpression) -> TestExpression {
        TestExpression::Optional(Box::new(n))
    }
    fn alternation(ns: &[TestExpression]) -> TestExpression {
        TestExpression::Alternation(ns.to_vec())
    }
    fn concatenation(ns: &[TestExpression]) -> TestExpression {
        TestExpression::Concatenation(ns.to_vec())
    }

    #[derive(Debug, Clone)]
    struct TestMatch {
        expression: TestExpression,
        remaining: Vec<TestExpression>,
    }

    impl<'a> PartialEq<TestMatch> for Match<'a> {
        fn eq(&self, other: &TestMatch) -> bool {
            *self.expression == other.expression && self.remaining.iter().map(|e| *e).eq(other.remaining.iter())
        }
    }

    fn m_term(s: &'static str, r: &[TestExpression]) -> TestMatch {
        TestMatch {
            expression: term(s),
            remaining: r.to_vec(),
        }
    }

    fn m_var(s: &'static str, r: &[TestExpression]) -> TestMatch {
        TestMatch {
            expression: variable(s),
            remaining: r.to_vec(),
        }
    }

    fn m_rep(n: TestExpression, r: &[TestExpression]) -> TestMatch {
        TestMatch {
            expression: repetition(n),
            remaining: r.to_vec(),
        }
    }

    #[test]
    fn collect_passive() {
        collect_node("m = ~'a';", "m", |ctx| {
            assert_matches!(ctx.end, ContextEnd::None);
            assert!(!ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[])),
            ]);
        });

        collect_node("m = ~'a' 'b';", "m", |ctx| {
            assert_matches!(ctx.end, ContextEnd::None);
            assert!(!ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[term("b")])),
            ]);
        });

        collect_node("m = ~'a'* ~'b';", "m", |ctx| {
            assert_matches!(ctx.end, ContextEnd::None);
            assert!(!ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[]), m_rep(term("a"), &[repetition(term("a")), passive(term("b"))])),
                vec!(m_term("b", &[])),
            ]);
        });

        collect_node("m = (~'a')* ~'b';", "m", |ctx| {
            assert_matches!(ctx.end, ContextEnd::None);
            assert!(!ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[]), m_rep(passive(term("a")), &[repetition(passive(term("a"))), passive(term("b"))])),
                vec!(m_term("b", &[])),
            ]);
        });

        collect_node("m = ~('a' | 'b') 'c';", "m", |ctx| {
            assert_matches!(ctx.end, ContextEnd::None);
            assert!(!ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[term("c")])),
                vec!(m_term("b", &[term("c")])),
            ]);
        });

        collect_node("m = ~'a'?;", "m", |ctx| {
            assert_matches!(ctx.end, ContextEnd::None);
            assert!(ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[])),
            ]);
        });

        collect_node("m = (~'a')?;", "m", |ctx| {
            match ctx.end {
                ContextEnd::Push(ctx2) => {
                    assert_matches!(ctx2.end, ContextEnd::None);
                    assert!(!ctx2.maybe_empty);
                    assert_eq!(ctx2.matches, [
                        vec!(m_term("a", &[])),
                    ]);
                },
                _ => panic!(),
            }
            assert!(ctx.maybe_empty);
            assert!(ctx.matches.is_empty());
        });

        collect_node("m = (~'a')* 'b';", "m", |ctx| {
            match ctx.end {
                ContextEnd::Push(ctx2) => {
                    assert_matches!(ctx2.end, ContextEnd::None);
                    assert!(!ctx2.maybe_empty);
                    assert_eq!(ctx2.matches, [
                        vec!(m_term("a", &[]), m_rep(passive(term("a")), &[repetition(passive(term("a"))), term("b")])),
                    ]);
                },
                _ => panic!(),
            }
            assert!(!ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[]), m_rep(passive(term("a")), &[repetition(passive(term("a"))), term("b")])),
                vec!(m_term("b", &[])),
            ]);
        });

        collect_node("m = 'a'? ~'b';", "m", |ctx| {
            match ctx.end {
                ContextEnd::Push(ctx2) => {
                    assert_matches!(ctx2.end, ContextEnd::None);
                    assert!(!ctx2.maybe_empty);
                    assert_eq!(ctx2.matches, [
                        vec!(m_term("b", &[])),
                    ]);
                },
                _ => panic!(),
            }
            assert!(!ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[passive(term("b"))])),
                vec!(m_term("b", &[])),
            ]);
        });
    }

    #[test]
    fn collect_repetition() {
        collect_node("m = 'a'*;", "m", |ctx| {
            assert_matches!(ctx.end, ContextEnd::Illegal);
            assert!(ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[]), m_rep(term("a"), &[repetition(term("a"))])),
            ]);
        });

        collect_node("m = ('a'? 'b' | 'c')*;", "m", |ctx| {
            assert_matches!(ctx.end, ContextEnd::Illegal);
            assert!(ctx.maybe_empty);
            let inner = alternation(&[
                concatenation(&[optional(term("a")), term("b")]),
                term("c"),
            ]);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[term("b")]), m_rep(inner.clone(), &[repetition(inner.clone())])),
                vec!(m_term("b", &[]), m_rep(inner.clone(), &[repetition(inner.clone())])),
                vec!(m_term("c", &[]), m_rep(inner.clone(), &[repetition(inner.clone())])),
            ]);
        });
    }

    #[test]
    fn collect_optional() {
        collect_node("m = 'a'?;", "m", |ctx| {
            assert_matches!(ctx.end, ContextEnd::Illegal);
            assert!(ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[])),
            ]);
        });

        collect_node("m = ('a' | 'b'* 'c')?;", "m", |ctx| {
            assert_matches!(ctx.end, ContextEnd::Illegal);
            assert!(ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[])),
                vec!(m_term("b", &[]), m_rep(term("b"), &[repetition(term("b")), term("c")])),
                vec!(m_term("c", &[])),
            ]);
        });
    }

    #[test]
    fn collect_alternation() {
        collect_node("m = 'a' | 'b';", "m", |ctx| {
            assert_matches!(ctx.end, ContextEnd::Illegal);
            assert!(!ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[])),
                vec!(m_term("b", &[])),
            ]);
        });

        collect_node("m = 'a' | 'b' 'c';", "m", |ctx| {
            assert_matches!(ctx.end, ContextEnd::Illegal);
            assert!(!ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[])),
                vec!(m_term("b", &[term("c")])),
            ]);
        });

        collect_node("m = 'a'? | 'b' | 'c'*;", "m", |ctx| {
            assert_matches!(ctx.end, ContextEnd::Illegal);
            assert!(ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[])),
                vec!(m_term("b", &[])),
                vec!(m_term("c", &[]), m_rep(term("c"), &[repetition(term("c"))])),
            ]);
        });
    }

    #[test]
    fn collect_concat() {
        collect_node("m = 'a' 'b';", "m", |ctx| {
            assert_matches!(ctx.end, ContextEnd::Illegal);
            assert!(!ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[term("b")])),
            ]);
        });

        collect_node("m = ('a' | 'b') 'c';", "m", |ctx| {
            assert_matches!(ctx.end, ContextEnd::Illegal);
            assert!(!ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[term("c")])),
                vec!(m_term("b", &[term("c")])),
            ]);
        });

        collect_node("m = 'a'? 'b'* 'c';", "m", |ctx| {
            assert_matches!(ctx.end, ContextEnd::Illegal);
            assert!(!ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[repetition(term("b")), term("c")])),
                vec!(m_term("b", &[]), m_rep(term("b"), &[repetition(term("b")), term("c")])),
                vec!(m_term("c", &[])),
            ]);
        });

        collect_node("m = 'a'* 'b'?;", "m", |ctx| {
            assert_matches!(ctx.end, ContextEnd::Illegal);
            assert!(ctx.maybe_empty);
            assert_eq!(ctx.matches, [
                vec!(m_term("a", &[]), m_rep(term("a"), &[repetition(term("a")), optional(term("b"))])),
                vec!(m_term("b", &[])),
            ]);
        });
    }
}
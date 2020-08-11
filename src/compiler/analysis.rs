use std::collections::HashMap;

use super::common::{parse_top_level_scope, trim_ascii, Error, CompileOptions, CompileResult};
use crate::sbnf::{Node, NodeData, Grammar};
use crate::sublime_syntax;

pub struct Analysis<'a> {
    pub rules: HashMap<&'a str, Vec<&'a Node<'a>>>,
    pub metadata: Metadata,
    pub debug_contexts: bool,
}

pub struct Metadata {
    pub name: String,
    pub file_extensions: Vec<String>,
    pub first_line_match: Option<sublime_syntax::Pattern>,
    pub scope: sublime_syntax::Scope,
    pub scope_postfix: String,
    pub hidden: bool,
}

struct State<'a> {
    errors: Vec<Error<'a>>,
    warnings: Vec<Error<'a>>,
}

pub fn analyze<'a>(options: &CompileOptions<'a>, grammar: &'a Grammar<'a>) -> CompileResult<'a, Analysis<'a>> {
    let mut state = State { errors: vec!(), warnings: vec!() };

    let metadata = collect_metadata(options, grammar, &mut state);

    let rules = collect_rules(grammar, &mut state);

    CompileResult::new(
        if state.errors.is_empty() {
            Ok(Analysis {
                rules,
                metadata,
                debug_contexts: options.debug_contexts,
            })
        } else {
            Err(state.errors)
        },
        state.warnings,
    )
}

fn collect_metadata<'a>(options: &CompileOptions<'a>, grammar: &'a Grammar<'a>, state: &mut State<'a>) -> Metadata {
    let mut name: Option<(&'a Node<'a>, String)> =  None;
    let mut file_extensions: Option<(&'a Node<'a>, String)> =  None;
    let mut first_line_match: Option<(&'a Node<'a>, String)> =  None;
    let mut scope: Option<(&'a Node<'a>, String)> =  None;
    let mut scope_postfix: Option<(&'a Node<'a>, String)> =  None;
    let mut hidden: Option<(&'a Node<'a>, bool)> =  None;

    for node in &grammar.nodes {
        match &node.data {
            NodeData::Header(value_node) => {
                let value = value_node.text;

                match node.text {
                    "name" => {
                        if let Some((first_node, _)) = name {
                            state.errors.push(Error::from_str(
                                "Duplicate 'name' header",
                                node,
                                vec!(
                                    (first_node, "already used here".to_string()),
                                    (node, "conflicts with first usage".to_string()),
                                )));
                        } else {
                            name = Some((node, trim_ascii(value).to_string()));
                        }
                    },
                    "extensions" => {
                        if let Some((first_node, _)) = file_extensions {
                            state.errors.push(Error::from_str(
                                "Duplicate 'extensions' header",
                                node,
                                vec!(
                                    (first_node, "already used here".to_string()),
                                    (node, "conflicts with first usage".to_string()),
                                )));
                        } else {
                            file_extensions = Some((node, value.to_string()));
                        }
                    },
                    "first-line" => {
                        if let Some((first_node, _)) = first_line_match {
                            state.errors.push(Error::from_str(
                                "Duplicate 'first-line' header",
                                node,
                                vec!(
                                    (first_node, "already used here".to_string()),
                                    (node, "conflicts with first usage".to_string()),
                                )));
                        } else {
                            first_line_match = Some((node, trim_ascii(value).to_string()));
                        }
                    },
                    "scope" => {
                        if let Some((first_node, _)) = scope {
                            state.errors.push(Error::from_str(
                                "Duplicate 'scope' header",
                                node,
                                vec!(
                                    (first_node, "already used here".to_string()),
                                    (node, "conflicts with first usage".to_string()),
                                )));
                        } else {
                            scope = Some((node, value.to_string()));
                        }
                    },
                    "scope-postfix" => {
                        if let Some((first_node, _)) = scope_postfix {
                            state.errors.push(Error::from_str(
                                "Duplicate 'scope-postfix' header",
                                node,
                                vec!(
                                    (first_node, "already used here".to_string()),
                                    (node, "conflicts with first usage".to_string()),
                                )));
                        } else {
                            scope_postfix = Some((node, trim_ascii(value).to_string()));
                        }
                    },
                    "hidden" => {
                        if let Some((first_node, _)) = hidden {
                            state.errors.push(Error::from_str(
                                "Duplicate 'hidden' header",
                                node,
                                vec!(
                                    (first_node, "already used here".to_string()),
                                    (node, "conflicts with first usage".to_string()),
                                )));
                        } else if let Some(v) = trim_ascii(value).parse::<bool>().ok() {
                            hidden = Some((node, v));
                        } else {
                            state.errors.push(Error::from_str(
                                "Invalid header value",
                                &value_node,
                                vec!((&value_node, "expected either 'true' or 'false'".to_string()))));
                        }
                    },
                    header => {
                        state.errors.push(Error::new(
                            format!("Unknown header '{}'", header),
                            node,
                            vec!((node, "".to_string()))));
                    }
                }
            },
            NodeData::Rule { .. } => {},
            _ => panic!(),
        }
    }

    // A name is required, either from a header or the name hint
    if options.name_hint.is_none() && name.is_none() {
        state.errors.push(Error::without_node(
            "No syntax name provided. Use a 'name' header to specify the name of the syntax".to_string(),
            vec!()));
    }

    let derived_name = name
        .map(|s| s.1)
        .or_else(|| options.name_hint.map(|s| trim_ascii(s).to_string()))
        .unwrap_or_else(|| "".to_string());

    let scope = scope.map_or_else(
            || sublime_syntax::Scope::new(
                vec!(format!("source.{}", derived_name.to_lowercase()))),
            |s| parse_top_level_scope(&s.1));

    let scope_postfix = scope_postfix.map_or_else(|| derived_name.to_lowercase(), |s| s.1);

    Metadata {
        name: derived_name,
        // File extensions are separated by whitespace
        file_extensions: file_extensions
            .map_or(
                vec!(),
                |s| s.1.split_ascii_whitespace()
                       .map(|s| s.to_string())
                       .collect::<Vec<String>>()),
        first_line_match: first_line_match.map(|s| sublime_syntax::Pattern::new(s.1)),
        // Use source.derived
        scope,
        scope_postfix,
        hidden: hidden.map_or(false, |s| s.1),
    }
}

fn collect_rules<'a>(grammar: &'a Grammar<'a>, state: &mut State<'a>) -> HashMap<&'a str, Vec<&'a Node<'a>>> {
    let mut rules: HashMap<&'a str, Vec<&'a Node<'a>>> = HashMap::new();

    for node in &grammar.nodes {
        match &node.data {
            NodeData::Header(_) => {},
            NodeData::Rule { parameters, .. } => {
                let name = &node.text;

                if let Some(overloads) = rules.get_mut(name) {
                    let first_node = &overloads[0];
                    let overload_params =
                        if let NodeData::Rule { parameters, .. } = &first_node.data {
                            parameters
                        } else {
                            panic!();
                        };

                    if overload_params.is_empty() {
                        state.errors.push(Error::new(
                            format!("Rule '{}' has already been defined", name),
                            node,
                            vec!(
                                (first_node, "already defined here".to_string()),
                                (node, "conflicts with first definition".to_string()),
                            )));
                    } else if overload_params.len() != parameters.len() {
                        state.errors.push(Error::from_str(
                            "Rules must have the same number of parameters",
                            node,
                            vec!(
                                (first_node, format!("has {} parameters", overload_params.len())),
                                (node, format!("has {} parameters", parameters.len())),
                            )));
                    } else {
                        overloads.push(node);
                    }
                } else {
                    rules.insert(name, vec!(node));
                }
            },
            _ => panic!(),
        }
    }

    rules
}

pub fn parse_scope(metadata: &Metadata, s: &str) -> sublime_syntax::Scope {
    let mut s = parse_top_level_scope(s);
    for scope in &mut s.scopes {
        let postfix = &metadata.scope_postfix;
        if !postfix.is_empty() {
            scope.push('.');
            scope.push_str(postfix);
        }
    }
    s
}

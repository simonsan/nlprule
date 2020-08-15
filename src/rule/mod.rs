use crate::composition::{Composition, Group, MatchGraph};
use crate::tokenizer::{tokenize, Token};
use crate::utils;
use log::{info, warn};
use regex::Regex;
use std::collections::HashSet;

pub mod from_structure;

#[derive(Debug)]
pub struct Suggestion {
    pub start: usize,
    pub end: usize,
    pub text: Vec<String>,
}

impl std::cmp::PartialEq for Suggestion {
    fn eq(&self, other: &Suggestion) -> bool {
        let a: HashSet<&String> = self.text.iter().collect();
        let b: HashSet<&String> = other.text.iter().collect();

        a.intersection(&b).count() > 0 && other.start == self.start && other.end == self.end
    }
}

#[derive(Debug)]
pub struct Test {
    text: String,
    suggestion: Option<Suggestion>,
}

pub struct Match {
    id: usize,
    conversion: Box<dyn Fn(&str) -> String>,
    regex_replacer: Option<(Regex, String)>,
    has_conversion: bool,
}

impl std::fmt::Debug for Match {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(fmt, "Match {{ index: {:?} }}", self.id)?;
        Ok(())
    }
}

impl Match {
    fn apply(&self, graph: &MatchGraph) -> String {
        let text = graph
            .by_id(self.id)
            .unwrap_or_else(|| panic!("group must exist in graph: {}", self.id))
            .tokens
            .get(0)
            .map(|x| x.text)
            .unwrap_or("");

        if let Some((regex, replacement)) = &self.regex_replacer {
            let replaced = regex.replace_all(text, replacement.as_str());
            (self.conversion)(replaced.as_ref())
        } else {
            (self.conversion)(text)
        }
    }

    fn new(
        id: usize,
        conversion: Option<Box<dyn Fn(&str) -> String>>,
        regex_replacer: Option<(Regex, String)>,
    ) -> Self {
        Match {
            id,
            has_conversion: conversion.is_some(),
            conversion: conversion.unwrap_or_else(|| Box::new(|x: &str| x.to_string())),
            regex_replacer,
        }
    }

    fn has_conversion(&self) -> bool {
        self.has_conversion
    }
}

#[derive(Debug)]
pub enum SuggesterPart {
    Text(String),
    Match(Match),
}

#[derive(Debug)]
pub struct Suggester {
    parts: Vec<SuggesterPart>,
}

impl Suggester {
    fn apply(&self, groups: &MatchGraph, start_group: &Group, _end_group: &Group) -> String {
        let mut output = Vec::new();
        let mut matchers_have_conversion = false;

        for part in &self.parts {
            match part {
                SuggesterPart::Text(t) => output.push(t.clone()),
                SuggesterPart::Match(m) => {
                    if m.has_conversion() {
                        matchers_have_conversion = true;
                    };
                    output.push(m.apply(groups));
                }
            }
        }

        let suggestion = utils::normalize_whitespace(&output.join(""));

        // if the suggestion contains no case conversion matches, make it title case if:
        // * at sentence start
        // * the replaced text is title case
        if !matchers_have_conversion
            && !start_group.tokens.is_empty()
            && (start_group.tokens[0]
                .text
                .chars()
                .next()
                .expect("token must have at least one char")
                .is_uppercase())
        {
            utils::apply_to_first(&suggestion, |x| x.to_uppercase().collect())
        } else {
            suggestion
        }
    }
}

pub struct Rule {
    id: String,
    composition: Composition,
    antipatterns: Vec<Composition>,
    tests: Vec<Test>,
    suggesters: Vec<Suggester>,
    start: usize,
    end: usize,
}

impl Rule {
    pub fn set_id(&mut self, id: String) {
        self.id = id;
    }

    pub fn apply<'a>(&self, tokens: &[Token<'a>]) -> Vec<Suggestion> {
        let refs: Vec<&Token> = tokens.iter().collect();
        let mut suggestions = Vec::new();

        let mut range_mask: Vec<_> = vec![
            false;
            tokens
                .get(tokens.len() - 1)
                .map(|x| x.char_span.1)
                .unwrap_or(0)
        ];

        for i in 0..tokens.len() {
            if let Some(graph) = self.composition.apply(&refs, i) {
                let start_group = graph.by_id(self.start).unwrap_or_else(|| {
                    panic!("{} group must exist in graph: {}", self.id, self.start)
                });
                let end_group = graph.by_id(self.end - 1).unwrap_or_else(|| {
                    panic!("{} group must exist in graph: {}", self.id, self.end - 1)
                });

                let start = start_group.char_start;
                let end = end_group.char_end;

                // only add the suggestion if we don't have any yet from this rule in its range
                if !range_mask[start..end].iter().any(|x| *x) {
                    let mut blocked = false;

                    // TODO: cache / move to outer loop
                    for i in 0..tokens.len() {
                        for antipattern in &self.antipatterns {
                            if let Some(anti_graph) = antipattern.apply(&refs, i) {
                                let anti_start = anti_graph.by_index(0).char_start;
                                let anti_end = anti_graph.by_index(anti_graph.len() - 1).char_end;

                                let rule_start = graph.by_index(0).char_start;
                                let rule_end = graph.by_index(graph.len() - 1).char_end;

                                if anti_start <= rule_end && rule_start <= anti_end {
                                    blocked = true;
                                    break;
                                }
                            }
                        }
                        if blocked {
                            break;
                        }
                    }

                    if !blocked {
                        range_mask[start..end].iter_mut().for_each(|x| *x = true);

                        suggestions.push(Suggestion {
                            start,
                            end,
                            text: self
                                .suggesters
                                .iter()
                                .map(|x| x.apply(&graph, start_group, end_group))
                                .collect(),
                        });
                    }
                }
            }
        }

        suggestions
    }

    pub fn test(&self) -> bool {
        let mut passes = Vec::new();

        for test in &self.tests {
            let tokens = tokenize(&test.text);
            info!("Tokens: {:#?}", tokens);
            let suggestions = self.apply(&tokens);

            let pass = if suggestions.len() > 1 {
                false
            } else {
                match &test.suggestion {
                    Some(correct_suggestion) => {
                        suggestions.len() == 1 && correct_suggestion == &suggestions[0]
                    }
                    None => suggestions.is_empty(),
                }
            };

            if !pass {
                warn!(
                    "Rule {}: test \"{}\" failed. Expected: {:#?}. Found: {:#?}.",
                    self.id, test.text, test.suggestion, suggestions
                );
            }

            passes.push(pass);
        }

        passes.iter().all(|x| *x)
    }
}

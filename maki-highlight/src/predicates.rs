//! The tree-sitter query predicates tree-sitter leaves to its caller, shared
//! by highlighting and `maki.treesitter.query`.

use std::collections::HashMap;

use regex::{Error as RegexError, Regex};
use tree_sitter::{Query, QueryCapture, QueryPredicateArg};

const KNOWN_PREDICATES: &[&str] = &[
    "eq?",
    "match?",
    "lua-match?",
    "contains?",
    "has-ancestor?",
    "has-parent?",
];

#[derive(Clone, Copy)]
struct PredicateModifiers {
    negated: bool,
    any: bool,
}

fn parse_predicate_op(op: &str) -> (PredicateModifiers, &str) {
    let (negated, rest) = op
        .strip_prefix("not-")
        .map(|r| (true, r))
        .unwrap_or((false, op));
    let (any, base) = rest
        .strip_prefix("any-")
        .map(|r| (true, r))
        .unwrap_or((false, rest));
    (PredicateModifiers { negated, any }, base)
}

/// Whether {op}, with its `not-` and `any-` prefixes, is one
/// [`predicates_hold`] knows. Tree-sitter itself only checks `eq?`,
/// `match?` and `any-of?`, and hands the rest over untouched.
pub fn is_known_predicate(op: &str) -> bool {
    KNOWN_PREDICATES.contains(&parse_predicate_op(op).1)
}

/// The first regex of a `match?` or `lua-match?` in {pattern}, `not-` and
/// `any-` forms included, that [`predicates_hold`] could not compile, since
/// there it only makes the predicate fail.
pub fn broken_regex(query: &Query, pattern: usize) -> Option<(&str, RegexError)> {
    query
        .general_predicates(pattern)
        .iter()
        .find_map(|predicate| {
            let (_, base_op) = parse_predicate_op(&predicate.operator);
            let ("match?" | "lua-match?", Some(QueryPredicateArg::String(regex))) =
                (base_op, predicate.args.get(1))
            else {
                return None;
            };
            Regex::new(regex).err().map(|e| (regex.as_ref(), e))
        })
}

/// The predicates tree-sitter leaves to its caller. `#set!` never gets
/// here: tree-sitter files it under the pattern's property settings.
pub fn predicates_hold(
    query: &Query,
    pattern_index: usize,
    captures: &[QueryCapture<'_>],
    source: &[u8],
    regex_cache: &mut HashMap<String, Option<Regex>>,
) -> bool {
    for predicate in query.general_predicates(pattern_index) {
        let (mods, base_op) = parse_predicate_op(predicate.operator.as_ref());
        let args = &predicate.args;

        match base_op {
            "eq?" if eval_eq(captures, source, args, mods.any) == mods.negated => {
                return false;
            }
            "match?" | "lua-match?"
                if eval_match(captures, source, args, mods.any, regex_cache) == mods.negated =>
            {
                return false;
            }
            "contains?" if eval_contains(captures, source, args, mods.any) == mods.negated => {
                return false;
            }
            "any-of?" if eval_any_of(captures, source, args) == mods.negated => {
                return false;
            }
            "has-ancestor?" if eval_has_ancestor(captures, args) == mods.negated => {
                return false;
            }
            "has-parent?" if eval_has_parent(captures, args) == mods.negated => {
                return false;
            }
            _ => {}
        }
    }
    true
}

fn capture_text<'a>(captures: &[QueryCapture<'_>], source: &'a [u8], idx: u32) -> Option<&'a str> {
    captures
        .iter()
        .find(|c| c.index == idx)
        .and_then(|c| std::str::from_utf8(&source[c.node.start_byte()..c.node.end_byte()]).ok())
}

fn resolve_arg<'a>(
    captures: &[QueryCapture<'_>],
    source: &'a [u8],
    arg: &'a QueryPredicateArg,
) -> Option<&'a str> {
    match arg {
        QueryPredicateArg::Capture(idx) => capture_text(captures, source, *idx),
        QueryPredicateArg::String(s) => Some(s.as_ref()),
    }
}

fn eval_eq(
    captures: &[QueryCapture<'_>],
    source: &[u8],
    args: &[QueryPredicateArg],
    any: bool,
) -> bool {
    let (Some(lhs), Some(rhs)) = (
        args.first().and_then(|a| resolve_arg(captures, source, a)),
        args.get(1).and_then(|a| resolve_arg(captures, source, a)),
    ) else {
        return false;
    };
    if any {
        lhs.lines().any(|line| line == rhs)
    } else {
        lhs == rhs
    }
}

fn eval_match(
    captures: &[QueryCapture<'_>],
    source: &[u8],
    args: &[QueryPredicateArg],
    any: bool,
    regex_cache: &mut HashMap<String, Option<Regex>>,
) -> bool {
    let Some(text) = args.first().and_then(|a| resolve_arg(captures, source, a)) else {
        return false;
    };
    let Some(QueryPredicateArg::String(pattern)) = args.get(1) else {
        return false;
    };
    let re = regex_cache.entry(pattern.to_string()).or_insert_with(|| {
        Regex::new(pattern.as_ref())
            .inspect_err(|_| tracing::debug!(pattern = pattern.as_ref(), "invalid regex predicate"))
            .ok()
    });
    let Some(re) = re else { return false };
    if any {
        text.lines().any(|line| re.is_match(line))
    } else {
        re.is_match(text)
    }
}

fn eval_contains(
    captures: &[QueryCapture<'_>],
    source: &[u8],
    args: &[QueryPredicateArg],
    any: bool,
) -> bool {
    let Some(text) = args.first().and_then(|a| resolve_arg(captures, source, a)) else {
        return false;
    };
    let Some(needle) = args.get(1).and_then(|a| resolve_arg(captures, source, a)) else {
        return false;
    };
    if any {
        text.lines().any(|line| line.contains(needle))
    } else {
        text.contains(needle)
    }
}

fn eval_any_of(captures: &[QueryCapture<'_>], source: &[u8], args: &[QueryPredicateArg]) -> bool {
    let Some(QueryPredicateArg::Capture(idx)) = args.first() else {
        return false;
    };
    let Some(text) = capture_text(captures, source, *idx) else {
        return false;
    };
    args[1..].iter().any(|arg| match arg {
        QueryPredicateArg::String(s) => text == s.as_ref(),
        QueryPredicateArg::Capture(idx) => {
            capture_text(captures, source, *idx).is_some_and(|t| t == text)
        }
    })
}

fn eval_has_ancestor(captures: &[QueryCapture<'_>], args: &[QueryPredicateArg]) -> bool {
    let Some(QueryPredicateArg::Capture(idx)) = args.first() else {
        return false;
    };
    let Some(QueryPredicateArg::String(type_name)) = args.get(1) else {
        return false;
    };
    let Some(cap) = captures.iter().find(|c| c.index == *idx) else {
        return false;
    };
    let mut current = cap.node.parent();
    while let Some(parent) = current {
        if parent.kind() == type_name.as_ref() {
            return true;
        }
        current = parent.parent();
    }
    false
}

fn eval_has_parent(captures: &[QueryCapture<'_>], args: &[QueryPredicateArg]) -> bool {
    let Some(QueryPredicateArg::Capture(idx)) = args.first() else {
        return false;
    };
    let Some(QueryPredicateArg::String(type_name)) = args.get(1) else {
        return false;
    };
    let Some(cap) = captures.iter().find(|c| c.index == *idx) else {
        return false;
    };
    cap.node
        .parent()
        .is_some_and(|p| p.kind() == type_name.as_ref())
}

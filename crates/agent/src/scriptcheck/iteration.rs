use std::collections::{HashMap, HashSet};

use super::python::{
    self, Arg, Kind, Statement, Token, argument, comma_parts, comprehension, primary_expression,
};

pub(super) fn finite_comprehension_source(tokens: &[Token<'_>]) -> bool {
    if let Some((_, sources)) = comprehension(tokens) {
        return sources.is_ok_and(|sources| sources.into_iter().all(finite_comprehension_source));
    }
    match tokens {
        [token] => match &token.kind {
            Kind::Group('(', inner) => {
                inner.is_empty()
                    || inner.iter().any(|token| token.is(","))
                    || finite_comprehension_source(inner)
            }
            Kind::String(_) | Kind::Group('[' | '{', _) => true,
            _ => token.name().is_some(),
        },
        [name, args] if name.name().is_some() && args.group('(').is_some() => {
            let args = args.group('(').unwrap();
            match name.name().unwrap() {
                "range" => (1..=3).contains(&comma_parts(args).count())
                    && comma_parts(args).map(argument).all(
                        |arg| matches!(arg, Arg::Positional(value) if integer_expression(value)),
                    ),
                wrapper @ ("enumerate" | "sorted" | "reversed" | "list" | "tuple" | "set"
                | "zip" | "map" | "filter") => finite_wrapper(wrapper, args),
                _ => true,
            }
        }
        // 已解析的主表达式涵盖属性、下标以及任意接收者上的调用。
        _ => primary_expression(tokens),
    }
}

fn integer_expression(tokens: &[Token<'_>]) -> bool {
    !tokens.is_empty()
        && tokens.iter().all(|token| match &token.kind {
            Kind::Number(value) => {
                matches!(
                    value.get(..2),
                    Some("0x" | "0X" | "0o" | "0O" | "0b" | "0B")
                ) || value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b'_')
            }
            Kind::Op("+" | "-" | "~" | "*" | "//" | "%" | "**" | "<<" | ">>" | "|" | "^" | "&") => {
                true
            }
            Kind::Group('(', inner) => integer_expression(inner),
            _ => false,
        })
}

fn finite_wrapper(wrapper: &str, args: &[Token<'_>]) -> bool {
    if args.is_empty() {
        return matches!(wrapper, "list" | "tuple" | "set" | "zip");
    }
    let mut inputs = comma_parts(args).map(argument).filter_map(|arg| match arg {
        Arg::Positional(input) | Arg::Star(input) => Some(input),
        Arg::Keyword("iterable", input) if wrapper == "enumerate" => Some(input),
        _ => None,
    });
    match wrapper {
        "zip" => inputs.all(finite_comprehension_source),
        "map" => {
            // 第一个参数是映射函数，之后的每个位置参数都是迭代源。
            inputs.next();
            inputs.all(finite_comprehension_source)
        }
        "filter" => inputs.nth(1).is_some_and(finite_comprehension_source),
        _ => inputs.next().is_some_and(finite_comprehension_source),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ItertoolsFactory {
    Unbounded,
    Repeat,
}

impl ItertoolsFactory {
    fn named(name: &str) -> Option<Self> {
        match name {
            "count" | "cycle" => Some(Self::Unbounded),
            "repeat" => Some(Self::Repeat),
            _ => None,
        }
    }

    fn allows(self, tail: &[Token<'_>]) -> bool {
        self == Self::Repeat
            && tail
                .first()
                .and_then(|token| token.group('('))
                .is_some_and(|args| {
                    let mut positional = 0;
                    let mut has_times = false;
                    for arg in comma_parts(args).map(argument) {
                        match arg {
                            Arg::Positional(value) if !value.is_empty() => positional += 1,
                            Arg::Keyword("times", _) => has_times = true,
                            Arg::Star(_) | Arg::DoubleStar(_) => return false,
                            _ => {}
                        }
                    }
                    positional >= 2 || has_times
                })
    }
}

// 在构造点拦截无界迭代器，覆盖先赋值再迭代、容器包装和调用结果。
pub(super) struct IteratorCalls<'a> {
    modules: HashSet<&'a str>,
    factories: HashMap<&'a str, ItertoolsFactory>,
}

impl<'a> IteratorCalls<'a> {
    pub(super) fn new(suite: &[Statement<'a>]) -> Self {
        let mut calls = Self {
            modules: HashSet::from(["itertools"]),
            factories: HashMap::new(),
        };
        for stmt in suite {
            calls.collect_imports(stmt);
        }
        calls
    }

    fn collect_imports(&mut self, stmt: &Statement<'a>) {
        for entry in python::imports(stmt.head).into_iter().flatten() {
            if entry.module != "itertools" {
                continue;
            }
            if let Some(symbol) = entry.symbol {
                if let Some(factory) = ItertoolsFactory::named(symbol) {
                    self.factories
                        .entry(entry.alias.unwrap_or(symbol))
                        .and_modify(|existing| {
                            if factory == ItertoolsFactory::Unbounded {
                                *existing = factory;
                            }
                        })
                        .or_insert(factory);
                }
            } else {
                self.modules.insert(entry.alias.unwrap_or("itertools"));
            }
        }
        for child in stmt.children() {
            self.collect_imports(child);
        }
    }

    pub(super) fn allows(&self, name: &str, prev: Option<&Token<'_>>, tail: &[Token<'_>]) -> bool {
        if prev.is_some_and(|t| t.is(".")) {
            return true;
        }
        if let Some(factory) = self.factories.get(name) {
            return factory.allows(tail);
        }
        if self.modules.contains(name) {
            return matches!(tail, [dot, method, args @ ..] if dot.is(".")
                && method.name().and_then(ItertoolsFactory::named).is_none_or(|f| f.allows(args)));
        }
        if name == "iter" {
            return tail
                .first()
                .and_then(|token| token.group('('))
                .is_some_and(|args| {
                    let mut parts = comma_parts(args).map(argument);
                    matches!(parts.next(), Some(Arg::Positional(value)) if !value.is_empty())
                        && parts.next().is_none()
                });
        }
        true
    }
}

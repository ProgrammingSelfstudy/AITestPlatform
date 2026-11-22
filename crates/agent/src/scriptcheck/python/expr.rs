use super::{Kind, Token};

pub(in crate::scriptcheck) fn split<'a>(
    tokens: &'a [Token<'a>],
    sep: &str,
) -> Option<(&'a [Token<'a>], &'a [Token<'a>])> {
    let index = tokens.iter().position(|token| token.is(sep))?;
    Some((&tokens[..index], &tokens[index + 1..]))
}

pub(in crate::scriptcheck) fn comma_parts<'a>(
    tokens: &'a [Token<'a>],
) -> impl Iterator<Item = &'a [Token<'a>]> {
    let end = tokens.len() - usize::from(tokens.last().is_some_and(|token| token.is(",")));
    tokens[..end].split(|token| token.is(","))
}

pub(in crate::scriptcheck) enum Arg<'a> {
    Positional(&'a [Token<'a>]),
    Keyword(&'a str, &'a [Token<'a>]),
    Star(&'a [Token<'a>]),
    DoubleStar(&'a [Token<'a>]),
}

pub(in crate::scriptcheck) fn argument<'a>(part: &'a [Token<'a>]) -> Arg<'a> {
    match part {
        [first, tail @ ..] if first.is("*") => Arg::Star(tail),
        [first, tail @ ..] if first.is("**") => Arg::DoubleStar(tail),
        [name, op, tail @ ..] if op.is("=") && name.name().is_some() => {
            Arg::Keyword(name.name().unwrap(), tail)
        }
        _ => Arg::Positional(part),
    }
}

pub(super) fn expression(tokens: &[Token<'_>]) -> bool {
    let mut parser = Expr { tokens, offset: 0 };
    parser.binary(1) && parser.offset == tokens.len()
}

pub(in crate::scriptcheck) fn primary_expression(tokens: &[Token<'_>]) -> bool {
    let mut parser = Expr { tokens, offset: 0 };
    parser.primary() && parser.offset == tokens.len()
}

pub(super) fn expression_list(tokens: &[Token<'_>]) -> bool {
    !tokens.is_empty() && comma_parts(tokens).all(expression)
}

struct Expr<'a> {
    tokens: &'a [Token<'a>],
    offset: usize,
}

impl Expr<'_> {
    fn eat(&mut self, text: &str) -> bool {
        let matches = self
            .tokens
            .get(self.offset)
            .is_some_and(|token| token.is(text));
        self.offset += usize::from(matches);
        matches
    }

    fn binary(&mut self, min: u8) -> bool {
        // 一元 not 的优先级低于比较，其余一元运算高于乘除。
        if min <= 3 && self.eat("not") {
            while self.eat("not") {}
            if !self.binary(4) {
                return false;
            }
        } else {
            while self.eat("+") || self.eat("-") || self.eat("~") {}
            if !self.primary() {
                return false;
            }
        }
        while let Some((priority, size)) = operator(&self.tokens[self.offset..]) {
            if priority < min {
                break;
            }
            self.offset += size;
            if !self.binary(priority + 1) {
                return false;
            }
        }
        // 条件表达式限制为一个分支。多层条件可用括号明确分组。
        if min == 1 && self.eat("if") {
            return self.binary(2) && self.eat("else") && self.binary(2);
        }
        true
    }

    fn primary(&mut self) -> bool {
        let Some(token) = self.tokens.get(self.offset) else {
            return false;
        };
        self.offset += 1;
        match &token.kind {
            Kind::Number(_) => {}
            Kind::Word(_)
                if token.name().is_some()
                    || ["True", "False", "None"].iter().any(|s| token.is(s)) => {}
            Kind::Op("...") => {}
            Kind::String(literal) => {
                if !literal.fields.iter().all(|field| expression_list(field)) {
                    return false;
                }
                while let Some(Token {
                    kind: Kind::String(next),
                    ..
                }) = self.tokens.get(self.offset)
                {
                    if !next.fields.iter().all(|field| expression_list(field)) {
                        return false;
                    }
                    self.offset += 1;
                }
            }
            Kind::Group(kind, inner) if display(*kind, inner) => {}
            _ => return false,
        }
        while let Some(next) = self.tokens.get(self.offset) {
            match &next.kind {
                Kind::Op(".") => {
                    self.offset += 1;
                    if self.tokens.get(self.offset).and_then(Token::name).is_none() {
                        return false;
                    }
                }
                Kind::Group('(', args) => {
                    if !call(args) {
                        return false;
                    }
                }
                Kind::Group('[', indexes) => {
                    if indexes.is_empty()
                        || !comma_parts(indexes).all(|part| {
                            let fields: Vec<_> = part.split(|token| token.is(":")).collect();
                            if fields.len() == 1 {
                                expression(part)
                            } else {
                                fields.len() <= 3
                                    && fields
                                        .iter()
                                        .all(|field| field.is_empty() || expression(field))
                            }
                        })
                    {
                        return false;
                    }
                }
                _ => break,
            }
            self.offset += 1;
        }
        true
    }
}

fn operator(tokens: &[Token<'_>]) -> Option<(u8, usize)> {
    let [first, ..] = tokens else { return None };
    let second = tokens.get(1);
    let priority = match &first.kind {
        Kind::Word("or") => 1,
        Kind::Word("and") => 2,
        Kind::Word("not") if second.is_some_and(|t| t.is("in")) => return Some((4, 2)),
        Kind::Word("is") if second.is_some_and(|t| t.is("not")) => return Some((4, 2)),
        Kind::Word("in" | "is") | Kind::Op("==" | "!=" | "<" | "<=" | ">" | ">=") => 4,
        Kind::Op("|") => 5,
        Kind::Op("^") => 6,
        Kind::Op("&") => 7,
        Kind::Op("<<" | ">>") => 8,
        Kind::Op("+" | "-") => 9,
        Kind::Op("*" | "@" | "/" | "//" | "%") => 10,
        Kind::Op("**") => 11,
        _ => return None,
    };
    Some((priority, 1))
}

pub(super) fn call(tokens: &[Token<'_>]) -> bool {
    if tokens.is_empty() {
        return true;
    }
    if let Some((head, iterators)) = comprehension(tokens) {
        return expression(head) && iterators.is_ok();
    }
    let mut keywords = Vec::new();
    let mut double_star = false;
    comma_parts(tokens).all(|part| match argument(part) {
        Arg::Positional(expr) => keywords.is_empty() && !double_star && expression(expr),
        Arg::Star(expr) => !double_star && expression(expr),
        Arg::DoubleStar(expr) => {
            double_star = true;
            expression(expr)
        }
        Arg::Keyword(name, expr) => {
            if keywords.contains(&name) {
                return false;
            }
            keywords.push(name);
            expression(expr)
        }
    })
}

fn display(opening: char, tokens: &[Token<'_>]) -> bool {
    if tokens.is_empty() {
        return true;
    }
    if let Some((head, iterators)) = comprehension(tokens) {
        return (if opening == '{' {
            dict_entry(head).unwrap_or_else(|| expression(head))
        } else {
            expression(head)
        }) && iterators.is_ok();
    }
    let mut is_dict = None;
    comma_parts(tokens).all(|part| {
        let entry = (opening == '{').then(|| dict_entry(part)).flatten();
        let dict = *is_dict.get_or_insert(entry.is_some());
        match (dict, entry) {
            (true, Some(valid)) => valid,
            (false, None) => expression(part),
            _ => false,
        }
    })
}

fn dict_entry(part: &[Token<'_>]) -> Option<bool> {
    let (key, value) = split(part, ":")?;
    Some(expression(key) && expression(value))
}

type IteratorSources<'a> = Result<Vec<&'a [Token<'a>]>, ()>;

pub(in crate::scriptcheck) fn comprehension<'a>(
    tokens: &'a [Token<'a>],
) -> Option<(&'a [Token<'a>], IteratorSources<'a>)> {
    let (head, mut clause) = split(tokens, "for")?;
    let mut iterators = Vec::new();
    loop {
        let Some((lhs, rhs)) = split(clause, "in") else {
            break;
        };
        if !target(lhs) {
            break;
        }
        let mut segments = rhs.split(|token| token.is("if") || token.is("for"));
        let iterable = segments.next().unwrap();
        if !expression(iterable) {
            break;
        }
        iterators.push(iterable);
        let mut tail = &rhs[iterable.len()..];
        while tail.first().is_some_and(|token| token.is("if")) {
            let filter = segments.next().unwrap();
            if !expression(filter) {
                return Some((head, Err(())));
            }
            tail = &tail[1 + filter.len()..];
        }
        match tail.split_first() {
            Some((token, next)) if token.is("for") => clause = next,
            None => return Some((head, Ok(iterators))),
            _ => break,
        }
    }
    Some((head, Err(())))
}

pub(super) fn target(tokens: &[Token<'_>]) -> bool {
    !tokens.is_empty()
        && comma_parts(tokens).all(|part| {
            let Some((first, attrs)) = part.split_first() else {
                return false;
            };
            if attrs.is_empty() {
                return first.name().is_some()
                    || first
                        .group('(')
                        .or_else(|| first.group('['))
                        .is_some_and(target);
            }
            first.name().is_some()
                && attrs
                    .chunks(2)
                    .all(|pair| matches!(pair, [dot, name] if dot.is(".") && name.name().is_some()))
        })
}

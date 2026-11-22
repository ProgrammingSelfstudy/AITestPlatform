//! 生成脚本使用的 Python 子集。这里只解析源码，超出子集的写法会被拒绝。

mod expr;
mod lexer;

use std::collections::HashSet;

use expr::{call, expression, expression_list, target};

pub(super) use expr::{Arg, argument, comma_parts, comprehension, primary_expression, split};
pub(super) use lexer::{Kind, Line, Token, lex};

/// 表达式里只有数字、运算符和括号，没有任何名字参与。
pub(super) fn literal_only(tokens: &[Token<'_>]) -> bool {
    !tokens.is_empty()
        && tokens.iter().all(|token| match &token.kind {
            Kind::Number(_) | Kind::Op(_) => true,
            Kind::Group(_, inner) => literal_only(inner) || inner.is_empty(),
            _ => false,
        })
}

pub(super) struct SyntaxError {
    pub line: usize,
    pub message: &'static str,
}

fn syntax(line: usize, message: &'static str) -> SyntaxError {
    SyntaxError { line, message }
}

pub(super) struct Statement<'a> {
    pub head: &'a [Token<'a>],
    pub body: Vec<Statement<'a>>,
    pub clauses: Vec<Statement<'a>>,
}

impl<'a> Statement<'a> {
    pub fn is(&self, keyword: &str) -> bool {
        self.head[0].is(keyword)
    }

    pub fn line(&self) -> usize {
        self.head[0].line
    }

    pub fn children(&self) -> impl Iterator<Item = &Statement<'a>> {
        self.body.iter().chain(&self.clauses)
    }
}

#[derive(Clone, Copy, Default)]
struct Scope {
    function: bool,
    in_loop: bool,
}

type Lines<'a> = std::iter::Peekable<std::slice::Iter<'a, Line<'a>>>;

pub(super) fn parse<'a>(lines: &'a [Line<'a>]) -> Result<Vec<Statement<'a>>, SyntaxError> {
    suite(&mut lines.iter().peekable(), 0, Scope::default(), 0)
}

fn suite<'a>(
    lines: &mut Lines<'a>,
    indent: usize,
    scope: Scope,
    depth: usize,
) -> Result<Vec<Statement<'a>>, SyntaxError> {
    let mut statements: Vec<Statement<'a>> = Vec::new();
    let mut decorator = None;
    while let Some(line) = lines.next_if(|line| line.indent >= indent) {
        if line.indent != indent || depth >= 64 {
            return Err(syntax(line.number, "缩进不一致或语句块嵌套过深"));
        }
        let tokens = &line.tokens;
        let first = &tokens[0];
        if first.is("@") {
            if !expression(&tokens[1..]) {
                return Err(syntax(line.number, "装饰器表达式无效"));
            }
            decorator.get_or_insert(line.number);
            statements.push(Statement {
                head: tokens,
                body: vec![],
                clauses: vec![],
            });
            continue;
        }
        if let Some(number) = decorator.take()
            && !first.is("def")
            && !first.is("class")
        {
            return Err(syntax(number, "装饰器后必须紧跟函数或类"));
        }
        if !["def", "class", "if", "elif", "else", "for", "while", "try"]
            .iter()
            .any(|s| first.is(s))
        {
            statements.extend(simple_line(tokens, scope)?);
            continue;
        }
        let previous = statements.last();
        if (first.is("elif") && !previous.is_some_and(|s| s.is("if") || s.is("elif")))
            || (first.is("else")
                && !previous
                    .is_some_and(|s| ["if", "elif", "for", "while"].iter().any(|word| s.is(word))))
        {
            return Err(syntax(line.number, "elif 或 else 缺少对应的前置语句"));
        }
        let mut stmt = block(line, lines, scope, depth)?;
        if stmt.is("try") {
            while let Some(handler) =
                lines.next_if(|next| next.indent == indent && next.tokens[0].is("except"))
            {
                if stmt
                    .clauses
                    .last()
                    .is_some_and(|clause| clause.head.len() == 1)
                {
                    return Err(syntax(handler.number, "裸 except 必须位于所有处理块之后"));
                }
                stmt.clauses.push(block(handler, lines, scope, depth)?);
            }
            if stmt.clauses.is_empty() {
                return Err(syntax(line.number, "try 缺少 except 处理块"));
            }
            for keyword in ["else", "finally"] {
                if let Some(clause) =
                    lines.next_if(|next| next.indent == indent && next.tokens[0].is(keyword))
                {
                    stmt.clauses.push(block(clause, lines, scope, depth)?);
                }
            }
        }
        statements.push(stmt);
    }
    if let Some(line) = decorator {
        return Err(syntax(line, "装饰器后缺少函数或类"));
    }
    Ok(statements)
}

fn block<'a>(
    line: &'a Line<'a>,
    lines: &mut Lines<'a>,
    scope: Scope,
    depth: usize,
) -> Result<Statement<'a>, SyntaxError> {
    let Some((head, inline)) = split(&line.tokens, ":") else {
        return Err(syntax(line.number, "语句块标题缺少冒号"));
    };
    let inner = compound(head, scope).ok_or_else(|| syntax(line.number, "语句块标题格式无效"))?;
    let body = if !inline.is_empty() {
        simple_line(inline, inner)?
    } else {
        let next_indent = lines
            .peek()
            .map(|next| next.indent)
            .filter(|next| *next > line.indent)
            .ok_or_else(|| syntax(line.number, "语句块缺少缩进的语句"))?;
        suite(lines, next_indent, inner, depth + 1)?
    };
    Ok(Statement {
        head,
        body,
        clauses: vec![],
    })
}

fn compound(tokens: &[Token<'_>], scope: Scope) -> Option<Scope> {
    let [first, header @ ..] = tokens else {
        return None;
    };
    if first.is("def") {
        let [name, args, tail @ ..] = header else {
            return None;
        };
        let tail_ok = tail.is_empty() || (tail[0].is("->") && expression(&tail[1..]));
        return (name.name().is_some() && args.group('(').is_some_and(parameters) && tail_ok)
            .then_some(Scope {
                function: true,
                in_loop: false,
            });
    }
    if first.is("class") {
        let [name, tail @ ..] = header else {
            return None;
        };
        let bases_ok =
            tail.is_empty() || matches!(tail, [bases] if bases.group('(').is_some_and(call));
        return (name.name().is_some() && bases_ok).then_some(Scope::default());
    }
    if first.is("for") {
        let (lhs, rhs) = split(header, "in")?;
        return (target(lhs) && expression_list(rhs)).then_some(Scope {
            in_loop: true,
            ..scope
        });
    }
    if first.is("while") {
        return expression(header).then_some(Scope {
            in_loop: true,
            ..scope
        });
    }
    if first.is("else") || first.is("try") || first.is("finally") {
        return header.is_empty().then_some(scope);
    }
    if first.is("except") {
        if header.is_empty() {
            // 裸 except 由规则 8 报告，语法错误仍由规则 1 报告。
            return Some(scope);
        }
        let exception = if let Some((name, alias)) = split(header, "as") {
            let [alias] = alias else { return None };
            alias.name()?;
            name
        } else {
            header
        };
        return dotted(exception, false).map(|_| scope);
    }
    expression(header).then_some(scope)
}

fn parameters(tokens: &[Token<'_>]) -> bool {
    if tokens.is_empty() {
        return true;
    }
    let mut names = HashSet::new();
    let mut keyword_only = false;
    let mut has_default = false;
    let mut bare_star = false;
    for arg in comma_parts(tokens) {
        let Some((first, _)) = arg.split_first() else {
            return false;
        };
        if first.is("*") && arg.len() == 1 && !keyword_only {
            keyword_only = true;
            bare_star = true;
            continue;
        }
        let (decl, value) =
            split(arg, "=").map_or((arg, None), |(decl, value)| (decl, Some(value)));
        let (name, annotation) =
            split(decl, ":").map_or((decl, None), |(name, ty)| (name, Some(ty)));
        let [name] = name else { return false };
        if !name.name().is_some_and(|name| names.insert(name))
            || annotation.is_some_and(|ty| !expression(ty))
            || value.is_some_and(|value| !expression(value))
            || (!keyword_only && has_default && value.is_none())
        {
            return false;
        }
        bare_star = false;
        has_default |= value.is_some();
    }
    !bare_star
}

fn simple_line<'a>(
    tokens: &'a [Token<'a>],
    scope: Scope,
) -> Result<Vec<Statement<'a>>, SyntaxError> {
    let stop = tokens.len() - usize::from(tokens.last().is_some_and(|token| token.is(";")));
    tokens[..stop]
        .split(|token| token.is(";"))
        .map(|head| {
            if !simple(head, scope) {
                return Err(syntax(
                    head.first().unwrap_or(&tokens[0]).line,
                    "Python 语句或表达式语法无效，或超出生成脚本支持的子集",
                ));
            }
            Ok(Statement {
                head,
                body: vec![],
                clauses: vec![],
            })
        })
        .collect()
}

fn simple(tokens: &[Token<'_>], scope: Scope) -> bool {
    let [first, tail @ ..] = tokens else {
        return false;
    };
    match first.kind {
        Kind::Word("import" | "from") => imports(tokens).is_some(),
        Kind::Word("assert") => split(tail, ",").map_or_else(
            || expression(tail),
            |(test, message)| expression(test) && expression(message),
        ),
        Kind::Word("return") => scope.function && (tail.is_empty() || expression_list(tail)),
        Kind::Word("raise") => {
            tail.is_empty()
                || split(tail, "from").map_or_else(
                    || expression(tail),
                    |(error, cause)| expression(error) && expression(cause),
                )
        }
        Kind::Word("break" | "continue") => scope.in_loop && tail.is_empty(),
        Kind::Word("pass") => tail.is_empty(),
        _ => {
            if let Some((lhs, op, rhs)) = assignment(tokens) {
                let mut rest = rhs;
                if !assignment_target(lhs, op) {
                    return false;
                }
                while let Some((lhs, next, rhs)) = assignment(rest) {
                    if op != "=" || next != "=" || !target(lhs) {
                        return false;
                    }
                    rest = rhs;
                }
                expression_list(rest)
            } else if let Some((lhs, annotation)) = split(tokens, ":") {
                assignment_target(lhs, "+=") && expression(annotation)
            } else {
                expression_list(tokens)
            }
        }
    }
}

pub(super) fn assignment<'a>(
    tokens: &'a [Token<'a>],
) -> Option<(&'a [Token<'a>], &'a str, &'a [Token<'a>])> {
    let (index, op) = tokens
        .iter()
        .enumerate()
        .find_map(|(index, token)| match token.kind {
            Kind::Op(op)
                if op == "=" || (op.ends_with('=') && !["==", "<=", ">=", "!="].contains(&op)) =>
            {
                Some((index, op))
            }
            _ => None,
        })?;
    Some((&tokens[..index], op, &tokens[index + 1..]))
}

fn assignment_target(tokens: &[Token<'_>], op: &str) -> bool {
    if let Some((base, annotation)) = split(tokens, ":") {
        return op == "=" && base.len() == 1 && target(base) && expression(annotation);
    }
    target(tokens)
        && (op == "="
            || !tokens
                .iter()
                .any(|t| t.is(",") || t.group('(').is_some() || t.group('[').is_some()))
}

pub(super) struct Import<'a> {
    pub module: String,
    pub symbol: Option<&'a str>,
    pub alias: Option<&'a str>,
}

pub(super) fn imports<'a>(tokens: &'a [Token<'a>]) -> Option<Vec<Import<'a>>> {
    let (module, names) = if tokens.first()?.is("import") {
        (None, &tokens[1..])
    } else if tokens[0].is("from") {
        let (path, names) = split(&tokens[1..], "import")?;
        (Some(dotted(path, true)?), names)
    } else {
        return None;
    };
    if module.is_some() && matches!(names, [star] if star.is("*")) {
        return Some(vec![Import {
            module: module?,
            symbol: Some("*"),
            alias: None,
        }]);
    }
    let list = if module.is_some()
        && let [group] = names
        && let Some(items) = group.group('(')
    {
        items
    } else {
        if names.last().is_some_and(|t| t.is(",")) {
            return None;
        }
        names
    };
    comma_parts(list)
        .map(|part| {
            let (name, alias) = if let Some((name, tail)) = split(part, "as") {
                let [alias] = tail else { return None };
                (name, Some(alias.name()?))
            } else {
                (part, None)
            };
            if let Some(path) = &module {
                let [symbol] = name else { return None };
                Some(Import {
                    module: path.clone(),
                    symbol: Some(symbol.name()?),
                    alias,
                })
            } else {
                Some(Import {
                    module: dotted(name, false)?,
                    symbol: None,
                    alias,
                })
            }
        })
        .collect()
}

fn dotted(tokens: &[Token<'_>], relative: bool) -> Option<String> {
    let prefix = if relative {
        tokens
            .iter()
            .take_while(|t| t.is(".") || t.is("..."))
            .count()
    } else {
        0
    };
    let mut name = String::new();
    for token in &tokens[..prefix] {
        name.push_str(if token.is("...") { "..." } else { "." });
    }
    if tokens.len() == prefix {
        return (!name.is_empty()).then_some(name);
    }
    for (index, token) in tokens[prefix..].iter().enumerate() {
        if index % 2 == 0 {
            name.push_str(token.name()?);
        } else if token.is(".") {
            name.push('.');
        } else {
            return None;
        }
    }
    (!name.ends_with('.')).then_some(name)
}

use super::python::{Kind, Statement, Token, comma_parts, split};

pub(super) fn check_try(stmt: &Statement<'_>, mut report: impl FnMut(usize, &str)) {
    let body_has_assertion = stmt.body.iter().any(contains_assertion);
    // else 里的断言只有 try 体不抛异常才会执行：except 一旦吞掉异常，else 的断言就被跳过，测试在没断言的情况下通过
    let else_has_assertion = stmt.clauses.iter().any(|clause| clause.is("else") && clause.body.iter().any(contains_assertion));
    let mut assertion_before_finally = body_has_assertion;
    for clause in &stmt.clauses {
        if clause.is("except") {
            let header = &clause.head[1..];
            let exception = split(header, "as").map_or(header, |(name, _)| name);
            if exception.is_empty()
                || exception
                    .last()
                    .is_some_and(|name| name.is("BaseException"))
            {
                report(clause.line(), "禁止裸 except 和 except BaseException");
            } else if (body_has_assertion || else_has_assertion) && !propagates_failure(&clause.body) {
                report(
                    clause.line(),
                    "except 可能吞掉断言（含 else 里的断言会被跳过），处理路径必须执行 raise 或 pytest.fail(...)",
                );
            }
        }
        if clause.is("finally") {
            if assertion_before_finally && escapes(clause, false) {
                report(clause.line(), "finally 不得使用控制流跳转吞掉断言");
            }
        } else {
            assertion_before_finally |= clause.body.iter().any(contains_assertion);
        }
    }
}

/// 恒真的断言和恒假的分支：写了 assert 却永远不会失败、或者永远不会执行。
/// 只认字面量（`True`、非零数字、非空字符串、`x or True`；`False`、`None`、`0`、空字符串），不做常量折叠，是启发式。
pub(super) fn check_constant(stmt: &Statement<'_>, mut report: impl FnMut(usize, &str)) {
    let head = &stmt.head[1..];
    if stmt.is("assert") {
        let condition = comma_parts(head).next().map_or(&[][..], strip_parens);
        let always_true = matches!(condition, [only] if truthy_literal(only))
            || condition.windows(2).any(|pair| pair[0].is("or") && truthy_literal(&pair[1]));
        if always_true {
            report(stmt.line(), "断言的条件恒为真，永远不会失败，不能当作对预期的断言");
        }
    } else if (stmt.is("if") || stmt.is("elif") || stmt.is("while")) && stmt.body.iter().any(contains_assertion) {
        if matches!(strip_parens(head), [only] if falsy_literal(only)) {
            report(stmt.line(), "条件恒为假，里面的断言永远不会执行");
        }
    }
}

fn truthy_literal(token: &Token<'_>) -> bool {
    match &token.kind {
        Kind::Word("True") => true,
        Kind::Number(text) => !is_zero(text),
        Kind::String(literal) => !literal.text.is_empty(),
        _ => false,
    }
}

fn falsy_literal(token: &Token<'_>) -> bool {
    match &token.kind {
        Kind::Word("False" | "None") => true,
        Kind::Number(text) => is_zero(text),
        Kind::String(literal) => literal.text.is_empty() && !literal.formatted,
        _ => false,
    }
}

fn is_zero(text: &str) -> bool {
    text.chars().all(|c| matches!(c, '0' | '.' | '_'))
}

pub(super) fn count(statements: &[Statement<'_>]) -> usize {
    statements
        .iter()
        .map(|stmt| {
            if stmt.is("assert") {
                1
            } else if stmt.is("def") || stmt.is("class") {
                0
            } else if stmt.is("try") {
                count(&stmt.clauses)
            } else {
                count(&stmt.body)
            }
        })
        .sum()
}

fn contains_assertion(stmt: &Statement<'_>) -> bool {
    let head = if stmt.is("def") || stmt.is("class") {
        // 定义时求值的默认参数和基类也可能调用断言函数。
        &stmt.head[2..]
    } else {
        stmt.head
    };
    stmt.is("assert")
        || has_call(
            head,
            |tokens| matches!(tokens, [callee, args, ..] if args.group('(').is_some() && is_assertion_function(callee)),
        )
        || (!stmt.is("def") && stmt.children().any(contains_assertion))
}

fn is_assertion_function(callee: &Token<'_>) -> bool {
    let name = match strip_parens(std::slice::from_ref(callee)) {
        [name] => name.name(),
        [.., dot, method] if dot.is(".") => method.name(),
        _ => None,
    };
    name.is_some_and(|name| name.starts_with("assert_"))
}

fn propagates_failure(body: &[Statement<'_>]) -> bool {
    let mut statements = body.iter().peekable();
    while let Some(stmt) = statements.next() {
        if escapes(stmt, false) {
            return false;
        }
        let head = strip_parens(stmt.head);
        if stmt.is("raise") || matches!(pytest_call(head), Some(("fail", len)) if len == head.len())
        {
            return true;
        }
        if stmt.is("if") {
            let mut all_branches_fail = propagates_failure(&stmt.body);
            while let Some(branch) = statements.next_if(|s| s.is("elif") || s.is("else")) {
                if escapes(branch, false) {
                    return false;
                }
                all_branches_fail &= propagates_failure(&branch.body);
                if branch.is("else") && all_branches_fail {
                    return true;
                }
            }
        }
    }
    false
}

fn escapes(stmt: &Statement<'_>, in_local_loop: bool) -> bool {
    stmt.is("return")
        || (!in_local_loop && (stmt.is("break") || stmt.is("continue")))
        || has_call(stmt.head, |tokens| {
            matches!(pytest_call(tokens), Some(("skip" | "xfail" | "exit", _)))
        })
        || (!stmt.is("def")
            && stmt
                .children()
                .any(|child| escapes(child, in_local_loop || stmt.is("for") || stmt.is("while"))))
}

fn pytest_call<'a>(tokens: &[Token<'a>]) -> Option<(&'a str, usize)> {
    match tokens {
        [callee, args, ..] if args.group('(').is_some() => {
            Some((pytest_method(callee.group('(')?)?, 2))
        }
        [_, _, _, args, ..] if args.group('(').is_some() => Some((pytest_method(&tokens[..3])?, 4)),
        _ => None,
    }
}

fn pytest_method<'a>(callee: &[Token<'a>]) -> Option<&'a str> {
    match strip_parens(callee) {
        [module, dot, method]
            if dot.is(".")
                && matches!(strip_parens(std::slice::from_ref(module)), [name] if name.is("pytest")) =>
        {
            method.name()
        }
        _ => None,
    }
}

fn strip_parens<'t, 's>(mut tokens: &'t [Token<'s>]) -> &'t [Token<'s>] {
    while let [group] = tokens
        && let Some(inner) = group.group('(')
    {
        tokens = inner;
    }
    tokens
}

fn has_call(tokens: &[Token<'_>], matches_call: impl Fn(&[Token<'_>]) -> bool + Copy) -> bool {
    tokens.iter().enumerate().any(|(index, token)| {
        matches_call(&tokens[index..])
            || match &token.kind {
                Kind::Group(_, items) => has_call(items, matches_call),
                Kind::String(literal) => literal
                    .fields
                    .iter()
                    .any(|field| has_call(field, matches_call)),
                _ => false,
            }
    })
}

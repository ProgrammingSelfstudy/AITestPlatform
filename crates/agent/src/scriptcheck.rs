//! 生成脚本落盘前的静态闸门。只处理内存中的源码。

mod assertions;
mod contract;
mod iteration;
mod python;
mod sensitive;

use std::collections::{BTreeMap, HashMap, HashSet};

use iteration::{IteratorCalls, finite_comprehension_source};
use python::{Arg, Kind, Statement, Token, argument, assignment, comma_parts, split};
use serde::Serialize;

pub use contract::{ParameterKind, UiContract, UiMethod, UiParameter};
pub(crate) use sensitive::redact as redact_sensitive;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Violation {
    pub file: String,
    pub line: usize,
    pub rule: u8,
    pub message: String,
}

impl Violation {
    pub(crate) fn new(file: &str, line: usize, rule: u8, message: &str) -> Self {
        Self {
            file: file.into(),
            line,
            rule,
            message: message.into(),
        }
    }
}

pub(crate) fn case_key(code: &str) -> Option<String> {
    let key = code.replace('-', "").to_ascii_lowercase();
    (!key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))
    .then_some(key)
}

/// 检查 Python 源码及当前用例的 pytest 标记和断言数量。行号从 1 开始。
pub fn check(
    files: &BTreeMap<String, String>,
    contract: &UiContract,
    case_code: &str,
    expected_count: usize,
) -> Vec<Violation> {
    check_with_redactions(files, contract, case_code, expected_count, &[])
}

pub(crate) fn check_with_redactions(
    files: &BTreeMap<String, String>,
    contract: &UiContract,
    case_code: &str,
    expected_count: usize,
    redactions: &[(String, String)],
) -> Vec<Violation> {
    let Some(key) = case_key(case_code) else {
        return vec![Violation::new("输出", 1, 0, "用例编号不能转换成脚本文件名")];
    };
    let test_name = format!("test_{key}.py");
    let page_name = format!("pages/{key}_pages.py");
    let mut page_snapshots = HashSet::new();
    let mut violations = Vec::new();
    if !files.contains_key(&test_name) {
        violations.push(Violation::new(
            &test_name,
            1,
            6,
            "缺少对应的 pytest 测试文件",
        ));
    }
    // 文件按名称有序，配套 Page Object 会先于测试文件解析。
    for (file, source) in files {
        // Python 会先统一换行符，再解释缩进和字符串内的行续接。
        let source = source.replace("\r\n", "\n").replace('\r', "\n");
        let parsed = python::lex(&source);
        let suite = match parsed
            .as_ref()
            .map_err(|err| (err.line, err.message))
            .and_then(|lines| python::parse(lines).map_err(|err| (err.line, err.message)))
        {
            Ok(suite) => suite,
            Err((line, message)) => {
                violations.push(Violation::new(file, line, 1, message));
                continue;
            }
        };
        if file == &page_name {
            page_snapshots.extend(
                suite
                    .iter()
                    .filter(|stmt| stmt.is("class"))
                    .flat_map(|stmt| snapshot_methods(&stmt.body))
                    .map(str::to_owned),
            );
        }
        let mut check = FileCheck {
            file,
            contract,
            key: &key,
            redactions,
            page_snapshots: &page_snapshots,
            iterator_calls: IteratorCalls::new(&suite),
            regex_modules: suite
                .iter()
                .flat_map(|stmt| python::imports(stmt.head).into_iter().flatten())
                .filter(|entry| entry.module == "re" && entry.symbol.is_none())
                .map(|entry| entry.alias.unwrap_or("re"))
                .collect(),
            violations: &mut violations,
        };
        for (line, text) in source.lines().enumerate() {
            if contains_known(text, redactions) {
                check.report(line + 1, 5, "源码含已登记的敏感值");
            }
        }
        check.suite(&suite, &HashMap::new(), &HashSet::new());
        if file == &test_name {
            check.tests(&suite, case_code, expected_count.max(1));
        }
    }
    violations.sort();
    violations.dedup();
    violations
}

pub(crate) fn contains_sensitive(text: &str, redactions: &[(String, String)]) -> bool {
    sensitive::contains(text) || contains_known(text, redactions)
}

fn contains_known(text: &str, redactions: &[(String, String)]) -> bool {
    redactions
        .iter()
        .any(|(value, _)| !value.is_empty() && text.contains(value))
}

struct FileCheck<'a, 'v> {
    file: &'a str,
    contract: &'a UiContract,
    key: &'a str,
    redactions: &'a [(String, String)],
    page_snapshots: &'a HashSet<String>,
    iterator_calls: IteratorCalls<'a>,
    regex_modules: HashSet<&'a str>,
    violations: &'v mut Vec<Violation>,
}

// 只支持能在源码里直接读出长度的展开。None 表示名称重复赋值或值不是定长元组。
type FixedTuples<'a> = HashMap<&'a str, Option<usize>>;

struct Scope<'a, 's> {
    tuples: &'s FixedTuples<'a>,
    class_tuples: &'s FixedTuples<'a>,
}

impl FileCheck<'_, '_> {
    fn report(&mut self, line: usize, rule: u8, message: &str) {
        self.violations
            .push(Violation::new(self.file, line, rule, message));
    }

    fn suite<'a>(
        &mut self,
        statements: &[Statement<'a>],
        class_tuples: &FixedTuples<'a>,
        class_snapshots: &HashSet<&'a str>,
    ) {
        let tuples = tuple_bindings(statements);
        let mut prior = None;
        for stmt in statements {
            let import = stmt.is("import") || stmt.is("from");
            if import {
                for entry in python::imports(stmt.head).expect("导入语句已解析") {
                    if entry.symbol == Some("*")
                        || !self.contract.allows_import(&entry.module, self.key)
                    {
                        self.report(stmt.line(), 2, "导入的模块不在白名单内，或使用了星号导入");
                    }
                    if entry.symbol == Some("sleep")
                        && entry.alias.is_some_and(|alias| alias != "sleep")
                    {
                        self.report(stmt.line(), 9, "sleep 必须直接调用并使用不超过 3 秒的常量");
                    }
                }
            }
            if stmt.is("for") {
                let (_, iterable) = split(&stmt.head[1..], "in").expect("for 已解析");
                if !self.snapshot_source(iterable, class_snapshots) {
                    self.report(
                        stmt.line(),
                        7,
                        "for 缺少可识别的有限迭代源，请用 range 或有限字面量",
                    );
                }
            } else if stmt.is("while") && !bounded_while(stmt, prior) {
                self.report(
                    stmt.line(),
                    7,
                    "while 缺少可验证的计数上限，while True 还需要达到上限时的 break",
                );
            } else if stmt.is("try") {
                assertions::check_try(stmt, |line, message| self.report(line, 8, message));
            }
            let scope = Scope {
                tuples: &tuples,
                class_tuples,
            };
            self.tokens(stmt.head, &scope, import);
            if stmt.is("class") {
                self.suite(
                    &stmt.body,
                    &tuple_bindings(&stmt.body),
                    &snapshot_methods(&stmt.body),
                );
            } else if !stmt.body.is_empty() {
                self.suite(&stmt.body, class_tuples, class_snapshots);
            }
            if !stmt.clauses.is_empty() {
                self.suite(&stmt.clauses, class_tuples, class_snapshots);
            }
            prior = Some(stmt);
        }
    }

    fn snapshot_source(&self, tokens: &[Token<'_>], class_snapshots: &HashSet<&str>) -> bool {
        bounded_iterable(tokens, class_snapshots)
            || matches!(tokens, [owner, dot, method, args] if !owner.is("self")
                && owner.name().is_some() && dot.is(".") && args.group('(').is_some()
                && method.name().is_some_and(|name| self.page_snapshots.contains(name)))
    }

    fn tokens(&mut self, tokens: &[Token<'_>], scope: &Scope<'_, '_>, import: bool) {
        for (index, token) in tokens.iter().enumerate() {
            let next = &tokens[index + 1..];
            let prev = index.checked_sub(1).map(|n| &tokens[n]);
            match &token.kind {
                Kind::Word(word) => {
                    let regex_compile = *word == "compile"
                        && matches!(&tokens[..index], [.., module, dot]
                            if dot.is(".") && module.name().is_some_and(|name| self.regex_modules.contains(name)))
                        && (index < 3 || !tokens[index - 3].is("."))
                        && next.first().and_then(|token| token.group('(')).is_some();
                    if !regex_compile && forbidden(word, prev.is_some_and(|t| t.is("def"))) {
                        self.report(
                            token.line,
                            3,
                            "使用了禁止的执行、文件、进程、网络或反射操作",
                        );
                    }
                    if token.is("ui")
                        && let [dot, method, tail @ ..] = next
                        && dot.is(".")
                    {
                        let allowed_receiver = match &tokens[..index] {
                            [.., owner, dot] if dot.is(".") => owner.is("self"),
                            _ => true,
                        };
                        if allowed_receiver {
                            self.ui_call(
                                token.line,
                                method,
                                tail,
                                scope.tuples,
                                scope.class_tuples,
                            );
                        } else {
                            self.report(token.line, 4, "Ui 调用应使用 ui 或 self.ui");
                        }
                    }
                    // 防止重绑定 range 使有界循环检查失效。
                    if token.is("range")
                        && (prev.is_some_and(|t| t.is(".") || t.is("def") || t.is("class"))
                            || next.first().and_then(|t| t.group('(')).is_none())
                    {
                        self.report(token.line, 7, "range 只允许作为内置函数直接调用");
                    }
                    if !import && !self.iterator_calls.allows(word, prev, next) {
                        self.report(
                            token.line,
                            7,
                            "禁止无上限的迭代器，iter 只允许直接调用并传入一个参数",
                        );
                    }
                    if token.is("sleep")
                        && !import
                        && !valid_wait(next.first().and_then(|t| t.group('(')), None)
                    {
                        self.report(token.line, 9, "sleep 必须直接调用并使用不超过 3 秒的常量");
                    }
                }
                Kind::Group(_, inner) => {
                    if !import
                        && let Some((_, Ok(sources))) = python::comprehension(inner)
                        && !sources.into_iter().all(finite_comprehension_source)
                    {
                        self.report(token.line, 7, "推导式缺少可识别的有限迭代源");
                    }
                    self.tokens(inner, scope, import);
                }
                Kind::String(literal) => {
                    let starts_run = prev.is_none_or(|t| !matches!(t.kind, Kind::String(_)))
                        && !(prev.is_some_and(|t| t.is("+"))
                            && index >= 2
                            && matches!(tokens[index - 2].kind, Kind::String(_)));
                    if starts_run {
                        self.strings(token.line, &tokens[index..]);
                    }
                    for field in &literal.fields {
                        self.tokens(field, scope, import);
                    }
                }
                _ => {}
            }
        }
    }

    fn strings(&mut self, line: usize, mut tokens: &[Token<'_>]) {
        let mut joined = String::new();
        while let Some(Token {
            kind: Kind::String(literal),
            ..
        }) = tokens.first()
        {
            joined.push_str(&literal.text);
            tokens = &tokens[1..];
            if tokens.first().is_some_and(|t| t.is("+")) {
                tokens = &tokens[1..];
            }
        }
        if contains_sensitive(&joined, self.redactions) {
            self.report(
                line,
                5,
                "字符串含凭证、邮箱、绝对路径、adb 或设备序列号形态",
            );
        }
    }

    fn ui_call(
        &mut self,
        line: usize,
        method: &Token<'_>,
        tail: &[Token<'_>],
        tuples: &FixedTuples<'_>,
        class_tuples: &FixedTuples<'_>,
    ) {
        let Some(signature) = method
            .name()
            .and_then(|name| self.contract.methods.get(name))
        else {
            self.report(line, 4, "Ui 方法不在契约中");
            return;
        };
        let Some(args) = tail.first().and_then(|t| t.group('(')) else {
            self.report(line, 4, "Ui 方法必须直接调用");
            return;
        };
        let mut positional = 0;
        let mut keywords = Vec::new();
        if !args.is_empty() {
            for arg in comma_parts(args).map(argument) {
                match arg {
                    Arg::Positional(_) => positional += 1,
                    Arg::Keyword(name, _) => keywords.push(name),
                    Arg::Star(value) => {
                        let len = fixed_tuple(value).or_else(|| match value {
                            [name] => tuples.get(name.name()?).copied().flatten(),
                            [owner, dot, name] if owner.is("self") && dot.is(".") => {
                                class_tuples.get(name.name()?).copied().flatten()
                            }
                            _ => None,
                        });
                        let Some(len) = len else {
                            self.report(line, 4, "Ui 调用只接受可确定参数个数的元组展开");
                            return;
                        };
                        positional += len;
                    }
                    Arg::DoubleStar(_) => {
                        self.report(line, 4, "Ui 调用不接受动态关键字参数展开");
                        return;
                    }
                }
            }
        }
        if !signature.accepts(positional, &keywords) {
            self.report(line, 4, "Ui 调用的位置参数个数或关键字参数与契约不符");
        }
        self.literal_coordinates(line, method, args);
        if method.is("wait") && !valid_wait(Some(args), Some("seconds")) {
            self.report(
                line,
                9,
                "ui.wait 必须使用不超过 3 秒的常量，请优先使用 wait_text",
            );
        }
    }

    /// 规则 10：坐标不能是写死的数字常量。
    /// 屏幕尺寸、系统字体和布局都会变，字面坐标只在录制那台机器上成立；坐标必须由页面上找到的元素推出
    /// （例如 `ui.find(...).center` 再加偏移），名字参与计算才算有依据。
    /// 已知缺口：先把字面元组绑给变量再 `*变量` 展开，这里看不出来。
    fn literal_coordinates(&mut self, line: usize, method: &Token<'_>, args: &[Token<'_>]) {
        let coordinate_arg = |index: usize, arg: &Arg<'_>| match (method.name(), arg) {
            (Some("tap_xy"), Arg::Positional(_)) => index < 2,
            (Some("tap_xy"), Arg::Keyword(name, _)) => matches!(*name, "x" | "y"),
            (Some("tap"), Arg::Keyword(name, _)) => *name == "xy",
            _ => false,
        };
        if args.is_empty() {
            return;
        }
        let literal = comma_parts(args).map(argument).enumerate().any(|(index, arg)| {
            let value = match &arg {
                Arg::Positional(value) | Arg::Keyword(_, value) => value,
                _ => return false,
            };
            coordinate_arg(index, &arg) && python::literal_only(value)
        });
        if literal {
            self.report(
                line,
                10,
                "坐标不能写死成数字常量，请由 ui.find 等找到的元素位置推出",
            );
        }
    }

    fn tests(&mut self, statements: &[Statement<'_>], case: &str, expected_count: usize) {
        let mut markers = Vec::new();
        let mut found = false;
        for stmt in statements {
            if stmt.is("@") {
                if let Some(args) = case_marker(stmt.head) {
                    markers.push(args);
                }
                continue;
            }
            if stmt.is("def")
                && stmt.head[1]
                    .name()
                    .is_some_and(|name| name.starts_with("test_"))
            {
                found = true;
                if !matches!(markers.as_slice(), [args] if marker_matches(args, case)) {
                    self.report(
                        stmt.line(),
                        6,
                        "测试函数缺少唯一且匹配用例编号的 @pytest.mark.case(...)",
                    );
                }
                if assertions::count(&stmt.body) < expected_count {
                    self.report(
                        stmt.line(),
                        8,
                        &format!("测试函数中显式 assert 少于 {expected_count} 条"),
                    );
                }
            }
            markers.clear();
        }
        if !found {
            self.report(1, 6, "缺少带 @pytest.mark.case(...) 的顶层测试函数");
            self.report(1, 8, "缺少含显式 assert 的测试函数");
        }
    }
}

fn forbidden(word: &str, definition: bool) -> bool {
    matches!(
        word,
        "exec"
            | "eval"
            | "compile"
            | "open"
            | "__import__"
            | "subprocess"
            | "os"
            | "sys"
            | "socket"
            | "requests"
            | "urllib"
            | "shutil"
            | "pathlib"
            | "importlib"
            | "builtins"
            | "ctypes"
            | "pickle"
            | "marshal"
            | "io"
            | "tempfile"
            | "httpx"
            | "multiprocessing"
            | "getattr"
            | "setattr"
            | "delattr"
            | "globals"
            | "locals"
            | "vars"
            | "input"
            | "breakpoint"
            | "adb"
    ) || (word.starts_with("__")
        && word.ends_with("__")
        && word != "__future__"
        && !(word == "__init__" && definition))
}

fn tuple_bindings<'a>(suite: &[Statement<'a>]) -> FixedTuples<'a> {
    let mut bindings = HashMap::new();
    for stmt in suite {
        if let Some(([name], op, rhs)) = assignment(stmt.head)
            && let Some(name) = name.name()
        {
            bindings
                .entry(name)
                .and_modify(|value| *value = None)
                .or_insert_with(|| if op == "=" { fixed_tuple(rhs) } else { None });
        }
    }
    bindings
}

fn fixed_tuple(tokens: &[Token<'_>]) -> Option<usize> {
    let items = match tokens {
        [group] => group.group('(')?,
        _ => tokens,
    };
    if items.is_empty() {
        return Some(0);
    }
    (items.iter().any(|t| t.is(","))
        && python::comprehension(items).is_none()
        && comma_parts(items).all(|part| matches!(argument(part), Arg::Positional(_))))
    .then(|| comma_parts(items).count())
}

fn bounded_iterable(tokens: &[Token<'_>], class_snapshots: &HashSet<&str>) -> bool {
    match tokens {
        [function, args] if function.is("range") => args.group('(').is_some_and(|args| {
            (1..=3).contains(&comma_parts(args).count())
                && comma_parts(args)
                    .all(|part| !part.is_empty() && matches!(argument(part), Arg::Positional(_)))
        }),
        [owner, dot, method, args]
            if owner.is("self")
                && dot.is(".")
                && method
                    .name()
                    .is_some_and(|name| class_snapshots.contains(name)) =>
        {
            args.group('(').is_some()
        }
        [ui, dot, method, args] | [_, _, ui, dot, method, args]
            if ui.is("ui") && dot.is(".") && (method.is("labels") || method.is("elements")) =>
        {
            args.group('(').is_some()
                && (tokens.len() == 4 || (tokens[0].is("self") && tokens[1].is(".")))
        }
        [only] => match &only.kind {
            Kind::String(_) | Kind::Group('[' | '{', _) => true,
            Kind::Group('(', items) => {
                fixed_tuple(tokens).is_some() || bounded_iterable(items, class_snapshots)
            }
            _ => false,
        },
        _ => fixed_tuple(tokens).is_some(),
    }
}

fn snapshot_methods<'a>(class: &[Statement<'a>]) -> HashSet<&'a str> {
    fn returns<'a>(stmt: &Statement<'a>, out: &mut Vec<&'a [Token<'a>]>) {
        if stmt.is("return") {
            out.push(&stmt.head[1..]);
        } else if !stmt.is("def") && !stmt.is("class") {
            for child in stmt.children() {
                returns(child, out);
            }
        }
    }
    let no_methods = HashSet::new();
    class
        .iter()
        .filter(|stmt| stmt.is("def"))
        .filter_map(|stmt| {
            let mut expressions = Vec::new();
            for child in stmt.children() {
                returns(child, &mut expressions);
            }
            (!expressions.is_empty()
                && expressions
                    .iter()
                    .all(|expr| bounded_iterable(expr, &no_methods)))
            .then(|| stmt.head[1].name())
            .flatten()
        })
        .collect()
}

fn integer(tokens: &[Token<'_>]) -> Option<i64> {
    let [number] = tokens else { return None };
    let Kind::Number(digits) = number.kind else {
        return None;
    };
    digits.replace('_', "").parse().ok()
}

fn bounded_while(stmt: &Statement<'_>, prior: Option<&Statement<'_>>) -> bool {
    let condition = &stmt.head[1..];
    if matches!(condition, [value] if value.is("False")) {
        return true;
    }
    let Some(([counter], "=", initial)) = prior.and_then(|stmt| assignment(stmt.head)) else {
        return false;
    };
    let Some(counter) = counter.name() else {
        return false;
    };
    if integer(initial).is_none() {
        return false;
    }
    let Some(update) = stmt.body.iter().position(|stmt| {
        assignment(stmt.head).is_some_and(|(lhs, op, rhs)| {
            matches!(lhs, [name] if name.is(counter))
                && match (op, rhs) {
                    ("+=", _) => integer(rhs).is_some_and(|n| n > 0),
                    ("=", [name, plus, step]) if name.is(counter) && plus.is("+") => {
                        integer(std::slice::from_ref(step)).is_some_and(|n| n > 0)
                    }
                    _ => false,
                }
        })
    }) else {
        return false;
    };
    let mut checkpoint = update;
    if matches!(condition, [value] if value.is("True")) {
        let Some(guard) = stmt.body.iter().position(|stmt| {
            stmt.is("if")
                && compare_counter(&stmt.head[1..], counter, &[">=", ">"])
                && matches!(stmt.body.as_slice(), [stop] if stop.is("break"))
        }) else {
            return false;
        };
        checkpoint = checkpoint.max(guard);
    } else if !compare_counter(condition, counter, &["<=", "<"]) {
        return false;
    }
    !stmt.body[..checkpoint].iter().any(has_continue)
        && stmt
            .body
            .iter()
            .enumerate()
            .all(|(index, stmt)| index == update || !writes(stmt, counter))
}

fn compare_counter(condition: &[Token<'_>], counter: &str, operators: &[&str]) -> bool {
    matches!(condition, [name, op, value] if name.is(counter) && operators.iter().any(|cmp| op.is(cmp))
        && integer(std::slice::from_ref(value)).is_some())
}

fn has_continue(stmt: &Statement<'_>) -> bool {
    stmt.is("continue")
        || (!["for", "while", "def", "class"]
            .iter()
            .any(|word| stmt.is(word))
            && stmt.children().any(has_continue))
}

fn writes(stmt: &Statement<'_>, name: &str) -> bool {
    fn includes(tokens: &[Token<'_>], name: &str) -> bool {
        tokens.iter().any(|token| {
            token.is(name) || matches!(&token.kind, Kind::Group(_, items) if includes(items, name))
        })
    }
    let binds = if stmt.is("for") {
        split(stmt.head, "in").is_some_and(|(lhs, _)| includes(lhs, name))
    } else if stmt.is("def") || stmt.is("class") {
        stmt.head[1].is(name)
    } else if stmt.is("import") || stmt.is("from") {
        includes(stmt.head, name)
    } else if stmt.is("except") {
        split(stmt.head, "as").is_some_and(|(_, alias)| includes(alias, name))
    } else {
        let mut rest = stmt.head;
        let mut binds = false;
        while let Some((lhs, _, rhs)) = assignment(rest) {
            binds |= includes(lhs, name);
            rest = rhs;
        }
        binds
    };
    binds || stmt.children().any(|stmt| writes(stmt, name))
}

fn valid_wait(args: Option<&[Token<'_>]>, keyword: Option<&str>) -> bool {
    let Some(args) = args else { return false };
    let mut parts = comma_parts(args);
    let value = match parts.next().map(argument) {
        Some(Arg::Positional(value)) => value,
        Some(Arg::Keyword(name, value)) if keyword == Some(name) => value,
        _ => return false,
    };
    let [token] = value else { return false };
    let Kind::Number(value) = token.kind else {
        return false;
    };
    parts.next().is_none()
        && value
            .replace('_', "")
            .parse::<f64>()
            .is_ok_and(|value| (0.0..=3.0).contains(&value))
}

fn case_marker<'a>(tokens: &'a [Token<'a>]) -> Option<&'a [Token<'a>]> {
    let [at, pytest, dot, mark, dot2, case, args] = tokens else {
        return None;
    };
    (at.is("@")
        && pytest.is("pytest")
        && dot.is(".")
        && mark.is("mark")
        && dot2.is(".")
        && case.is("case"))
    .then(|| args.group('('))
    .flatten()
}

fn marker_matches(tokens: &[Token<'_>], case: &str) -> bool {
    let mut args = comma_parts(tokens);
    matches!(args.next(), Some([Token { kind: Kind::String(literal), .. }])
        if !literal.formatted && literal.text == case)
        && args.next().is_none()
}

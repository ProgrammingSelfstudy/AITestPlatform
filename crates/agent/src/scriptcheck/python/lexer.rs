use std::sync::LazyLock;

use regex_automata::meta::Regex;

use super::{SyntaxError, syntax};

#[derive(Debug)]
pub(in crate::scriptcheck) struct Line<'a> {
    pub number: usize,
    pub indent: usize,
    pub tokens: Vec<Token<'a>>,
}

#[derive(Debug)]
pub(in crate::scriptcheck) struct Token<'a> {
    pub line: usize,
    pub kind: Kind<'a>,
}

#[derive(Debug)]
pub(in crate::scriptcheck) enum Kind<'a> {
    Word(&'a str),
    Number(&'a str),
    String(Literal<'a>),
    Op(&'a str),
    Group(char, Vec<Token<'a>>),
}

#[derive(Debug)]
pub(in crate::scriptcheck) struct Literal<'a> {
    pub text: String,
    pub formatted: bool,
    pub fields: Vec<Vec<Token<'a>>>,
}

impl<'a> Token<'a> {
    pub fn is(&self, value: &str) -> bool {
        matches!(self.kind, Kind::Word(s) | Kind::Op(s) if s == value)
    }

    pub fn name(&self) -> Option<&'a str> {
        match self.kind {
            Kind::Word(word) if !KEYWORDS.contains(&word) => Some(word),
            _ => None,
        }
    }

    pub fn group(&self, opening: char) -> Option<&[Token<'a>]> {
        match &self.kind {
            Kind::Group(kind, tokens) if *kind == opening => Some(tokens),
            _ => None,
        }
    }
}

const KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];
const MAX_NESTING: usize = 64;

pub(in crate::scriptcheck) fn lex(source: &str) -> Result<Vec<Line<'_>>, SyntaxError> {
    if let Some(index) = source.find('\0') {
        let line = 1 + source[..index]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        return Err(syntax(line, "Python 源码不能包含 NUL 字节"));
    }
    let mut lexer = Lexer {
        rest: source.strip_prefix('\u{feff}').unwrap_or(source),
        line: 1,
    };
    let mut lines = Vec::new();
    while !lexer.rest.is_empty() {
        let number = lexer.line;
        let indent = lexer.rest.len() - lexer.rest.trim_start_matches(' ').len();
        lexer.rest = &lexer.rest[indent..];
        if lexer.rest.starts_with(['\t', '\x0c']) {
            return Err(syntax(number, "缩进必须使用空格"));
        }
        let mut tokens = Vec::new();
        loop {
            lexer.skip_space();
            if lexer.rest.is_empty() || lexer.eat("\n") {
                break;
            }
            tokens.push(lexer.token(0)?);
        }
        if !tokens.is_empty() {
            lines.push(Line {
                number,
                indent,
                tokens,
            });
        }
    }
    Ok(lines)
}

struct Lexer<'a> {
    rest: &'a str,
    line: usize,
}

impl<'a> Lexer<'a> {
    fn eat(&mut self, prefix: &str) -> bool {
        if let Some(rest) = self.rest.strip_prefix(prefix) {
            self.rest = rest;
            self.line += prefix.bytes().filter(|c| *c == b'\n').count();
            return true;
        }
        false
    }

    fn take_char(&mut self) -> Option<char> {
        let ch = self.rest.chars().next()?;
        self.rest = &self.rest[ch.len_utf8()..];
        self.line += usize::from(ch == '\n');
        Some(ch)
    }

    fn skip_space(&mut self) {
        loop {
            self.rest = self.rest.trim_start_matches([' ', '\t', '\x0c']);
            if self.rest.starts_with('#') {
                self.rest = self.rest.find('\n').map_or("", |i| &self.rest[i..]);
            }
            if !self.eat("\\\n") {
                break;
            }
        }
    }

    fn token(&mut self, depth: usize) -> Result<Token<'a>, SyntaxError> {
        let line = self.line;
        if depth >= MAX_NESTING {
            return Err(syntax(line, "括号或字符串插值嵌套过深"));
        }
        let first = self.rest.chars().next().expect("已有词法输入");
        let kind = match first {
            '\'' | '"' => Kind::String(self.string("", depth)?),
            'a'..='z' | 'A'..='Z' | '_' => {
                let len = self
                    .rest
                    .bytes()
                    .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                    .count();
                let (word, rest) = self.rest.split_at(len);
                self.rest = rest;
                if rest.starts_with(['\'', '"'])
                    && matches!(
                        word.to_ascii_lowercase().as_str(),
                        "r" | "u" | "f" | "fr" | "rf"
                    )
                {
                    Kind::String(self.string(word, depth)?)
                } else {
                    Kind::Word(word)
                }
            }
            '0'..='9' => Kind::Number(self.number()?),
            '.' if self.rest.as_bytes().get(1).is_some_and(u8::is_ascii_digit) => {
                Kind::Number(self.number()?)
            }
            '(' | '[' | '{' => {
                self.take_char();
                Kind::Group(first, self.group(first, depth + 1)?)
            }
            ')' | ']' | '}' => return Err(syntax(line, "括号不匹配")),
            _ => {
                const OPS: &[&str] = &[
                    "**=", "//=", "<<=", ">>=", "...", "==", "!=", "<=", ">=", "**", "//", "+=",
                    "-=", "*=", "/=", "%=", "&=", "|=", "^=", "@=", "<<", ">>", "->", "+", "-",
                    "*", "/", "%", "<", ">", "=", ":", ",", ".", "@", ";", "|", "&", "^", "~",
                ];
                let op = OPS
                    .iter()
                    .find(|op| self.rest.starts_with(**op))
                    .ok_or_else(|| syntax(line, "出现非法字符或生成脚本不支持的词法结构"))?;
                self.eat(op);
                Kind::Op(op)
            }
        };
        Ok(Token { line, kind })
    }

    fn number(&mut self) -> Result<&'a str, SyntaxError> {
        static NUMBER: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(concat!(
                r"^(?:0[xX](?:_?[0-9a-fA-F])+|0[bB](?:_?[01])+|0[oO](?:_?[0-7])+|",
                r"(?:[0-9](?:_?[0-9])*(?:\.(?:[0-9](?:_?[0-9])*)?)?|\.[0-9](?:_?[0-9])*)",
                r"(?:[eE][+-]?[0-9](?:_?[0-9])*)?[jJ]?)"
            ))
            .expect("数值字面量正则有效")
        });
        let m = NUMBER.find(self.rest).expect("已有数字前缀");
        let (number, rest) = self.rest.split_at(m.end());
        self.rest = rest;
        if (number.starts_with('0')
            && number.bytes().all(|b| b.is_ascii_digit() || b == b'_')
            && number.bytes().any(|b| b.is_ascii_digit() && b != b'0'))
            || rest
                .as_bytes()
                .first()
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            return Err(syntax(self.line, "数值字面量格式无效"));
        }
        Ok(number)
    }

    fn group(&mut self, opening: char, depth: usize) -> Result<Vec<Token<'a>>, SyntaxError> {
        let start = self.line;
        let closing = match opening {
            '(' => ")",
            '[' => "]",
            '{' => "}",
            _ => unreachable!(),
        };
        let mut tokens = Vec::new();
        loop {
            self.skip_space();
            if self.eat(closing) {
                return Ok(tokens);
            }
            if self.rest.is_empty() {
                return Err(syntax(start, "括号未闭合"));
            }
            if !self.eat("\n") {
                tokens.push(self.token(depth)?);
            }
        }
    }

    fn string(&mut self, prefix: &str, depth: usize) -> Result<Literal<'a>, SyntaxError> {
        let line = self.line;
        let quote = self.take_char().expect("已有引号");
        let raw = prefix.contains(['r', 'R']);
        let formatted = prefix.contains(['f', 'F']);
        let quotes = if quote == '"' { "\"\"\"" } else { "'''" };
        let triple = self.eat(&quotes[1..]);
        let end = if triple { quotes } else { &quotes[..1] };
        let mut text = String::new();
        let mut fields = Vec::new();
        loop {
            if self.eat(end) {
                return Ok(Literal {
                    text,
                    formatted,
                    fields,
                });
            }
            match self.take_char() {
                None | Some('\n') if !triple => break,
                None => break,
                Some('\\') if !raw => self.escape(&mut text, formatted)?,
                Some('\\') => {
                    text.push('\\');
                    if self.rest.starts_with([quote, '\\', '\n']) {
                        text.push(self.take_char().expect("已有转义字符"));
                    }
                }
                Some('{') if formatted && !self.eat("{") => {
                    fields.push(self.f_field(depth + 1, &mut text)?);
                    text.push('\n');
                }
                Some('}') if formatted && !self.eat("}") => {
                    return Err(syntax(self.line, "f-string 中有单独的右花括号"));
                }
                Some(ch) => text.push(ch),
            }
        }
        Err(syntax(line, "字符串未闭合"))
    }

    fn escape(&mut self, out: &mut String, formatted: bool) -> Result<(), SyntaxError> {
        if formatted && self.rest.starts_with(['{', '}']) {
            return Err(syntax(self.line, "f-string 的花括号必须成对转义"));
        }
        let value = match self.take_char() {
            Some('\n') => return Ok(()),
            Some(ch @ ('\'' | '"' | '\\')) => ch,
            Some('a') => '\x07',
            Some('b') => '\x08',
            Some('f') => '\x0c',
            Some('n') => '\n',
            Some('r') => '\r',
            Some('t') => '\t',
            Some('v') => '\x0b',
            Some(ch @ ('x' | 'u' | 'U')) => {
                let count = if ch == 'x' {
                    2
                } else if ch == 'u' {
                    4
                } else {
                    8
                };
                let hex = self
                    .rest
                    .get(..count)
                    .filter(|s| s.bytes().all(|b| b.is_ascii_hexdigit()));
                let value = hex
                    .and_then(|s| u32::from_str_radix(s, 16).ok())
                    .and_then(char::from_u32)
                    .ok_or_else(|| syntax(self.line, "无效的十六进制或 Unicode 转义"))?;
                self.rest = &self.rest[count..];
                value
            }
            Some(ch @ '0'..='7') => {
                let mut value = ch.to_digit(8).unwrap();
                for _ in 0..2 {
                    let Some(next) = self.rest.chars().next().and_then(|c| c.to_digit(8)) else {
                        break;
                    };
                    self.take_char();
                    value = value * 8 + next;
                }
                char::from_u32(value).expect("三位八进制在字符范围内")
            }
            _ => {
                return Err(syntax(
                    self.line,
                    "不支持的字符串转义，请用原始字符串表示正则",
                ));
            }
        };
        out.push(value);
        Ok(())
    }

    fn f_field(
        &mut self,
        depth: usize,
        literal: &mut String,
    ) -> Result<Vec<Token<'a>>, SyntaxError> {
        let mut tokens = Vec::new();
        loop {
            self.skip_space();
            if self.rest.starts_with(['}', ':'])
                || (self.rest.starts_with('!') && !self.rest.starts_with("!="))
            {
                break;
            }
            if self.rest.is_empty() || self.rest.starts_with('\n') {
                return Err(syntax(self.line, "f-string 的插值必须闭合且在同一逻辑行"));
            }
            tokens.push(self.token(depth)?);
        }
        if self.eat("!") && !matches!(self.take_char(), Some('s' | 'r' | 'a')) {
            return Err(syntax(self.line, "f-string 转换符只能是 !s、!r 或 !a"));
        }
        if self.eat(":") {
            let format = self.rest.split(['}', '{', '\n']).next().unwrap();
            // 支持常见的固定格式（例如 .2f），不接受嵌套格式或动态宽度。
            if !format
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"<>=+ -_.%,#".contains(&b))
            {
                return Err(syntax(self.line, "不支持的 f-string 格式"));
            }
            literal.push('\n');
            literal.push_str(format);
            self.eat(format);
        }
        if !self.eat("}") || tokens.is_empty() {
            return Err(syntax(self.line, "f-string 插值无效或格式含嵌套字段"));
        }
        Ok(tokens)
    }
}

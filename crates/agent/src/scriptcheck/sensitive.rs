//! 字符串字面量和模型输入共用的敏感值形态识别。

use std::ops::Range;
use std::sync::LazyLock;

use regex_automata::meta::Regex;

const MASK: &str = "（已脱敏）";

fn spans(text: &str) -> Vec<Range<usize>> {
    static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        [
            r"(?i-u:\b[A-Z0-9.!#$%&'*+/=?^_{|}~-]+@[A-Z0-9-]+(?:\.[A-Z0-9-]+)+\b)",
            r#"(?i-u:\b(?:account|username|user_name|password|passwd|pwd|token|api[_-]?key)\b)[ \t]*[:=][ \t]*["']?[A-Za-z0-9@_.!+/$~-]+"#,
            r#"(?:账号|帐号|用户名|密码|口令|令牌|设备序列号)[ \t]*[:：=][ \t]*["']?[A-Za-z0-9@_.!+/$~-]+"#,
            r"(?i-u:\badb\b|\bemulator-[0-9]{4,5}\b|\b[0-9]{1,3}(?:\.[0-9]{1,3}){3}:[0-9]{2,5}\b)",
            r#"(?m)(?:^|[\s"'(=,:（：])((?:[A-Za-z]:[\\/]|\\\\|/)[^/\s"'\\,:;()<>][^\s"'<>，；）]*)"#,
            r#"(?i-u:file:///[A-Z0-9_])[^ \t\r\n"'<>]*"#,
            r"(?i-u:\b(?:sk-|sk_|gh[pousr]_|github_pat_|xox[baprs]-|akia|aiza)[A-Z0-9_-]{12,}\b)",
            r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b",
            r"\b[0-9]{8,}\b",
        ]
        .into_iter()
        .map(|pattern| Regex::new(pattern).expect("敏感值正则有效"))
        .collect()
    });
    static TOKEN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[A-Za-z0-9_+.-]{8,}").expect("令牌形态正则有效"));
    let mut ranges = Vec::new();
    for pattern in PATTERNS.iter() {
        for m in pattern.captures_iter(text) {
            ranges.push(
                m.get_group(1)
                    .map_or_else(|| m.get_match().unwrap().range(), |span| span.range()),
            );
        }
    }
    for m in TOKEN.find_iter(text) {
        let token = &text[m.start()..m.end()];
        let digits = token.bytes().filter(u8::is_ascii_digit).count();
        let uppercase = token.bytes().filter(u8::is_ascii_uppercase).count();
        let lowercase = token.bytes().filter(u8::is_ascii_lowercase).count();
        let hex_serial = digits >= 1 && token.bytes().all(|b| b.is_ascii_hexdigit());
        let usb_serial = digits >= 4 && uppercase >= 2 && digits + uppercase == token.len();
        let random = token.len() >= 16
            && digits >= 2
            && digits + uppercase + lowercase == token.len()
            && ((uppercase > 0 && lowercase > 0) || (token.len() >= 24 && lowercase >= 8))
            && token
                .bytes()
                .collect::<std::collections::HashSet<_>>()
                .len()
                >= 9;
        if hex_serial || usb_serial || random {
            ranges.push(m.range());
        }
    }
    ranges
}

pub(super) fn contains(text: &str) -> bool {
    !spans(text).is_empty()
}

pub(crate) fn redact(text: &str) -> String {
    let mut ranges = spans(text);
    ranges.sort_unstable_by_key(|range| range.start);
    let mut out = String::with_capacity(text.len());
    let mut end = 0;
    for range in ranges {
        if range.start >= end {
            out.push_str(&text[end..range.start]);
            out.push_str(MASK);
        }
        end = end.max(range.end);
    }
    out.push_str(&text[end..]);
    out
}

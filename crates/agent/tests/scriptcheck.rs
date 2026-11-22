use std::collections::BTreeMap;

use agent::scriptcheck::{UiContract, Violation, check};

const CONTRACT: &str = include_str!("fixtures/scriptgen/ui_contract.json");
const CASE: &str = "TC-04";
const TEST: &str = "test_tc04.py";
const ASSERT: &str = r#"assert ui.has("Music"), "预期显示 Music""#;

fn source(imports: &str, body: &str) -> String {
    let body: String = body.lines().map(|line| format!("    {line}\n")).collect();
    format!("{imports}\nimport pytest\n\n@pytest.mark.case(\"TC-04\")\ndef test_tc04(ui):\n{body}")
}

fn run(text: &str, expected: usize) -> Vec<Violation> {
    let contract = UiContract::from_json(CONTRACT.as_bytes()).unwrap();
    check(
        &BTreeMap::from([(TEST.into(), text.into())]),
        &contract,
        CASE,
        expected,
    )
}

fn valid(text: &str, expected: usize) {
    let issues = run(text, expected);
    assert!(issues.is_empty(), "{issues:?}");
}

fn invalid(text: &str, expected: usize, rule: u8, line: Option<usize>) {
    let issues = run(text, expected);
    assert!(
        issues.iter().any(|issue| issue.file == TEST
            && issue.rule == rule
            && line.is_none_or(|line| issue.line == line)),
        "{issues:?}"
    );
}

#[test]
fn 原样复制的七组_poc_脚本仅_tc10_有设备路径违规() {
    let contract = UiContract::from_json(CONTRACT.as_bytes()).unwrap();
    let mut unexpected = BTreeMap::new();
    for (case, expected, test, page) in [
        (
            "TC-01",
            5,
            include_str!("fixtures/scriptgen/generated/test_tc01.py"),
            include_str!("fixtures/scriptgen/generated/pages/tc01_pages.py"),
        ),
        (
            "TC-02",
            4,
            include_str!("fixtures/scriptgen/generated/test_tc02.py"),
            include_str!("fixtures/scriptgen/generated/pages/tc02_pages.py"),
        ),
        (
            "TC-04",
            5,
            include_str!("fixtures/scriptgen/generated/test_tc04.py"),
            include_str!("fixtures/scriptgen/generated/pages/tc04_pages.py"),
        ),
        (
            "TC-06b",
            3,
            include_str!("fixtures/scriptgen/generated/test_tc06b.py"),
            include_str!("fixtures/scriptgen/generated/pages/tc06b_pages.py"),
        ),
        (
            "TC-06c",
            3,
            include_str!("fixtures/scriptgen/generated/test_tc06c.py"),
            include_str!("fixtures/scriptgen/generated/pages/tc06c_pages.py"),
        ),
        (
            "TC-08",
            5,
            include_str!("fixtures/scriptgen/generated/test_tc08.py"),
            include_str!("fixtures/scriptgen/generated/pages/tc08_pages.py"),
        ),
        (
            "TC-10",
            5,
            include_str!("fixtures/scriptgen/generated/test_tc10.py"),
            include_str!("fixtures/scriptgen/generated/pages/tc10_pages.py"),
        ),
    ] {
        let key = case.replace('-', "").to_ascii_lowercase();
        let files = BTreeMap::from([
            (format!("test_{key}.py"), test.into()),
            (format!("pages/{key}_pages.py"), page.into()),
        ]);
        let issues = check(&files, &contract, case, expected);
        let matches_baseline = if case == "TC-10" {
            matches!(issues.as_slice(), [issue]
                if issue.file == "test_tc10.py" && issue.line == 8 && issue.rule == 5
                    && test.lines().nth(issue.line - 1).unwrap().starts_with("DEL_FILE_PATH ="))
        } else {
            issues.is_empty()
        };
        if !matches_baseline {
            unexpected.insert(case, issues);
        }
    }
    assert!(unexpected.is_empty(), "与 PoC 基线不符：{unexpected:#?}");
}

#[test]
fn 规则一解析多行表达式字符串和缩进并报告语法错误() {
    valid(
        &source(
            "",
            &format!(
                "value = (\n    ui.has('M' 'usic'),\n    1 + 2 * 3,\n)\nassert value[0], f'预期 {{value[1]:.2f}}'\n{ASSERT}"
            ),
        ),
        1,
    );
    for invalid_body in [
        "if True\n    pass",
        "ui.tap('Music'",
        "x = 1 +",
        "assert foo bar",
        "x = '未闭合",
        "if True:\npass",
        "x = f'{1+}'",
        "x = 012",
        "return\nbreak",
    ] {
        invalid(
            &source("", &format!("{invalid_body}\n{ASSERT}")),
            1,
            1,
            None,
        );
    }
    invalid(
        "import pytest\n@pytest.mark.case('TC-04')\ndef test_tc04(ui)\n    assert True\n",
        1,
        1,
        Some(3),
    );
}

#[test]
fn 规则二精确导入白名单并拒绝其他用例与星号() {
    valid(
        &source(
            "import re as rx, time\nfrom generated.pages.tc04_pages import (\n    RoomPage as Room,\n)",
            ASSERT,
        ),
        1,
    );
    for imports in [
        "import json",
        "import re, dataclasses as dc",
        "from generated.pages.tc08_pages import Room",
        "from .pages.tc04_pages import Room",
        "from re import *",
    ] {
        invalid(&source(imports, ASSERT), 1, 2, Some(1));
    }
}

#[test]
fn 运行时的参数名契约补齐签名并支持模块前缀通配() {
    let signatures = UiContract::from_json(CONTRACT.as_bytes()).unwrap();
    let runtime = serde_json::json!({
        "ui_api": {"tap": ["text", "xy", "exact", "scroll", "wait"]},
        "allowed_imports": ["pytest", "hafa_automation.*"]
    });
    let contract =
        UiContract::from_runtime_json(runtime.to_string().as_bytes(), &signatures).unwrap();
    let check_body = |imports: &str, body: &str| {
        check(
            &BTreeMap::from([(TEST.into(), source(imports, body))]),
            &contract,
            CASE,
            1,
        )
    };
    assert!(
        check_body(
            "import hafa_automation.runtime",
            "ui.tap('More', exact=True)\nassert True, '预期出现面板'"
        )
        .is_empty()
    );
    let issues = check_body(
        "import hafa_automation_other.runtime",
        "ui.tap('More', True)\nassert True",
    );
    assert_eq!(issues.iter().map(|v| v.rule).collect::<Vec<_>>(), [2, 4]);
    let mut changed = runtime;
    changed["ui_api"]["tap"] = serde_json::json!(["text", "exact"]);
    assert!(UiContract::from_runtime_json(changed.to_string().as_bytes(), &signatures).is_err());
}

#[test]
fn 规则三禁止执行文件进程网络和反射且忽略注释() {
    valid(
        &source("# open(), eval() 和 subprocess 是禁止的词", ASSERT),
        1,
    );
    valid(
        &source(
            "import re as rx",
            &format!("matcher = rx.compile('Music')\n{ASSERT}"),
        ),
        1,
    );
    for body in [
        "exec('pass')",
        "eval('True')",
        "compile('pass', 'test', 'exec')",
        "open('test.py')",
        "__import__('re')",
        "os.getcwd()",
        "sys.exit()",
        "subprocess.run([])",
        "socket.socket()",
        "requests.get('test')",
        "urllib.parse.urlsplit('test')",
        "shutil.which('tool')",
        "pathlib.Path('test.py')",
        "f'{getattr(ui, \"bad_method\")}'",
    ] {
        invalid(&source("", &format!("{body}\n{ASSERT}")), 1, 3, Some(6));
    }
    invalid(&source("import pathlib as p", ASSERT), 1, 3, Some(1));
}

#[test]
fn 规则四检查_ui_位置参数关键字和定长元组展开() {
    valid(
        &source(
            "",
            &format!(
                "ui.tap('Music', exact=True, xy=(1, 2))\nui.wait_text(text='Music', timeout=2)\n\
         point = (1, 2)\nui.tap_xy(*point, wait=1)\n{ASSERT}"
            ),
        ),
        1,
    );
    for call in [
        "ui.unknown('Music')",
        "self.ui.unknown()",
        "ui.tap()",
        "ui.tap('Music', True)",
        "ui.tap_xy(1)",
        "ui.tap_xy(*(1, 2, 3, 4))",
        "ui.wait_text('Music', seconds=2)",
        "ui.tap('Music', text='Music')",
        "ui.tap(**options)",
        "ui.tap_xy(*location)",
        "ui.dev.tap(1, 2)",
        "ui.tap",
    ] {
        invalid(&source("", &format!("{call}\n{ASSERT}")), 1, 4, Some(6));
    }
}

#[test]
fn 注入的契约区分仅位置与仅关键字参数() {
    let contract = UiContract::from_json(
        br#"{
        "allowed_imports": ["pytest"],
        "methods": {"look": {"params": [
            {"name": "text", "kind": "POSITIONAL_ONLY", "required": true},
            {"name": "near", "kind": "KEYWORD_ONLY", "required": true}
        ]}}
    }"#
        .as_slice(),
    )
    .unwrap();
    let check_call = |call: &str| {
        check(
            &BTreeMap::from([(
                TEST.into(),
                source("", &format!("assert {call}, '预期文字'")),
            )]),
            &contract,
            CASE,
            1,
        )
    };
    assert!(check_call("ui.look('Music', near='Title')").is_empty());
    for call in [
        "ui.look(text='Music', near='Title')",
        "ui.look('Music', 'Title')",
        "ui.look('Music')",
    ] {
        let issues = check_call(call);
        assert!(
            matches!(issues.as_slice(), [issue] if issue.rule == 4),
            "{issues:?}"
        );
    }
}

#[test]
fn 规则五放行界面文案并拦住敏感字面量和常量拼接() {
    valid(
        &source(
            "",
            &format!(
                r#"pattern = r"^Music\(\d+/\d+\)$"
title = "曲名 / 歌手"
{ASSERT}"#
            ),
        ),
        1,
    );
    for text in [
        r#""test-user@example.test""#,
        r#""test-user\x40example.test""#,
        r#""test-user@" "example.test""#,
        r#""test-user@" + "example.test""#,
        r#""q7T3Vm2Nc5fD8LzX""#,
        r#""password=ShortPw""#,
        r#""/Users/demo/sample""#,
        r#""/some/project""#,
        r#"r"C:\demo\project""#,
        r#""call ADB now""#,
        r#""emulator-5554""#,
        r#""13579024""#,
        r#""R58M147ABCD""#,
        r#""192.168.0.4:5555""#,
        r#"f"{'test-user@example.test'}""#,
    ] {
        invalid(
            &source("", &format!("value = {text}\n{ASSERT}")),
            1,
            5,
            Some(6),
        );
    }
}

#[test]
fn 规则六标记必须附在本用例的测试函数并校验编号() {
    let test = source("", ASSERT);
    valid(&test, 1);
    invalid(&test.replace("TC-04", "TC-08"), 1, 6, Some(5));
    invalid(&test.replace("@pytest", "# @pytest"), 1, 6, Some(5));
    invalid(&test.replace("def test_tc04", "def helper"), 1, 6, Some(1));
    invalid(
        &test.replace(
            "def test_tc04(ui):",
            "def helper(): pass\ndef test_tc04(ui):",
        ),
        1,
        6,
        Some(6),
    );
}

#[test]
fn 规则七验证有界_for_及_while_计数和跳出路径() {
    for body in [
        "for _ in range(12):\n    ui.swipe('up')",
        "for label in ui.labels():\n    assert label",
        "labels = ui.labels()\nassert all(label for label in labels)",
        "for label in ['Music', 'More']:\n    assert label",
        "i = 0\nwhile True:\n    if i >= 4:\n        break\n    i += 1\n    ui.swipe('up')",
        "i = 0\nwhile i < 4:\n    i = i + 1\n    ui.swipe('up')",
    ] {
        valid(&source("", &format!("{body}\n{ASSERT}")), 1);
    }
    for body in [
        "for _ in iter(int, 1):\n    pass",
        "items = [item for item in iter(int, 1)]",
        "iterator = iter(int, 1)\nitems = [item for item in iterator]",
        "for item in items:\n    pass",
        "range = other\nfor item in range(2):\n    pass",
        "while True:\n    pass",
        "while True:\n    break",
        "i = 0\nwhile True:\n    i += 1\n    if i >= 4:\n        break\n    i = 0",
        "i = 0\nwhile True:\n    if ui.has('More'):\n        continue\n    i += 1\n    if i >= 4:\n        break",
        "i = 0\nwhile True:\n    i += 1",
        "while ui.has('More'):\n    ui.swipe('up')",
    ] {
        invalid(&source("", &format!("{body}\n{ASSERT}")), 1, 7, None);
    }
}

#[test]
fn 推导式迭代_itertools_count_时拒绝() {
    let test = source(
        "",
        &format!("items = [x for x in itertools.count()]\n{ASSERT}"),
    );
    invalid(&test, 1, 7, Some(6));
}

#[test]
fn 推导式迭代字符串_split_结果时放行() {
    valid(
        &source(
            "",
            &format!(
                r#"parts = [p.strip() for p in row.split(" / ")]
{ASSERT}"#
            ),
        ),
        1,
    );
}

#[test]
fn 推导式迭代双参数_iter_时拒绝() {
    let test = source("", &format!("items = [x for x in iter(f, None)]\n{ASSERT}"));
    invalid(&test, 1, 7, Some(6));
}

#[test]
fn 推导式允许常见有限迭代源与包装函数() {
    for iterable in [
        "values",
        "row.items",
        "row[1:]",
        "(1, 2)",
        "[]",
        "{'name': 1}",
        "{1, 2}",
        "range(1, 2 * (3 + 0xA), +1)",
        "fetch_rows()",
        "page.panel().rows()",
        "enumerate(iterable=values, start=1)",
        "sorted(reversed(list(row.items)), reverse=True)",
        "tuple(set(values))",
        "zip(values, row.items, range(3))",
        "map(convert, values, row.items)",
        "filter(predicate, values)",
        "list(value for value in values)",
        "iter(values)",
    ] {
        valid(
            &source("", &format!("items = [x for x in {iterable}]\n{ASSERT}")),
            1,
        );
    }
}

#[test]
fn 推导式_range_参数必须是整数常量表达式() {
    for iterable in ["range(limit)", "range(2.0)", "list(range(4 / 2))"] {
        invalid(
            &source("", &format!("items = [x for x in {iterable}]\n{ASSERT}")),
            1,
            7,
            Some(6),
        );
    }
}

#[test]
fn 推导式拒绝别名和有限包装内的无界迭代器() {
    let mut contract = UiContract::from_json(CONTRACT.as_bytes()).unwrap();
    contract.allowed_imports.push("itertools".into());
    let check_iterators = |imports: &str, body: &str| {
        check(
            &BTreeMap::from([(TEST.into(), source(imports, &format!("{body}\n{ASSERT}")))]),
            &contract,
            CASE,
            1,
        )
    };
    let imports = "import itertools as it\nfrom itertools import count as counter, repeat";
    for body in [
        "items = [x for x in zip(range(3), counter())]",
        "values = it.cycle(['Music'])\nitems = [x for x in values]",
        "items = [x for x in repeat('Music')]",
        "items = [x for x in list(iter(f, None))]",
    ] {
        let issues = check_iterators(imports, body);
        assert!(
            matches!(issues.as_slice(), [issue] if issue.rule == 7),
            "{issues:?}"
        );
    }
    let issues = check_iterators(
        imports,
        "items = [x for x in repeat('Music', times=2) for y in it.repeat('More', 3)]",
    );
    assert!(issues.is_empty(), "{issues:?}");
}

#[test]
fn 无上限的_ui_轮询即使经函数封装也被拒绝() {
    let test = source(
        "def poll(ui):\n    while ui.has('Music'):\n        pass\n    return []",
        &format!("items = [x for x in poll(ui)]\n{ASSERT}"),
    );
    invalid(&test, 1, 7, Some(2));
}

#[test]
fn while_驱动的生成器仍不属于语法子集() {
    let test = source(
        "def poll(ui):\n    while True:\n        yield ui.has('Music')",
        &format!("items = [x for x in poll(ui)]\n{ASSERT}"),
    );
    invalid(&test, 1, 1, Some(3));
}

#[test]
fn 规则七检查异常处理块内的循环控制和计数器绑定() {
    for handler in ["except Exception: continue", "except Exception as i: pass"] {
        let test = source(
            "",
            &format!(
                "i = 0\nwhile True:\n    try: pass\n    {handler}\n    i += 1\n    if i >= 3: break\n{ASSERT}"
            ),
        );
        invalid(&test, 1, 7, Some(7));
    }
}

#[test]
fn 规则八只统计测试函数内的显式断言并包含内层语句块() {
    let one = source(
        "def helper():\n    assert True",
        &format!("# assert True\ntext = 'assert True'\nif ui.has('Music'):\n    {ASSERT}"),
    );
    valid(&one, 1);
    invalid(&one, 2, 8, None);
    invalid(
        &source("", "def helper():\n    assert True\nhelper()"),
        1,
        8,
        Some(5),
    );
    valid(
        &source("", &format!("{ASSERT}\nfor _ in range(2):\n    {ASSERT}")),
        2,
    );
}

#[test]
fn 裸_except_按规则八拒绝() {
    let test = source("", &format!("try:\n    pass\nexcept:\n    pass\n{ASSERT}"));
    invalid(&test, 1, 8, Some(8));
}

#[test]
fn except_base_exception_按规则八拒绝() {
    let test = source(
        "",
        &format!("try: pass\nexcept BaseException as error: raise\n{ASSERT}"),
    );
    invalid(&test, 1, 8, Some(7));
}

#[test]
fn try_体有断言且处理块仅_pass_时报吞掉断言() {
    let test = source(
        "",
        &format!("try:\n    {ASSERT}\nexcept AssertionError:\n    pass\n{ASSERT}"),
    );
    let issues = run(&test, 1);
    assert!(
        matches!(issues.as_slice(), [issue]
            if issue.line == 8 && issue.rule == 8 && issue.message.contains("吞掉断言")),
        "{issues:?}"
    );
}

#[test]
fn try_体有断言且处理块重新抛出时放行() {
    let test = source(
        "",
        &format!(
            "try:\n    {ASSERT}\nexcept AssertionError:\n    raise\nexcept Exception as error:\n    message = str(error)\n    raise\n{ASSERT}"
        ),
    );
    valid(&test, 1);
}

#[test]
fn try_体没有断言且处理块返回_false_时放行() {
    let test = source(
        "",
        &format!(
            "def wait_for_music():\n    try:\n        ui.wait_text('Music', timeout=5)\n        return True\n    except AssertionError:\n        return False\nassert wait_for_music()\n{ASSERT}"
        ),
    );
    valid(&test, 1);
}

#[test]
fn try_体内的断言不计入预期条数() {
    let test = source(
        "",
        &format!(
            "try:\n    if ui.has('Music'):\n        {ASSERT}\nexcept AssertionError:\n    raise\n{ASSERT}"
        ),
    );
    valid(&test, 1);
    let issues = run(&test, 2);
    assert!(
        matches!(issues.as_slice(), [issue] if issue.rule == 8 && issue.line == 5),
        "{issues:?}"
    );
}

#[test]
fn finally_块继续执行现有静态校验() {
    let test = source(
        "",
        &format!(
            "try: pass\nexcept AssertionError: pass\nelse: pass\nfinally:\n    open('music.txt')\n    {ASSERT}"
        ),
    );
    let issues = run(&test, 1);
    assert!(
        matches!(issues.as_slice(), [issue] if issue.rule == 3 && issue.line == 10),
        "{issues:?}"
    );
}

#[test]
fn except_标题或块的语法错误仍报规则一() {
    for handler in [
        "except",
        "except AssertionError",
        "except as error: pass",
        "except AssertionError as: pass",
        "except AssertionError extra: pass",
        "except AssertionError:\npass",
        "except AssertionError: pass\nfinally: pass\nexcept RuntimeError: pass",
    ] {
        let test = source("", &format!("try: pass\n{handler}\n{ASSERT}"));
        let issues = run(&test, 1);
        assert!(
            matches!(issues.as_slice(), [issue] if issue.rule == 1),
            "{issues:?}"
        );
    }
}

#[test]
fn 断言函数调用要求每条处理路径传播失败() {
    let test = |assertion: &str, handler: &str| {
        source(
            "",
            &format!("try:\n    {assertion}\nexcept Exception:\n    {handler}\n{ASSERT}"),
        )
    };
    for assertion in ["f'{page.assert_ready()}'", "(page.assert_ready)()"] {
        for handler in [
            "pass",
            "if ui.has('More'): raise",
            "if ui.has('More'): return\n    raise",
            "pytest.skip('中止')\n    raise",
            "pytest.fail((pytest.skip)('中止'))",
        ] {
            invalid(&test(assertion, handler), 1, 8, Some(8));
        }
    }
    valid(&test("assert_ready()", "pytest.fail('断言失败')"), 1);
    valid(
        &test(
            "page.assert_ready()",
            "if ui.has('More'): raise\n    else: pytest.fail('断言失败')",
        ),
        1,
    );
}

#[test]
fn finally_不能用返回压掉断言失败() {
    let test = source(
        "",
        &format!(
            "{ASSERT}\ntry:\n    {ASSERT}\nexcept AssertionError:\n    raise\nfinally:\n    return"
        ),
    );
    invalid(&test, 1, 8, Some(11));
}

#[test]
fn 规则九固定等待最多三秒且不可隐藏为别名() {
    valid(
        &source(
            "import time as clock",
            &format!(
                "clock.sleep(3.0)\nui.wait(seconds=2)\nui.wait_text('Music', timeout=20)\n{ASSERT}"
            ),
        ),
        1,
    );
    for body in [
        "time.sleep(3.1)",
        "sleep(delay)",
        "delay = time.sleep",
        "ui.wait(4)",
        "ui.wait(seconds=limit)",
    ] {
        invalid(
            &source("import time", &format!("{body}\n{ASSERT}")),
            1,
            9,
            Some(6),
        );
    }
    invalid(
        &source("from time import sleep as pause", ASSERT),
        1,
        9,
        Some(1),
    );
}

//! 对本机真实 Claude、Codex 跑一遍「根据文档和截图生成用例」（默认忽略，花少量额度）：
//! `cargo test -p agent --test casegen_real -- --ignored --nocapture`
//! 只用合成文档和合成截图（一张 256 像素的四象限图，左上角红色、其余蓝色）。

use agent::casegen::{self, Image, Input};
use base64::Engine as _;
use protocol::ModelSource;

const QUAD_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAQAAAAEACAIAAADTED8xAAAESUlEQVR4nO3OsQ0AMBCEsN9/6WQMF4fkHu7dBdL9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0f57ur9P9ebq/Tvfn6f463Z+n++t0P/4ggfxBAvmDBPIHCeQPEsgfJJA/SCB/kED+IIH8QQL5gwTyBwnkDxLIHySQP0ggf5BA/iCB/EEC+YME8gcJ5A8SyB8kkD9IIH+QQP4ggfxBAvmDBPIHCeQPEsgfJJA/SCB/kED+IIH8QQL5gwTyBwnkDxLIHySQP0ggf5BA/iCB/EEC+YME8gcJ5A8SyB8kkD9IIH+QQP4ggfxBAvmDBPIHCeQPEsgfJJA/SCB/kED+IIH8QQL5gwTyBwnkDxLIHySQP0ggf5BA/iCB/EEC+YME8gcJ5A8SyB8kkD9IIH+QQP4ggfxBAvmDBPIHCeQPEsgfJJA/SCB/kED+IIH8QQL5gwTyBwnkDxLIHySQP0ggf5BA/iCB/EEC+YME8gcJ5A8SyB8kkD9IIH+QQP4ggfxBAvmDBPIHCeQPEsgfJJA/SCB/kED+IIH8QQL5gwTyBwnkDxLIHySQP0ggf5BA/iCB/EEC+YME8gcJ5A8SyB8kkD9IIH+QQP4ggfxBAvmDBPIHCeQPEsgfJJA/SCB/kDAfwtsO8oTbVcUAAAAASUVORK5CYII=";

fn quad() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(QUAD_PNG_B64).unwrap()
}

const DOC: &str = "# 红色提示块\n页面左上角有一个红色提示块，点击它会弹出「提示详情」对话框；点击对话框里的「关闭」按钮后对话框消失。\n登录账号使用 {{account.username}}。";

async fn run(source: ModelSource, model: &str) {
    let images = [Image { media_type: "image/png".into(), bytes: quad() }];
    let links = vec!["https://example.com/design/synthetic".to_string()];
    let out = casegen::generate(source, Some(model.into()), None, &Input { module: "提示块", doc: DOC, ui_links: &links, images: &images }).await;
    println!("{source:?}（{:.0} 秒）：失败={:?}\n{}\n", out.wall_s, out.failure, out.text);
    assert!(out.failure.is_none(), "{source:?} 失败：{:?}", out.failure);
    assert!(out.text.contains("steps:") && out.text.contains("expected:"), "输出不像用例：{}", out.text);
    assert!(!out.text.to_lowercase().contains("password:") || out.text.contains("{{account."), "不能有明文密码");
}

#[tokio::test]
#[ignore]
async fn claude_看图生成用例() {
    run(ModelSource::ClaudeCli, "claude-opus-5-5").await;
}

#[tokio::test]
#[ignore]
async fn codex_看图生成用例() {
    run(ModelSource::CodexExec, "gpt-6.2-astra").await;
}

fn brief(code: &str, title: &str, steps: &[&str], expected: &[&str]) -> casegen::CaseBrief {
    casegen::CaseBrief {
        code: code.into(),
        title: title.into(),
        preconditions: vec![],
        steps: steps.iter().map(|s| s.to_string()).collect(),
        expected: expected.iter().map(|s| s.to_string()).collect(),
    }
}

/// 复核：第一条完全来自文档，第二条编造了文档里没有的「长按分享」功能。只断言输出能解析、每条都有结论，
/// 并打印评审员的判断供人看（模型的判断质量不在这里断言）。
async fn review_run(source: ModelSource, model: &str) {
    let cases = [
        brief("A1", "点击红色提示块弹出详情", &["点击左上角红色提示块"], &["弹出标题为「提示详情」的对话框"]),
        brief("A2", "长按提示块分享给好友", &["长按左上角红色提示块"], &["弹出分享面板，可以选择好友发送"]),
    ];
    let out = casegen::review(source, Some(model.into()), None, DOC, &cases).await;
    println!("{source:?}（{:.0} 秒）：失败={:?}\n{}\n", out.wall_s, out.failure, out.text);
    assert!(out.failure.is_none(), "{source:?} 失败：{:?}", out.failure);
    let reviews = casegen::parse_reviews(&out.text).expect("输出应是合法的复核 JSON");
    for code in ["A1", "A2"] {
        let r = reviews.iter().find(|r| r.code == code).unwrap_or_else(|| panic!("没有给 {code} 结论"));
        assert!(matches!(r.verdict.as_str(), "ok" | "needs_human"), "{code} 的结论不认识：{}", r.verdict);
        println!("{code} → {}：{}", r.verdict, r.reason);
    }
}

#[tokio::test]
#[ignore]
async fn claude_复核用例() {
    review_run(ModelSource::ClaudeCli, "claude-opus-5-5").await;
}

#[tokio::test]
#[ignore]
async fn agy_复核用例() {
    review_run(ModelSource::Agy, "gemini-4-neon-high").await;
}

/// 管理员对真实 Claude 说一句话（默认忽略，花少量额度）：合成的项目现状，一条已批准的用例，让它提议发起测试。
/// 只断言「回答能解析、提议在白名单里」，并打印回答供人看。
#[tokio::test]
#[ignore]
async fn claude_管理员回答并提议() {
    let ask = protocol::AdminAsk {
        ask_id: uuid::Uuid::nil(),
        role_id: None,
        context: "【需求文档（最新）】\n语聊房：用户可以进入语聊房、上麦。\n\n【模块】\n- 模块「语聊房」：用例 2 条，已批准 1 条，需要人工确认 1 条，正在测试的 run 0 个\n    已批准用例 GEN-aaaaaa-01：进入语聊房\n".into(),
        history: vec![],
        message: "语聊房模块现在能测吗？能的话帮我安排一下。".into(),
    };
    let (reply, proposals, dropped) = agent::admin::answer(ModelSource::ClaudeCli, Some("claude-opus-5-5".into()), None, &ask).await.expect("管理员应当回答成功");
    println!("回答：{reply}\n提议：{proposals:#?}\n被丢掉：{dropped}");
    assert!(!reply.trim().is_empty());
    assert!(!reply.contains("<proposals>"), "回答正文里不应留下提议块");
    assert!(proposals.iter().all(|p| agent::admin::KINDS.contains(&p.kind.as_str())));
}

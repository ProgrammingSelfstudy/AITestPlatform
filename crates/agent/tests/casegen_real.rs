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

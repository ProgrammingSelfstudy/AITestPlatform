//! 真机上的 setup / teardown：Rust 运行器调起 Python 自动化脚本。
//! 会真的开房、清空音乐库、关房，只在测试机上跑。不处理登录（假定手机上已经是测试账号）。
//!
//!     cargo test -p node --test setup_real -- --ignored --nocapture
//! 环境变量：HAFA_TEST_PYTHON（装了 uiautomator2 的 Python）、HAFA_TEST_AUDIO（测试音频目录）、HAFA_TEST_SERIAL。

use std::time::Duration;

use node::setup_runner::{run_setup, SetupResult, SetupSpec};

fn spec() -> SetupSpec {
    let get = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("请设置环境变量 {k}"));
    let automation = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../automation");
    let mut s = SetupSpec::new(get("HAFA_TEST_PYTHON"), automation, &get("HAFA_TEST_SERIAL"), "com.hafa.pikka", "com.hafa.pikka.MainActivity");
    s.audio_dir = get("HAFA_TEST_AUDIO").into();
    s.state_dir = std::env::temp_dir().join("hafa_real_state");
    s.timeout = Duration::from_secs(900);
    s
}

#[tokio::test]
#[ignore]
async fn 真机_tc04的setup再teardown() {
    let mut s = spec();
    s.case = Some("TC-04".into());
    let r = run_setup(&s, |e| println!("[{}] {} {}", e.kind, e.ok.map(|o| if o { "✅" } else { "❌" }).unwrap_or(""), e.msg)).await;
    println!("setup 结果：{r:?}");
    assert_eq!(r, SetupResult::Ok, "setup 失败：{r:?}");

    let mut t = spec();
    t.teardown = true;
    let r = run_setup(&t, |e| println!("[{}] {} {}", e.kind, e.ok.map(|o| if o { "✅" } else { "❌" }).unwrap_or(""), e.msg)).await;
    println!("teardown 结果：{r:?}");
    assert_eq!(r, SetupResult::Ok, "teardown 失败：{r:?}");
}

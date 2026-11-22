use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine;
use mcp_device::evidence::{catalog, step_text_catalog};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_mcp-evidence");

struct RunDir(PathBuf);

impl RunDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "hafa-evidence-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        fs::create_dir(dir.join("shots")).unwrap();
        Self(dir)
    }

    fn shot(&self, name: &str) -> Vec<u8> {
        let png = mcp_device::observe::to_png(&image::RgbImage::new(1, 1));
        fs::write(self.0.join("shots").join(name), &png).unwrap();
        png
    }

    fn trace(&self, steps: Value) {
        fs::write(
            self.0.join("trace.json"),
            json!({"steps": steps}).to_string(),
        )
        .unwrap();
    }

    async fn connect(&self) -> RunningService<RoleClient, ()> {
        let mut command = Command::new(BIN);
        command
            .env("HAFA_RUN_DIR", &self.0)
            .env("RUST_LOG", "error");
        ().serve(TokioChildProcess::new(command).unwrap())
            .await
            .unwrap()
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn request(name: &str) -> CallToolRequestParams {
    CallToolRequestParams::new("image")
        .with_arguments(json!({"name": name}).as_object().unwrap().clone())
}

fn step_request(step: i64) -> CallToolRequestParams {
    CallToolRequestParams::new("step_text")
        .with_arguments(json!({"step": step}).as_object().unwrap().clone())
}

async fn assert_unavailable(client: &RunningService<RoleClient, ()>, name: &str) {
    let result = client.call_tool(request(name)).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    assert_eq!(result.content.len(), 1);
    assert_eq!(
        result.content[0].as_text().unwrap().text,
        "没有可用的清单内截图"
    );
}

async fn assert_step_unavailable(client: &RunningService<RoleClient, ()>, step: i64) {
    let result = client.call_tool(step_request(step)).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(result.structured_content.is_none());
    assert_eq!(result.content.len(), 1);
    assert_eq!(
        result.content[0].as_text().unwrap().text,
        "没有可用的步骤文字"
    );
}

#[test]
fn 清单按步骤与阶段排序并标动作目标() {
    let run = RunDir::new();
    for name in [
        "10_final.png",
        "002_after_3.png",
        "002_observe.png",
        "002_before.png",
        "002_after.png",
        "5_before.png",
    ] {
        run.shot(name);
    }
    for invalid in [
        "x_before.png",
        "02_after_.png",
        "02_after_1_2.png",
        "02_other.png",
        "02_after.png.bak",
    ] {
        run.shot(invalid);
    }
    fs::write(
        run.0.join("trace.json"),
        json!({"steps": [
            {"idx": 2, "action": "tap", "target": "音乐\n添加"},
            {"idx": 5, "action": "back", "target": null},
        ]})
        .to_string(),
    )
    .unwrap();

    let actual = catalog(&run.0);
    let pairs = actual
        .iter()
        .map(|(name, label)| (name.as_str(), label.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        pairs,
        [
            ("002_before.png", "步骤 2 · 操作前 tap 音乐 / 添加"),
            (
                "002_observe.png",
                "步骤 2 · 操作前观察（有编号遮挡） tap 音乐 / 添加"
            ),
            ("002_after.png", "步骤 2 · 操作后原图 tap 音乐 / 添加"),
            ("002_after_3.png", "步骤 2 · 操作后原图 tap 音乐 / 添加"),
            ("5_before.png", "步骤 5 · 操作前 back"),
            ("10_final.png", "步骤 10 · 终态原图"),
        ]
    );
}

#[test]
fn 清单拒绝符号链接非_png和非普通文件() {
    let run = RunDir::new();
    let other = RunDir::new();
    run.shot("01_before.png");
    other.shot("03_before.png");
    let shots = run.0.join("shots");
    fs::write(shots.join("02_after.png"), b"\x89PN").unwrap();
    symlink("01_before.png", shots.join("02_before.png")).unwrap();
    symlink(
        other.0.join("shots/03_before.png"),
        shots.join("03_before.png"),
    )
    .unwrap();
    fs::create_dir(shots.join("01_after.png")).unwrap();
    assert_eq!(
        catalog(&run.0),
        [("01_before.png".into(), "步骤 1 · 操作前".into())]
    );

    fs::rename(&shots, run.0.join("内层目录")).unwrap();
    symlink(other.0.join("shots"), &shots).unwrap();
    assert!(catalog(&run.0).is_empty());
}

#[tokio::test]
async fn 只有图像和步骤文字工具且取图不泄露路径() {
    let run = RunDir::new();
    run.shot("01_before.png");
    let client = run.connect().await;
    let tools = client.list_all_tools().await.unwrap();
    let mut names: Vec<_> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(names, ["image", "step_text"]);
    run.shot("02_after.png");
    for name in [
        "../shots/01_before.png",
        "./01_before.png",
        "01..png",
        "trace.json",
        "02_after.png",
        "99_final.png",
    ] {
        assert_unavailable(&client, name).await;
    }
    assert_unavailable(&client, run.0.join("shots/01_before.png").to_str().unwrap()).await;
    assert!(!run.0.join("served.json").exists());
    client.cancel().await.unwrap();
}

#[test]
fn 步骤文字索引按步骤排序并标明有无新文字() {
    let run = RunDir::new();
    run.trace(json!([
        {"idx": 8, "action": "tap", "target": "音乐\n添加", "after_labels": ["出现提示"], "new_labels": ["出现提示"]},
        {"idx": 3, "action": "back", "after_labels": ["选择音乐"], "new_labels": []},
        {"idx": 2, "action": "tap", "after_labels": []},
        {"idx": 1, "action": "need_human"},
    ]));
    assert_eq!(
        step_text_catalog(&run.0),
        [
            (3, "back · 无新出现的文字".into()),
            (8, "tap 音乐 / 添加 · 有新出现的文字".into()),
        ]
    );
}

#[tokio::test]
async fn 无截图目录时也能查看脱敏后的步骤文字字段() {
    let run = RunDir::new();
    fs::remove_dir(run.0.join("shots")).unwrap();
    let fields = json!({
        "action": "tap",
        "target": "添加音乐",
        "after_labels": ["选择音乐", "Music size cannot exceed 50MB"],
        "new_labels": ["Music size cannot exceed 50MB"],
    });
    let mut step = fields.clone();
    step["idx"] = json!(8);
    step["shot"] = json!("shots/08_after.png");
    run.trace(json!([step]));
    let client = run.connect().await;
    let result = client.call_tool(step_request(8)).await.unwrap();
    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.structured_content, Some(fields.clone()));
    assert_eq!(
        serde_json::from_str::<Value>(&result.content[0].as_text().unwrap().text).unwrap(),
        fields
    );
    assert_eq!(
        fs::read_to_string(run.0.join("served.json")).unwrap(),
        r#"["step:8"]"#
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn 步骤不存在或轨迹不可读时固定报错且不泄露路径() {
    let run = RunDir::new();
    let outside = RunDir::new();
    let steps = json!([{"idx": 8, "action": "tap", "after_labels": ["本次步骤"]}]);
    run.trace(steps.clone());
    outside.trace(steps);
    let client = run.connect().await;
    for step in [9, -1, i64::MAX] {
        assert_step_unavailable(&client, step).await;
    }
    let trace = run.0.join("trace.json");
    fs::remove_file(&trace).unwrap();
    symlink(outside.0.join("trace.json"), &trace).unwrap();
    assert_step_unavailable(&client, 8).await;
    assert!(step_text_catalog(&run.0).is_empty());
    assert!(!run.0.join("served.json").exists());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn 清单内文件取图时仍要验证符号链接和_png() {
    let run = RunDir::new();
    let other = RunDir::new();
    run.shot("01_before.png");
    run.shot("01_after.png");
    other.shot("01_before.png");
    let client = run.connect().await;
    let before = run.0.join("shots/01_before.png");
    fs::remove_file(&before).unwrap();
    symlink(other.0.join("shots/01_before.png"), before).unwrap();
    fs::write(run.0.join("shots/01_after.png"), "已被替换").unwrap();
    assert_unavailable(&client, "01_before.png").await;
    assert_unavailable(&client, "01_after.png").await;
    fs::remove_dir_all(run.0.join("shots")).unwrap();
    symlink(other.0.join("shots"), run.0.join("shots")).unwrap();
    assert_unavailable(&client, "01_before.png").await;
    assert!(!run.0.join("served.json").exists());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn 截图和步骤文字共享原子的去重查看记录() {
    let run = RunDir::new();
    let expected = run.shot("01_before.png");
    run.shot("01_after.png");
    run.shot("02_final.png");
    run.trace(json!([{"idx": 8, "action": "tap", "after_labels": ["出现提示"]}]));
    let client = run.connect().await;
    let second = run.connect().await;
    let before = client.call_tool(request("01_before.png")).await.unwrap();
    assert_ne!(before.is_error, Some(true));
    let png = before.content[0].as_image().unwrap();
    assert_eq!(png.mime_type, "image/png");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&png.data)
            .unwrap(),
        expected
    );
    let path = run.0.join("served.json");
    let old = File::open(&path).unwrap();
    let old_inode = old.metadata().unwrap().ino();

    let (after, final_image, repeated, text, repeated_text) = tokio::join!(
        client.call_tool(request("01_after.png")),
        second.call_tool(request("02_final.png")),
        client.call_tool(request("01_before.png")),
        second.call_tool(step_request(8)),
        client.call_tool(step_request(8)),
    );
    for reply in [after, final_image, repeated, text, repeated_text] {
        assert_ne!(reply.unwrap().is_error, Some(true));
    }
    let current = File::open(path).unwrap();
    assert_ne!(
        current.metadata().unwrap().ino(),
        old_inode,
        "发布新数组时要替换文件"
    );
    assert_eq!(
        serde_json::from_reader::<_, Vec<String>>(old).unwrap(),
        ["01_before.png"],
        "已打开的读端仍读到完整旧数组"
    );
    assert_eq!(
        serde_json::from_reader::<_, Vec<String>>(current).unwrap(),
        ["01_after.png", "01_before.png", "02_final.png", "step:8"]
    );
    assert!(
        !fs::read_dir(&run.0)
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
    );
    client.cancel().await.unwrap();
    second.cancel().await.unwrap();
}

#[tokio::test]
async fn 写入查看记录失败时拒绝供图且可以重试() {
    let run = RunDir::new();
    run.shot("01_before.png");
    let client = run.connect().await;
    let path = run.0.join("served.json");
    fs::create_dir(&path).unwrap();
    assert_unavailable(&client, "01_before.png").await;
    fs::remove_dir(&path).unwrap();
    let reply = client.call_tool(request("01_before.png")).await.unwrap();
    assert_ne!(reply.is_error, Some(true));
    assert_eq!(
        serde_json::from_reader::<_, Vec<String>>(File::open(path).unwrap()).unwrap(),
        ["01_before.png"]
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn 缺少轨迹目录时只在标准错误报错() {
    let output = Command::new(BIN)
        .env_remove("HAFA_RUN_DIR")
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("HAFA_RUN_DIR"));
}

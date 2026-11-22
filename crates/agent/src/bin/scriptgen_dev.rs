//! 开发验证入口，产物以 JSON 写到 stdout。

use std::error::Error;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use agent::claude_cli::ClaudeOpts;
use agent::scriptcheck::UiContract;
use agent::scriptgen::{
    GeneratedScripts, ScriptTrace, ScriptgenError, ScriptgenInput, ScriptgenOpts, generate,
};
use protocol::{CaseSpec, FinalResult, JudgeItem, ModelSource, RoleModel};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

const USAGE: &str = "用法：scriptgen_dev <run 目录> <用例.json> [--fake-claude <脚本>] \
[--contract <契约.json>] [--judge <判定.json>] [--app-info <app_info.yaml>] \
[--model <名称>] [--effort <强度>]";
const CONTRACT_FIXTURE: &str = include_str!("../../tests/fixtures/scriptgen/ui_contract.json");

#[derive(Deserialize)]
struct Judgement {
    #[serde(alias = "result")]
    final_result: FinalResult,
    items: Vec<JudgeItem>,
}

#[derive(Deserialize)]
struct AppInfo {
    ui_map: Value,
}

fn read_json<T: DeserializeOwned>(path: &Path, label: &str) -> Result<T, Box<dyn Error>> {
    let file = File::open(path).map_err(|err| format!("无法读取{label}：{err}"))?;
    serde_json::from_reader(file).map_err(|err| {
        format!(
            "{label} JSON 无效（第 {} 行，第 {} 列）",
            err.line(),
            err.column()
        )
        .into()
    })
}

async fn run(args: &[String]) -> Result<GeneratedScripts, Box<dyn Error>> {
    let [run, case_file, options @ ..] = args else {
        return Err(USAGE.into());
    };
    let run = Path::new(run);
    let mut judge_file = run.join("judge.json");
    let mut app_info = None;
    let mut contract_file = None;
    let mut opts = ScriptgenOpts {
        role: RoleModel {
            source: ModelSource::ClaudeCli,
            model: None,
            effort: None,
        },
        claude: ClaudeOpts::default(),
    };
    let mut options = options.chunks_exact(2);
    if !options.remainder().is_empty() {
        return Err(USAGE.into());
    }
    for pair in &mut options {
        let value = &pair[1];
        match pair[0].as_str() {
            "--fake-claude" => {
                opts.claude.bin = "/bin/sh".into();
                opts.claude.pre_args = vec![
                    std::path::absolute(value)?
                        .to_str()
                        .ok_or("假 Claude 路径必须是 UTF-8")?
                        .into(),
                ];
            }
            "--contract" => contract_file = Some(PathBuf::from(value)),
            "--judge" => judge_file = PathBuf::from(value),
            "--app-info" => app_info = Some(PathBuf::from(value)),
            "--model" => opts.role.model = Some(value.clone()),
            "--effort" => opts.role.effort = Some(value.clone()),
            _ => return Err(USAGE.into()),
        }
    }
    let case: CaseSpec = read_json(Path::new(case_file), "用例")?;
    let trace: ScriptTrace = read_json(&run.join("trace.json"), "轨迹")?;
    let judge: Judgement = read_json(&judge_file, "Judge 判定")?;
    let ui_map = if let Some(file) = app_info {
        let file = File::open(file).map_err(|err| format!("无法读取应用知识：{err}"))?;
        serde_yaml_ng::from_reader::<_, AppInfo>(file)
            .map_err(|_| "无法解析应用知识中的 ui_map")?
            .ui_map
    } else {
        Value::Null
    };
    let signatures = UiContract::from_json(CONTRACT_FIXTURE.as_bytes())
        .map_err(|_| "Ui 签名夹具 JSON 格式无效")?;
    let contract = if let Some(file) = contract_file {
        let bytes = fs::read(file).map_err(|err| format!("无法读取 Ui 契约：{err}"))?;
        UiContract::from_json(bytes.as_slice())
            .or_else(|_| UiContract::from_runtime_json(bytes.as_slice(), &signatures))
            .map_err(|_| "Ui 契约 JSON 格式无效，或运行时清单与签名夹具不符")?
    } else {
        signatures
    };
    Ok(generate(
        &opts,
        ScriptgenInput {
            case: &case,
            trace: &trace,
            judge_result: judge.final_result,
            judge_items: &judge.items,
            ui_map: &ui_map,
            contract: &contract,
            redactions: &[],
            previous_attempt: None,
        },
    )
    .await?)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if matches!(args.as_slice(), [help] if help == "--help" || help == "-h") {
        println!("{USAGE}");
        return std::process::ExitCode::SUCCESS;
    }
    let result = match run(&args).await {
        Ok(scripts) => {
            let mut out = io::stdout().lock();
            serde_json::to_writer_pretty(&mut out, &scripts)
                .map_err(io::Error::other)
                .and_then(|_| writeln!(out))
                .map_err(Box::<dyn Error>::from)
        }
        Err(err) => Err(err),
    };
    if let Err(err) = result {
        eprintln!("{err}");
        if let Some(ScriptgenError::Violations(violations)) = err.downcast_ref() {
            for violation in violations {
                eprintln!(
                    "{} 第 {} 行，规则 {}：{}",
                    violation.file, violation.line, violation.rule, violation.message
                );
            }
        }
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

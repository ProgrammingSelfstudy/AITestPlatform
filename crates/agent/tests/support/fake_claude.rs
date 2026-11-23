use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent::claude_cli::ClaudeOpts;

pub struct FakeClaude {
    pub dir: PathBuf,
    #[allow(dead_code)]
    pub run_dir: PathBuf,
    pub opts: ClaudeOpts,
}

impl FakeClaude {
    pub fn new(script: &str) -> Self {
        Self::with_login(script, r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"pro"}"#)
    }

    /// `login` 是 `claude auth status --json` 的假输出：调模型之前的会员登录检查会读它。
    pub fn with_login(script: &str, login: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "hafa-claude-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let run_dir = dir.join("run");
        fs::create_dir_all(run_dir.join("shots")).unwrap();
        let file = dir.join("claude.sh");
        fs::write(
            &file,
            format!(
                "set -eu\ncase \"$*\" in *\"auth status\"*) printf '%s\\n' '{login}'; exit 0;; esac\ncat > \"$0.prompt\"\nprintf '%s\\n' \"$@\" > \"$0.args\"\n{script}\n"
            ),
        )
        .unwrap();
        let opts = ClaudeOpts {
            bin: "/bin/sh".into(),
            pre_args: vec![file.to_string_lossy().into()],
            cwd: Some(dir.clone()),
            stall_timeout: Duration::from_secs(5),
            total_timeout: Duration::from_secs(10),
            ..Default::default()
        };
        Self { dir, run_dir, opts }
    }
}

impl Drop for FakeClaude {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

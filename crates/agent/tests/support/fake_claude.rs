use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent::claude_cli::ClaudeOpts;

pub struct FakeClaude {
    pub dir: PathBuf,
    pub run_dir: PathBuf,
    pub opts: ClaudeOpts,
}

impl FakeClaude {
    pub fn new(script: &str) -> Self {
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
                "set -eu\ncat > \"$0.prompt\"\nprintf '%s\\n' \"$@\" > \"$0.args\"\n{script}\n"
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

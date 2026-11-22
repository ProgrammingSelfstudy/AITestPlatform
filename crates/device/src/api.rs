//! 设备抽象：护栏、setup、执行服务只依赖这个 trait，所以都能用假设备测试，不需要真机。

use std::time::Duration;

use async_trait::async_trait;

use crate::{is_asleep, keyguard_showing, Android, DeviceError, Element, Result};

fn unsupported<T>(what: &str) -> Result<T> {
    Err(DeviceError::Unsupported(format!("这个设备实现不支持 {what}")))
}

#[async_trait]
pub trait DeviceApi: Send + Sync {
    async fn dump(&self) -> Result<Vec<Element>>;
    async fn screenshot(&self) -> Result<Vec<u8>>;
    async fn tap(&self, x: i32, y: i32) -> Result<()>;
    async fn swipe(&self, x1: i32, y1: i32, x2: i32, y2: i32, ms: u32) -> Result<()>;
    async fn back(&self) -> Result<()>;
    async fn launch_app(&self, package: &str, activity: &str) -> Result<()>;
    async fn stop_app(&self, package: &str) -> Result<()>;
    /// 屏幕宽高（像素）
    async fn screen_size(&self) -> Result<(i32, i32)>;

    // ---- 下面这些 setup 才用到，有默认实现（不支持），假设备按需覆盖 ----

    /// 在设备上执行一条 shell 命令，返回输出。
    async fn shell(&self, _cmd: &str) -> Result<String> {
        unsupported("shell")
    }

    /// 往聚焦的输入框输入文字。
    async fn input_text(&self, _text: &str, _clear: bool) -> Result<()> {
        unsupported("输入文字")
    }

    /// 把本机文件推到设备上。
    async fn push_file(&self, _local: &std::path::Path, _remote: &str) -> Result<()> {
        unsupported("推送文件")
    }

    /// 唤醒并解锁，并设置 USB 连接时保持常亮（长时间批量执行中途锁屏会让所有操作落空；锁屏时截图还是黑的）。
    /// 返回屏幕是否已解锁。`wm dismiss-keyguard` 只对滑动解锁有效：设了 PIN / 图案的手机解不开，
    /// 这时返回 `false`，调用方必须如实报告「屏幕仍锁定，需要人工」，不能假装成功。
    async fn ensure_awake(&self) -> Result<bool> {
        self.shell("svc power stayon usb").await?;
        let power = self.shell("dumpsys power").await?;
        let win = self.shell("dumpsys window").await?;
        if is_asleep(&power) || keyguard_showing(&win) {
            self.shell("input keyevent KEYCODE_WAKEUP").await?;
            tokio::time::sleep(Duration::from_secs(1)).await;
            self.shell("wm dismiss-keyguard").await?;
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        Ok(!keyguard_showing(&self.shell("dumpsys window").await?))
    }
}

#[async_trait]
impl DeviceApi for Android {
    async fn dump(&self) -> Result<Vec<Element>> {
        Android::dump(self).await
    }
    async fn screenshot(&self) -> Result<Vec<u8>> {
        self.screenshot_png().await
    }
    async fn tap(&self, x: i32, y: i32) -> Result<()> {
        Android::tap(self, x, y).await
    }
    async fn swipe(&self, x1: i32, y1: i32, x2: i32, y2: i32, ms: u32) -> Result<()> {
        Android::swipe(self, x1, y1, x2, y2, ms).await
    }
    async fn back(&self) -> Result<()> {
        Android::back(self).await
    }
    async fn launch_app(&self, package: &str, activity: &str) -> Result<()> {
        Android::launch_app(self, package, activity).await
    }
    async fn stop_app(&self, package: &str) -> Result<()> {
        Android::stop_app(self, package).await
    }
    async fn screen_size(&self) -> Result<(i32, i32)> {
        let i = self.info().await?;
        match (i.screen_w, i.screen_h) {
            (Some(w), Some(h)) => Ok((w as i32, h as i32)),
            _ => Err(DeviceError::Command { cmd: "wm size".into(), code: None, stderr: "读不到屏幕尺寸".into() }),
        }
    }
    async fn shell(&self, cmd: &str) -> Result<String> {
        Android::shell(self, cmd).await
    }
    async fn input_text(&self, text: &str, clear: bool) -> Result<()> {
        self.type_text(text, clear).await
    }
    async fn push_file(&self, local: &std::path::Path, remote: &str) -> Result<()> {
        Android::push_file(self, local, remote).await
    }
}

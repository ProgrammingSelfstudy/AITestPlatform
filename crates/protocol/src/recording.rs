//! 节点与服务端共用的录屏文件名和索引格式。

use serde::{Deserialize, Serialize};

pub const MAX_RECORDING_SIZE: u64 = 150 * 1024 * 1024;
pub const MAX_RECORDING_INDEX_SIZE: usize = 256 * 1024;

#[derive(Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct RecordingIndex {
    pub segments: Vec<RecordingSegment>,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct RecordingSegment {
    pub seq: i32,
    pub file: String,
    pub start_epoch_ms: i64,
    pub duration_ms: i64,
}

impl RecordingSegment {
    pub fn is_valid(&self) -> bool {
        recording_seq(&self.file) == Some(self.seq)
            && self.start_epoch_ms > 0
            && self.duration_ms > 0
            && self.start_epoch_ms.checked_add(self.duration_ms).is_some()
    }
}

pub fn recording_name(seq: i32) -> String {
    format!("rec_{seq:02}.mp4")
}

pub fn recording_seq(name: &str) -> Option<i32> {
    let seq = name
        .strip_prefix("rec_")?
        .strip_suffix(".mp4")?
        .parse()
        .ok()?;
    (seq > 0 && name == recording_name(seq)).then_some(seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 录屏文件名使用正整数且至少补齐两位() {
        for (file, seq) in [("rec_01.mp4", 1), ("rec_99.mp4", 99), ("rec_100.mp4", 100)] {
            assert_eq!(recording_seq(file), Some(seq));
        }
        for file in [
            "rec_00.mp4",
            "rec_1.mp4",
            "rec_001.mp4",
            "rec_-1.mp4",
            "rec_+1.mp4",
            "rec_01.MP4",
            "../rec_01.mp4",
        ] {
            assert_eq!(recording_seq(file), None);
        }
    }
}

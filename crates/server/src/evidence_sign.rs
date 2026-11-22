//! 进程内证据 URL 签名。

use std::sync::LazyLock;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

pub(crate) const TTL_SECS: i64 = 10 * 60;

pub(crate) struct Signer {
    key: [u8; 32],
    /// 独立生成的公开标识，供页面识别服务重启后的新签名。
    pub(crate) id: Uuid,
}

static PROCESS_SIGNER: LazyLock<Signer> = LazyLock::new(|| {
    let mut key = [0_u8; 32];
    key[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    key[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    Signer {
        key,
        id: Uuid::new_v4(),
    }
});

pub(crate) fn process_signer() -> &'static Signer {
    LazyLock::force(&PROCESS_SIGNER)
}

impl Signer {
    fn sign(&self, run_id: Uuid, name: &str, exp: i64) -> [u8; 32] {
        // UUID 和时间戳使用定长编码，余下字节为原始文件名。
        hmac_sha256(
            &self.key,
            &[run_id.as_bytes(), &exp.to_be_bytes(), name.as_bytes()],
        )
    }

    /// 文件名由调用方按证据文件的白名单筛选。
    pub(crate) fn url(&self, run_id: Uuid, name: &str, exp: i64) -> String {
        let sig = hex::encode(self.sign(run_id, name, exp));
        format!("/api/runs/{run_id}/files/{name}?exp={exp}&sig={sig}")
    }

    pub(crate) fn verify(&self, run_id: Uuid, name: &str, exp: i64, sig: &str, now: i64) -> bool {
        let mut provided = [0_u8; 32];
        if exp <= now || hex::decode_to_slice(sig, &mut provided).is_err() {
            return false;
        }
        bool::from(self.sign(run_id, name, exp).ct_eq(&provided))
    }
}

fn hmac_sha256(key: &[u8; 32], parts: &[&[u8]]) -> [u8; 32] {
    // 密钥固定为 32 字节，按 HMAC 规则补零至 SHA-256 的 64 字节分组。
    let mut ipad = [0x36_u8; 64];
    let mut opad = [0x5c_u8; 64];
    for (index, byte) in key.iter().enumerate() {
        ipad[index] ^= byte;
        opad[index] ^= byte;
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    for part in parts {
        inner.update(part);
    }
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner.finalize());
    outer.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SIGNER: Signer = Signer {
        key: [0x42; 32],
        id: Uuid::nil(),
    };
    const RUN: Uuid = Uuid::from_u128(1);
    const NAME: &str = "01_after.png";
    const NOW: i64 = 1_700_000_000;
    const EXP: i64 = NOW + TTL_SECS;

    #[test]
    fn hmac_sha256_匹配标准向量() {
        // RFC 4231 用例 1 的 20 字节密钥补零后得到相同的 HMAC。
        let mut key = [0_u8; 32];
        key[..20].fill(0x0b);
        assert_eq!(
            hex::encode(hmac_sha256(&key, &[b"Hi ", b"There"])),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn 有效签名通过且到期即失效() {
        let sig = hex::encode(TEST_SIGNER.sign(RUN, NAME, EXP));
        assert!(TEST_SIGNER.verify(RUN, NAME, EXP, &sig, NOW));
        assert!(TEST_SIGNER.verify(RUN, NAME, EXP, &sig, EXP - 1));
        assert!(!TEST_SIGNER.verify(RUN, NAME, EXP, &sig, EXP));
        assert!(!TEST_SIGNER.verify(RUN, NAME, EXP, &sig, EXP + 1));
    }

    #[test]
    fn 篡改文件名执行标识或到期秒数会失败() {
        let sig = hex::encode(TEST_SIGNER.sign(RUN, NAME, EXP));
        assert!(!TEST_SIGNER.verify(RUN, "02_after.png", EXP, &sig, NOW));
        assert!(!TEST_SIGNER.verify(Uuid::from_u128(2), NAME, EXP, &sig, NOW));
        assert!(!TEST_SIGNER.verify(RUN, NAME, EXP + 1, &sig, NOW));
    }

    #[test]
    fn 丢弃格式错误或使用其他密钥的签名() {
        let other = Signer {
            key: [0x24; 32],
            ..TEST_SIGNER
        };
        let sig = hex::encode(other.sign(RUN, NAME, EXP));
        assert!(!TEST_SIGNER.verify(RUN, NAME, EXP, &sig, NOW));
        assert!(!TEST_SIGNER.verify(RUN, NAME, EXP, "1234", NOW));
        assert!(!TEST_SIGNER.verify(RUN, NAME, EXP, &"zz".repeat(32), NOW));
    }
}

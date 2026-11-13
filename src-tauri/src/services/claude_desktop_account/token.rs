//! Claude Desktop 的 OAuth 令牌：解密、续期、查账号资料和额度。
//!
//! Desktop 把令牌放在 `config.json` 的 `oauth:tokenCacheV2`（档案里是 `config-tokenCacheV2`
//! 文件），用 Electron safeStorage（Chromium OSCrypt）加密：钥匙串里 `Claude Safe Storage`
//! 存着一串随机口令，PBKDF2-HMAC-SHA1（盐 `saltysalt`、1003 轮）派生出 16 字节 AES 密钥，
//! 每个值是 `v10` + AES-128-CBC（IV 为 16 个空格、PKCS7 填充）密文，整体 base64。
//! 解开后是个 JSON 对象，键形如 `<clientId>:<org>:<aud>:<scopes>`；只有带 `user:inference`
//! 的那条能查用量。它和 Claude Code 用同一个公开 OAuth client，所以用量、续期接口都通用。
//!
//! 续期会轮换 refresh token、让旧的作废，所以：
//! - 当前登录的账号永远只读（令牌归运行中的 Desktop 维护）；
//! - 其他账号的档案在切换锁里续期并写回档案，切换不会装上续期到一半的令牌。
//!
//! 做法参考 ai-usagebar（<https://github.com/akitaonrails/ai-usagebar>，MIT）。

use std::path::PathBuf;
use std::sync::Mutex;

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::Value;

use crate::error::AppError;

/// 钥匙串里存 safeStorage 口令的条目。
pub const SAFE_STORAGE_SERVICE: &str = "Claude Safe Storage";
const SALT: &[u8] = b"saltysalt";
const ROUNDS: u32 = 1003;
const IV: [u8; 16] = [b' '; 16];
const PREFIX: &[u8] = b"v10";
const PREFIX_V11: &[u8] = b"v11";

const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const BETA_HEADER: &str = "oauth-2025-04-20";
const INFERENCE_SCOPE: &str = "user:inference";
/// 离过期不到这么久就先续期。
pub const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;

/// 配置里 / 档案里放令牌的位置，新格式在前。
const CONFIG_KEYS: [&str; 2] = ["oauth:tokenCacheV2", "oauth:tokenCache"];
const PROFILE_FILES: [&str; 2] = [super::TOKEN_CACHE_V2, super::TOKEN_CACHE];

fn err(message: impl Into<String>) -> AppError {
    AppError::Message(message.into())
}

// ───────────────────────────── safeStorage ─────────────────────────────

pub fn derive_key(secret: &[u8]) -> [u8; 16] {
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(secret, SALT, ROUNDS, &mut key);
    key
}

/// 解开一个 safeStorage 值（base64 的 `v10…`）。
pub fn decrypt(key: &[u8; 16], value: &str) -> Result<Vec<u8>, AppError> {
    let raw = STANDARD
        .decode(value.trim())
        .map_err(|_| err("Claude Desktop 令牌不是 base64"))?;
    let cipher_text = raw
        .strip_prefix(PREFIX)
        .or_else(|| raw.strip_prefix(PREFIX_V11))
        .ok_or_else(|| err("Claude Desktop 令牌缺少 v10 前缀"))?;
    if cipher_text.is_empty() || cipher_text.len() % 16 != 0 {
        return Err(err("Claude Desktop 令牌长度不对"));
    }
    let cipher = aes::Aes128::new(key.into());
    let mut previous = IV;
    let mut plain = Vec::with_capacity(cipher_text.len());
    for chunk in cipher_text.chunks_exact(16) {
        let mut block = aes::Block::clone_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        for (byte, prev) in block.iter_mut().zip(previous.iter()) {
            *byte ^= prev;
        }
        plain.extend_from_slice(&block);
        previous.copy_from_slice(chunk);
    }
    let pad = *plain.last().unwrap_or(&0) as usize;
    if pad == 0
        || pad > 16
        || plain[plain.len() - pad..]
            .iter()
            .any(|b| *b as usize != pad)
    {
        return Err(err("Claude Desktop 令牌解密失败（钥匙串里的口令不对？）"));
    }
    plain.truncate(plain.len() - pad);
    Ok(plain)
}

/// 加密成 Desktop 能读的 safeStorage 值（固定 IV，与 Desktop 写的格式一致）。
pub fn encrypt(key: &[u8; 16], plain: &[u8]) -> String {
    let cipher = aes::Aes128::new(key.into());
    let pad = 16 - plain.len() % 16;
    let mut data = plain.to_vec();
    data.extend(std::iter::repeat_n(pad as u8, pad));
    let mut previous = IV;
    let mut out = PREFIX.to_vec();
    for chunk in data.chunks_exact(16) {
        let mut block = aes::Block::clone_from_slice(chunk);
        for (byte, prev) in block.iter_mut().zip(previous.iter()) {
            *byte ^= prev;
        }
        cipher.encrypt_block(&mut block);
        previous.copy_from_slice(&block);
        out.extend_from_slice(&block);
    }
    STANDARD.encode(out)
}

static KEY_CACHE: Mutex<Option<[u8; 16]>> = Mutex::new(None);

/// 读钥匙串里的 safeStorage 口令。第一次会弹系统授权框（选「始终允许」之后就不再问），
/// 读到后在内存里留一份，免得每次查额度都去碰钥匙串。
pub fn safe_storage_key() -> Result<[u8; 16], AppError> {
    if let Some(key) = KEY_CACHE.lock().ok().and_then(|cache| *cache) {
        return Ok(key);
    }
    let key = read_keychain_key()?;
    if let Ok(mut cache) = KEY_CACHE.lock() {
        *cache = Some(key);
    }
    Ok(key)
}

#[cfg(target_os = "macos")]
fn read_keychain_key() -> Result<[u8; 16], AppError> {
    let output = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", SAFE_STORAGE_SERVICE, "-w"])
        .output()
        .map_err(|e| err(format!("无法运行 security: {e}")))?;
    if !output.status.success() {
        return Err(err(format!(
            "读不到钥匙串里的「{SAFE_STORAGE_SERVICE}」：请在系统弹窗里允许 CC Switch 访问"
        )));
    }
    let secret = String::from_utf8_lossy(&output.stdout);
    Ok(derive_key(secret.trim().as_bytes()))
}

#[cfg(not(target_os = "macos"))]
fn read_keychain_key() -> Result<[u8; 16], AppError> {
    Err(err("Claude Desktop 账号额度目前只支持 macOS"))
}

// ───────────────────────────── 令牌 ─────────────────────────────

/// 令牌存放的位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    /// Desktop 的 `config.json`：当前登录的账号，只读。
    Live(PathBuf),
    /// 档案目录：其他账号，可以续期写回。
    Profile(PathBuf),
}

/// 能查用量的那条令牌。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// 毫秒；读不到时为 `None`
    pub expires_at: Option<i64>,
    /// 解开后的 JSON 里这条的键
    entry_key: String,
    /// 取自哪个键（config.json）或哪个文件（档案）
    slot: String,
}

impl DesktopToken {
    pub fn needs_refresh(&self, now_ms: i64) -> bool {
        self.expires_at
            .is_some_and(|expires| expires - REFRESH_MARGIN_MS <= now_ms)
    }
}

fn read_slot(source: &TokenSource) -> Result<(String, String), AppError> {
    match source {
        TokenSource::Live(config) => {
            let bytes = std::fs::read(config).map_err(|e| AppError::io(config, e))?;
            let value: Value =
                serde_json::from_slice(&bytes).map_err(|e| AppError::json(config, e))?;
            CONFIG_KEYS
                .iter()
                .find_map(|key| {
                    value
                        .get(*key)
                        .and_then(Value::as_str)
                        .filter(|blob| !blob.is_empty())
                        .map(|blob| (key.to_string(), blob.to_string()))
                })
                .ok_or_else(|| err("Claude Desktop 当前没有登录"))
        }
        TokenSource::Profile(dir) => PROFILE_FILES
            .iter()
            .find_map(|file| {
                std::fs::read_to_string(dir.join(file))
                    .ok()
                    .filter(|blob| !blob.trim().is_empty())
                    .map(|blob| (file.to_string(), blob))
            })
            .ok_or_else(|| err("这个账号没有保存的登录凭据")),
    }
}

/// 从解开的令牌表里挑出能查用量的那条。
fn pick_entry(plain: &[u8]) -> Result<(String, Value), AppError> {
    let value: Value =
        serde_json::from_slice(plain).map_err(|_| err("Claude Desktop 令牌内容不是 JSON"))?;
    let map = value
        .as_object()
        .ok_or_else(|| err("Claude Desktop 令牌内容不是 JSON 对象"))?;
    map.iter()
        .find(|(key, _)| key.contains(INFERENCE_SCOPE))
        .map(|(key, entry)| (key.clone(), entry.clone()))
        .ok_or_else(|| err("Claude Desktop 令牌里没有可查用量的那一条"))
}

pub fn read_token(key: &[u8; 16], source: &TokenSource) -> Result<DesktopToken, AppError> {
    let (slot, blob) = read_slot(source)?;
    let (entry_key, entry) = pick_entry(&decrypt(key, &blob)?)?;
    let text = |name: &str| {
        entry
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let access_token = text("token").ok_or_else(|| err("Claude Desktop 令牌里没有 token"))?;
    Ok(DesktopToken {
        access_token,
        refresh_token: text("refreshToken"),
        expires_at: entry.get("expiresAt").and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_f64().map(|number| number as i64))
        }),
        entry_key,
        slot,
    })
}

/// 把续期后的令牌写回档案（只改这一条的三个字段，其余原样）。当前登录的账号不写。
pub fn write_back(
    key: &[u8; 16],
    source: &TokenSource,
    token: &DesktopToken,
) -> Result<(), AppError> {
    let TokenSource::Profile(dir) = source else {
        return Err(err("当前登录的账号由 Claude Desktop 自己续期"));
    };
    let path = dir.join(&token.slot);
    let blob = std::fs::read_to_string(&path).map_err(|e| AppError::io(&path, e))?;
    let mut value: Value = serde_json::from_slice(&decrypt(key, &blob)?)
        .map_err(|_| err("Claude Desktop 令牌内容不是 JSON"))?;
    let entry = value
        .get_mut(&token.entry_key)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| err("续期写回时找不到原来那条令牌"))?;
    entry.insert("token".into(), token.access_token.clone().into());
    if let Some(refresh) = &token.refresh_token {
        entry.insert("refreshToken".into(), refresh.clone().into());
    }
    if let Some(expires) = token.expires_at {
        entry.insert("expiresAt".into(), expires.into());
    }
    let plain = serde_json::to_vec(&value).map_err(|source| AppError::JsonSerialize { source })?;
    crate::config::atomic_write_private(&path, encrypt(key, &plain).as_bytes())
}

/// 用 refresh token 续期，返回新令牌（refresh token 也会轮换）。
pub async fn refresh(token: &DesktopToken, now_ms: i64) -> Result<DesktopToken, String> {
    let refresh_token = token
        .refresh_token
        .as_deref()
        .ok_or("这个账号的登录没有 refresh token，请切换过去在 Claude Desktop 里重新登录")?;
    let response = crate::proxy::http_client::get()
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .header("anthropic-beta", BETA_HEADER)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": refresh_token,
        }))
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("续期 Claude Desktop 登录失败（网络）: {e}"))?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("续期 Claude Desktop 登录失败: {e}"))?;
    if !status.is_success() {
        // 只报状态码，不回显响应体（可能夹带令牌）
        return Err(format!(
            "续期 Claude Desktop 登录失败（HTTP {status}），请切换到这个账号后在 Claude Desktop 里重新登录"
        ));
    }
    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or("续期 Claude Desktop 登录失败：响应里没有 access_token")?;
    let expires_in = body.get("expires_in").and_then(Value::as_i64);
    Ok(DesktopToken {
        access_token: access_token.to_string(),
        refresh_token: body
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| token.refresh_token.clone()),
        expires_at: expires_in.map(|seconds| now_ms + seconds * 1000),
        entry_key: token.entry_key.clone(),
        slot: token.slot.clone(),
    })
}

// ───────────────────────────── 账号资料 ─────────────────────────────

/// `/api/oauth/profile` 里用得上的几项。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountInfo {
    pub email: Option<String>,
    pub org_name: Option<String>,
    /// `pro` / `max` / `team` / `enterprise`
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
}

pub fn parse_account_info(body: &Value) -> AccountInfo {
    let text = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let account = body.get("account");
    let org = body.get("organization");
    let subscription_type = text(org.and_then(|o| o.get("organization_type"))).map(|kind| {
        kind.strip_prefix("claude_")
            .unwrap_or(&kind)
            .to_ascii_lowercase()
    });
    AccountInfo {
        email: text(account.and_then(|a| a.get("email")))
            .or_else(|| text(account.and_then(|a| a.get("email_address")))),
        org_name: text(org.and_then(|o| o.get("name"))),
        subscription_type,
        rate_limit_tier: text(org.and_then(|o| o.get("rate_limit_tier"))),
    }
}

pub async fn fetch_account_info(access_token: &str) -> Result<AccountInfo, String> {
    let response = crate::proxy::http_client::get()
        .get(PROFILE_URL)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", BETA_HEADER)
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("查询 Claude 账号资料失败: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("查询 Claude 账号资料失败（HTTP {status}）"));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("查询 Claude 账号资料失败: {e}"))?;
    Ok(parse_account_info(&body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn key() -> [u8; 16] {
        derive_key(b"not-a-real-secret")
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = key();
        for message in [&b""[..], b"x", b"exactly16bytes!!", br#"{"a":"b"}"#] {
            let blob = encrypt(&key, message);
            assert_eq!(decrypt(&key, &blob).unwrap(), message);
        }
        assert!(decrypt(&derive_key(b"other"), &encrypt(&key, b"hello world")).is_err());
        assert!(decrypt(&key, "not base64!").is_err());
    }

    /// 参考值由 Python `hashlib.pbkdf2_hmac` 和系统 `openssl enc -aes-128-cbc` 独立算出：
    /// 口令 `peanuts` 派生的密钥、以及用它加密 "hello" 的结果。逐字节一致 Desktop 才读得回。
    #[test]
    fn matches_independent_reference() {
        let key = derive_key(b"peanuts");
        assert_eq!(
            key,
            [
                0xd9, 0xa0, 0x9d, 0x49, 0x9b, 0x4e, 0x1b, 0x74, 0x61, 0xf2, 0x8e, 0x67, 0x97, 0x2c,
                0x6d, 0xbd
            ]
        );
        assert_eq!(encrypt(&key, b"hello"), "djEwhpCnyzx4cdGhYCRGnONbQw==");
        assert_eq!(
            decrypt(&key, "djEwhpCnyzx4cdGhYCRGnONbQw==").unwrap(),
            b"hello"
        );
    }

    fn write_profile(dir: &Path, key: &[u8; 16], entries: Value) {
        std::fs::create_dir_all(dir).unwrap();
        let blob = encrypt(key, &serde_json::to_vec(&entries).unwrap());
        std::fs::write(dir.join(super::super::TOKEN_CACHE_V2), blob).unwrap();
    }

    #[test]
    fn reads_inference_entry_and_writes_back() {
        let dir = tempfile::tempdir().unwrap();
        let key = key();
        write_profile(
            dir.path(),
            &key,
            json!({
                "cid:org:aud:user:profile": {"token": "profile-only"},
                "cid:org:aud:user:inference user:profile": {
                    "token": "old", "refreshToken": "r1", "expiresAt": 1000, "other": 1
                }
            }),
        );
        let source = TokenSource::Profile(dir.path().to_path_buf());
        let token = read_token(&key, &source).unwrap();
        assert_eq!(token.access_token, "old");
        assert_eq!(token.refresh_token.as_deref(), Some("r1"));
        assert!(token.needs_refresh(0));

        let renewed = DesktopToken {
            access_token: "new".into(),
            refresh_token: Some("r2".into()),
            expires_at: Some(10_000_000),
            ..token
        };
        write_back(&key, &source, &renewed).unwrap();
        let again = read_token(&key, &source).unwrap();
        assert_eq!(again.access_token, "new");
        assert_eq!(again.refresh_token.as_deref(), Some("r2"));
        // 其他条目和字段原样
        let blob = std::fs::read_to_string(dir.path().join(super::super::TOKEN_CACHE_V2)).unwrap();
        let all: Value = serde_json::from_slice(&decrypt(&key, &blob).unwrap()).unwrap();
        assert_eq!(
            all["cid:org:aud:user:profile"]["token"],
            json!("profile-only")
        );
        assert_eq!(
            all["cid:org:aud:user:inference user:profile"]["other"],
            json!(1)
        );
    }

    #[test]
    fn live_source_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let key = key();
        let entries = json!({"c:o:a:user:inference": {"token": "live"}});
        let config = dir.path().join("config.json");
        std::fs::write(
            &config,
            serde_json::to_vec(&json!({
                "oauth:tokenCacheV2": encrypt(&key, &serde_json::to_vec(&entries).unwrap())
            }))
            .unwrap(),
        )
        .unwrap();
        let source = TokenSource::Live(config);
        let token = read_token(&key, &source).unwrap();
        assert_eq!(token.access_token, "live");
        assert!(!token.needs_refresh(i64::MAX));
        assert!(write_back(&key, &source, &token).is_err());
    }

    #[test]
    fn parses_account_info() {
        let info = parse_account_info(&json!({
            "account": {"email": "me@example.com"},
            "organization": {"name": "Me", "organization_type": "claude_max", "rate_limit_tier": "default_claude_max_20x"}
        }));
        assert_eq!(info.email.as_deref(), Some("me@example.com"));
        assert_eq!(info.subscription_type.as_deref(), Some("max"));
        assert_eq!(
            info.rate_limit_tier.as_deref(),
            Some("default_claude_max_20x")
        );
        let fallback = parse_account_info(&json!({"account": {"email_address": "x@y"}}));
        assert_eq!(fallback.email.as_deref(), Some("x@y"));
        assert_eq!(fallback.subscription_type, None);
    }
}

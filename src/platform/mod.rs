mod library;
#[allow(unused_imports)] // Read-only examples include this module without Tauri command types.
pub use library::{Collection, CollectionPage, EntityDetail, LibrarySummary, TrackPage};
mod discovery;
mod search;
#[allow(unused_imports)] // Standalone probes do not use the Tauri command return type.
pub use discovery::PlaylistCategory;

use crate::model::{AppResult, Lyrics, Track};
use base64::Engine;
use ncm_api_rs::{
    request::{ApiClient, ApiResponse},
    Query,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{future::Future, sync::Mutex};

pub struct Netease {
    client: ApiClient,
    music_client: ApiClient,
    cookie: Mutex<Option<String>>,
    domain: Option<String>,
    pub(crate) accounts:
        Option<std::sync::Arc<dyn nons_music_adapter_sdk::account::ProviderAccounts>>,
    frozen: bool,
    session_generation: std::sync::atomic::AtomicU64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QrLogin {
    pub key: String,
    pub image: String,
}

#[derive(Serialize, Deserialize)]
pub struct LoginStatus {
    pub code: i64,
    pub message: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountProfile {
    pub user_id: u64,
    pub nickname: String,
    pub avatar_url: nons_music_adapter_sdk::cover::CoverImage,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SongInformation {
    pub artists: Vec<SongCredit>,
    pub album_id: Option<u64>,
    pub published_at: Option<u64>,
}
#[derive(Serialize, Deserialize)]
pub struct SongCredit {
    pub name: String,
    pub id: Option<u64>,
}

impl Netease {
    pub(crate) async fn invalidate_session(&self, expected_generation: u64) {
        if let Some(accounts) = &self.accounts {
            let expected = accounts.session();
            if expected.generation == expected_generation {
                let _ = accounts.invalidate(expected).await.inspect_err(|error| {
                    crate::diagnostics::warn("account.invalidate", error);
                });
            }
            return;
        }
        // A late unauthenticated response from an old account cannot revoke a new login.
        let _ = self.session_generation.compare_exchange(
            expected_generation,
            expected_generation.wrapping_add(1),
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        );
    }
    pub fn session_generation(&self) -> u64 {
        if let Some(accounts) = &self.accounts {
            return accounts.session().generation;
        }
        self.session_generation
            .load(std::sync::atomic::Ordering::Acquire)
    }
    pub fn account_credentials(&self) -> AppResult<Option<String>> {
        if !self.frozen {
            if let Some(accounts) = &self.accounts {
                return accounts
                    .credential()
                    .map_err(|e| {
                        crate::diagnostics::warn("account.credential.read", &e);
                        e.to_string()
                    })?
                    .map(|v| {
                        String::from_utf8(v.expose().to_vec()).map_err(|error| {
                            crate::diagnostics::warn("account.credential.decode", &error);
                            "网易云凭据格式无效".to_owned()
                        })
                    })
                    .transpose();
            }
        }
        self.cookie
            .lock()
            .map(|cookie| cookie.clone())
            .map_err(|error| {
                crate::diagnostics::warn("account.credential.lock", &error);
                "登录状态锁不可用".into()
            })
    }
    pub fn with_accounts(
        accounts: std::sync::Arc<dyn nons_music_adapter_sdk::account::ProviderAccounts>,
    ) -> Self {
        let client = ncm_api_rs::create_client(None);
        let music_client = client.clone();
        Self {
            client,
            music_client,
            cookie: Mutex::new(None),
            domain: None,
            accounts: Some(accounts),
            frozen: false,
            session_generation: Default::default(),
        }
    }
    pub(crate) fn scoped(
        &self,
        context: &nons_music_adapter_sdk::adapter::RequestContext,
    ) -> nons_music_adapter_sdk::MusicResult<Self> {
        if context.session.source.as_str() != "netease" {
            return Err(nons_music_adapter_sdk::ErrorCode::PermissionDenied.into());
        }
        let bytes = if let Some(accounts) = &self.accounts {
            accounts.read(context)?
        } else {
            self.account_credentials()
                .map_err(|_| nons_music_adapter_sdk::ErrorCode::Internal)?
                .map(|s| nons_music_adapter_sdk::account::OpaqueCredential::new(s.into_bytes()))
                .transpose()?
        };
        let cookie = bytes
            .map(|v| {
                String::from_utf8(v.expose().to_vec()).map_err(|error| {
                    crate::diagnostics::warn("account.scoped.decode", &error);
                    nons_music_adapter_sdk::ErrorCode::InvalidData
                })
            })
            .transpose()?;
        // Business endpoints can include full playlist metadata before pagination.
        // Retain the pinned client's bounded 16MiB budget; playback uses music_client.
        let client = self.client.clone();
        Ok(Self {
            client,
            music_client: self.music_client.clone(),
            cookie: Mutex::new(cookie),
            domain: self.domain.clone(),
            accounts: self.accounts.clone(),
            frozen: true,
            session_generation: std::sync::atomic::AtomicU64::new(context.session.generation),
        })
    }

    pub(crate) async fn song(&self, id: u64) -> nons_music_adapter_sdk::MusicResult<Track> {
        if id == 0 || id > 9_007_199_254_740_991 {
            return Err(nons_music_adapter_sdk::ErrorCode::InvalidData.into());
        }
        let body = music_checked(
            self.music_client.song_detail(
                &self
                    .query()
                    .map_err(|_| nons_music_adapter_sdk::ErrorCode::Internal)?
                    .param("ids", &id.to_string()),
            ),
        )
        .await?;
        body.pointer("/songs/0")
            .and_then(track_from_json)
            .ok_or_else(|| nons_music_adapter_sdk::ErrorCode::NotFound.into())
    }
    /// Probe-only constructor; the caller owns storage and authorization.
    pub fn with_cookie(cookie: Option<String>) -> Self {
        let client = ncm_api_rs::create_client(None);
        let music_client = client.clone();
        Self {
            music_client,
            client,
            cookie: Mutex::new(cookie),
            domain: None,
            accounts: None,
            frozen: false,
            session_generation: Default::default(),
        }
    }

    fn query(&self) -> AppResult<Query> {
        let mut query = Query::new();
        if let Some(domain) = &self.domain {
            query.domain = Some(domain.clone());
        }
        Ok(match self.account_credentials()? {
            Some(cookie) => query.cookie(&cookie),
            None => query,
        })
    }

    pub async fn search(&self, keyword: &str, offset: u32) -> AppResult<Vec<Track>> {
        if keyword.trim().is_empty() || keyword.len() > 256 {
            return Err("请输入不超过 256 字节的搜索词".into());
        }
        let query = self
            .query()?
            .param("keywords", keyword.trim())
            .param("type", "1")
            .param("limit", "50")
            .param("offset", &offset.to_string());
        let body = checked(self.client.cloudsearch(&query)).await?;
        let songs = body.pointer("/result/songs").and_then(Value::as_array);
        Ok(songs
            .into_iter()
            .flatten()
            .filter_map(track_from_json)
            .collect())
    }

    pub async fn liked_song_ids(&self) -> AppResult<Vec<u64>> {
        let profile = self.profile().await?.ok_or("请先登录网易云音乐")?;
        let query = self.query()?.param("uid", &profile.user_id.to_string());
        let body = checked(self.client.likelist(&query)).await?;
        let ids = body
            .get("ids")
            .and_then(Value::as_array)
            .ok_or("收藏列表响应缺少歌曲 ID")?;
        Ok(ids.iter().filter_map(Value::as_u64).collect())
    }

    pub async fn set_song_liked(&self, id: u64, liked: bool) -> AppResult<()> {
        if id == 0 {
            return Err("歌曲 ID 无效".into());
        }
        self.profile().await?.ok_or("请先登录网易云音乐")?;
        let query = self
            .query()?
            .param("id", &id.to_string())
            .param("like", if liked { "true" } else { "false" });
        checked(self.client.like(&query)).await?;
        Ok(())
    }

    pub async fn song_information(&self, id: u64) -> AppResult<SongInformation> {
        let query = self.query()?.param("ids", &id.to_string());
        let body = checked(self.client.song_detail(&query)).await?;
        let song = body.pointer("/songs/0").ok_or("歌曲详情缺失")?;
        let mut information = song_information_from_json(song);
        if information.published_at.is_none() {
            if let Some(album_id) = information.album_id {
                let query = self.query()?.param("id", &album_id.to_string());
                if let Ok(album) = checked(self.client.album(&query)).await {
                    information.published_at = album
                        .pointer("/album/publishTime")
                        .and_then(Value::as_u64)
                        .filter(|v| *v > 0);
                }
            }
        }
        Ok(information)
    }

    pub(crate) async fn resolve_resource(
        &self,
        id: u64,
        quality: &str,
        allow_downgrade: bool,
    ) -> nons_music_adapter_sdk::MusicResult<nons_music_adapter_sdk::resource::PlaybackResource>
    {
        use nons_music_adapter_sdk::{resource::*, ErrorCode};
        // The plain web transport avoids intermittent eapi risk-control rejection.
        let data = serde_json::json!({
            "ids": format!("[{id}]"),
            "level": quality,
            "encodeType": "flac"
        });
        let query = self
            .query()
            .map_err(|_| ErrorCode::Internal)?
            .param("uri", "/api/song/enhance/player/url/v1")
            .param("data", &data.to_string())
            .param("crypto", "api");
        let body = music_checked(self.music_client.api(&query)).await?;
        let item = body.pointer("/data/0").ok_or(ErrorCode::NotFound)?;
        if item.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(ErrorCode::InvalidData.into());
        }
        let uri = item
            .get("url")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or(ErrorCode::PermissionDenied)?;
        let actual = item
            .get("level")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .ok_or(ErrorCode::InvalidData)?
            .to_string();
        if !allow_downgrade && actual != quality {
            return Err(ErrorCode::PermissionDenied.into());
        }
        let uri = self.music_client.resolve_url(uri).await.map_err(|error| {
            crate::diagnostics::warn("music.redirect", &error);
            music_error_code(error)
        })?;
        let extent = match item.get("freeTrialInfo").filter(|v| !v.is_null()) {
            Some(trial) => PlaybackExtent::Preview {
                start_ms: trial
                    .get("start")
                    .and_then(Value::as_u64)
                    .ok_or(ErrorCode::InvalidData)?
                    .checked_mul(1000)
                    .ok_or(ErrorCode::InvalidData)?,
                end_ms: trial
                    .get("end")
                    .and_then(Value::as_u64)
                    .ok_or(ErrorCode::InvalidData)?
                    .checked_mul(1000)
                    .ok_or(ErrorCode::InvalidData)?,
            },
            None => PlaybackExtent::Full,
        };
        let expires_at_ms = item
            .get("expi")
            .and_then(Value::as_u64)
            .and_then(|seconds| seconds.checked_mul(1000))
            .and_then(|ttl| crate::now_ms().checked_add(ttl));
        Ok(PlaybackResource {
            metadata: ResourceMetadata {
                actual_quality: actual,
                extent,
                expires_at_ms,
            },
            access: ResourceAccess::Http(HttpAccess {
                url: uri,
                headers: vec![],
            }),
        })
    }

    pub async fn lyrics(&self, id: u64) -> AppResult<Option<Lyrics>> {
        let query = self.query()?.param("id", &id.to_string());
        let body = checked(self.client.lyric_new(&query)).await?;
        let word = body
            .pointer("/yrc/lyric")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty());
        let line = body
            .pointer("/lrc/lyric")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty());
        let Some(content) = word.or(line) else {
            return Ok(None);
        };
        if content.len() > crate::model::MAX_LYRIC_BYTES {
            return Err("网易云歌词过大".into());
        }
        Ok(Some(Lyrics {
            match_score: None,
            source: "netease".into(),
            format: if word.is_some() { "yrc" } else { "lrc" }.into(),
            content: content.into(),
            translation: body
                .pointer(if word.is_some() {
                    "/ytlrc/lyric"
                } else {
                    "/tlyric/lyric"
                })
                .and_then(Value::as_str)
                .map(str::to_owned),
            romanization: body
                .pointer(if word.is_some() {
                    "/yromalrc/lyric"
                } else {
                    "/romalrc/lyric"
                })
                .and_then(Value::as_str)
                .map(str::to_owned),
        }))
    }

    pub async fn qr_login(&self) -> AppResult<QrLogin> {
        let query = self.query()?;
        let body = checked(self.client.login_qr_key(&query)).await?;
        let key = qr_key(&body)?.to_string();
        let query = Query::new().param("key", &key);
        let body = checked(self.client.login_qr_create(&query)).await?;
        let url = body
            .pointer("/data/qrurl")
            .and_then(Value::as_str)
            .ok_or("二维码地址缺失")?;
        let qr = qrcode::QrCode::new(url).map_err(|error| {
            crate::diagnostics::warn("account.qr.encode", &error);
            "无法生成二维码图片"
        })?;
        let svg = qr
            .render::<qrcode::render::svg::Color>()
            .min_dimensions(224, 224)
            .build();
        let image = format!(
            "data:image/svg+xml;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(svg)
        );
        Ok(QrLogin { key, image })
    }

    pub(crate) async fn poll_completion(
        &self,
        key: &str,
    ) -> AppResult<(
        LoginStatus,
        Option<nons_music_adapter_sdk::account::OpaqueCredential>,
    )> {
        if key.is_empty() || key.len() > 256 {
            return Err("登录二维码已失效，请重新生成".into());
        }
        let query = Query::new().param("key", key);
        let response = timed(self.client.login_qr_check(&query)).await?;
        let code = response
            .body
            .get("code")
            .and_then(Value::as_i64)
            .unwrap_or(response.status);
        let mut completion = None;
        if code == 803 {
            let mut tokens = std::collections::BTreeMap::new();
            for cookie in &response.cookie {
                match cookie::Cookie::parse(cookie.as_str()) {
                    Ok(cookie) => {
                        if matches!(cookie.name(), "MUSIC_U" | "__csrf" | "MUSIC_A" | "NMTID") {
                            tokens.insert(cookie.name().to_string(), cookie.value().to_string());
                        }
                    }
                    Err(error) => crate::diagnostics::warn("account.cookie.parse", &error),
                }
            }
            if !tokens.contains_key("MUSIC_U") {
                return Err("登录返回缺少有效会话，请重新扫码".into());
            }
            let value = tokens
                .into_iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            completion = Some(
                nons_music_adapter_sdk::account::OpaqueCredential::new(value.into_bytes())
                    .map_err(|e| e.to_string())?,
            );
        }
        let message = match code {
            800 => "二维码已过期",
            801 => "等待扫码",
            802 => "请在网易云音乐中确认",
            803 => "登录成功",
            _ => "登录状态异常，请重试",
        };
        Ok((
            LoginStatus {
                code,
                message: message.into(),
            },
            completion,
        ))
    }

    pub async fn profile(&self) -> AppResult<Option<AccountProfile>> {
        if self.account_credentials()?.is_none() {
            return Ok(None);
        }
        let generation = self.session_generation();
        let query = self.query()?;
        let body = checked(self.client.login_status(&query)).await?;
        let profile = account_profile(&body);
        if profile.is_none() && !self.frozen {
            self.invalidate_session(generation).await;
        }
        Ok(profile)
    }

    pub(crate) async fn remote_logout(&self) -> nons_music_adapter_sdk::MusicResult<()> {
        music_checked(
            self.music_client.logout(
                &self
                    .query()
                    .map_err(|_| nons_music_adapter_sdk::ErrorCode::Internal)?,
            ),
        )
        .await?;
        Ok(())
    }
    pub(crate) async fn profile_with_credential(
        &self,
        credential: &nons_music_adapter_sdk::account::OpaqueCredential,
    ) -> AppResult<Option<AccountProfile>> {
        let cookie = std::str::from_utf8(credential.expose()).map_err(|error| {
            crate::diagnostics::warn("account.profile.decode", &error);
            "网易云凭据格式无效"
        })?;
        let body = checked(self.client.login_status(&Query::new().cookie(cookie))).await?;
        Ok(account_profile(&body))
    }
}

fn account_profile(body: &Value) -> Option<AccountProfile> {
    let profile = body
        .pointer("/data/profile")
        .or_else(|| body.get("profile"))?;
    let user_id = profile.get("userId")?.as_u64().filter(|id| *id > 0)?;
    Some(AccountProfile {
        user_id,
        nickname: profile
            .get("nickname")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .unwrap_or("网易云用户")
            .into(),
        avatar_url: crate::cover::image(
            profile
                .get("avatarUrl")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
    })
}

async fn timed<F: Future<Output = ncm_api_rs::error::Result<ApiResponse>>>(
    future: F,
) -> AppResult<ApiResponse> {
    future.await.map_err(|error| {
        crate::diagnostics::warn("login.request", &error);
        "网易云请求失败，请检查网络后重试".into()
    })
}

async fn checked<F: Future<Output = ncm_api_rs::error::Result<ApiResponse>>>(
    future: F,
) -> AppResult<Value> {
    music_checked(future)
        .await
        .map_err(|error| error.to_string())
}

fn qr_key(body: &Value) -> AppResult<&str> {
    body.get("unikey")
        .and_then(Value::as_str)
        .filter(|key| !key.trim().is_empty())
        .or_else(|| {
            body.pointer("/data/unikey")
                .and_then(Value::as_str)
                .filter(|key| !key.trim().is_empty())
        })
        .ok_or_else(|| "网易云二维码响应缺少有效登录 key，请重新生成".into())
}

fn song_information_from_json(song: &Value) -> SongInformation {
    SongInformation {
        artists: song
            .get("ar")
            .or_else(|| song.get("artists"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|artist| SongCredit {
                name: artist
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                id: artist.get("id").and_then(Value::as_u64).filter(|v| *v > 0),
            })
            .collect(),
        album_id: song
            .get("al")
            .or_else(|| song.get("album"))
            .and_then(|album| album.get("id"))
            .and_then(Value::as_u64)
            .filter(|v| *v > 0),
        published_at: song
            .get("publishTime")
            .and_then(Value::as_u64)
            .filter(|v| *v > 0),
    }
}

fn song_aliases(song: &Value) -> Vec<String> {
    let title = song.get("name").and_then(Value::as_str).unwrap_or_default();
    let mut names = Vec::new();
    for field in ["tns", "transNames", "alia", "alias"] {
        if let Some(values) = song.get(field).and_then(Value::as_array) {
            for value in values {
                if let Some(name) = value.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    if name != title && !names.iter().any(|existing| existing == name) {
                        names.push(name.to_owned());
                    }
                }
            }
        }
    }
    names
}

fn track_from_json(song: &Value) -> Option<Track> {
    let id = song.get("id")?.as_u64().filter(|id| *id > 0)?;
    let artists = song.get("ar").or_else(|| song.get("artists"))?.as_array()?;
    let artist = artists
        .iter()
        .filter_map(|v| v.get("name")?.as_str())
        .collect::<Vec<_>>()
        .join(" / ");
    let album = song.get("al").or_else(|| song.get("album"));
    use crate::identity::{EntityKind, LyricIdentifier, MusicCredit, MusicTrack};
    let reference = crate::reference(id).ok()?;
    Track::from_music(MusicTrack {
        reference: reference.clone(),
        title: song.get("name")?.as_str()?.into(),
        aliases: song_aliases(song),
        artists: artists
            .iter()
            .filter_map(|v| {
                Some(MusicCredit {
                    name: v.get("name")?.as_str()?.into(),
                    reference: v
                        .get("id")
                        .and_then(Value::as_u64)
                        .filter(|id| *id > 0)
                        .and_then(|id| crate::entity_reference(EntityKind::Artist, id).ok()),
                })
            })
            .collect(),
        album_reference: album
            .and_then(|v| v.get("id"))
            .and_then(Value::as_u64)
            .filter(|id| *id > 0)
            .and_then(|id| crate::entity_reference(EntityKind::Album, id).ok()),
        artist: if artist.is_empty() {
            "未知艺术家".into()
        } else {
            artist
        },
        album: album
            .and_then(|v| v.get("name"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("未知专辑")
            .into(),
        duration_ms: song
            .get("dt")
            .or_else(|| song.get("duration"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cover: crate::cover::image(
            album
                .and_then(|v| v.get("picUrl"))
                .and_then(Value::as_str)
                .unwrap_or(""),
        ),
        associations: vec![],
        lyric_identifiers: vec![LyricIdentifier {
            kind: "song-id".into(),
            id: reference.id,
        }],
        unavailable_reason: None,
    })
    .ok()
}

async fn music_checked<F: Future<Output = ncm_api_rs::error::Result<ApiResponse>>>(
    future: F,
) -> nons_music_adapter_sdk::MusicResult<Value> {
    use nons_music_adapter_sdk::ErrorCode;
    let response = future.await.map_err(music_error)?;
    let status = response
        .body
        .get("code")
        .and_then(Value::as_i64)
        .unwrap_or(response.status);
    if status != 200 {
        log::warn!(target: "nons_adapter_netease::recovery", "operation=music.response http_status={} platform_code={status}", response.status);
    }
    match status {
        200 => Ok(response.body),
        301 | 302 => Err(ErrorCode::Unauthenticated.into()),
        403 => Err(ErrorCode::PermissionDenied.into()),
        404 => Err(ErrorCode::NotFound.into()),
        429 => Err(ErrorCode::RateLimited.into()),
        _ => Err(ErrorCode::Network.into()),
    }
}

fn music_error(error: ncm_api_rs::error::NcmError) -> nons_music_adapter_sdk::MusicError {
    crate::diagnostics::warn("music.request", &error);
    music_error_code(error)
}

fn music_error_code(error: ncm_api_rs::error::NcmError) -> nons_music_adapter_sdk::MusicError {
    use ncm_api_rs::error::NcmError;
    use nons_music_adapter_sdk::ErrorCode;
    let code = match error {
        NcmError::AuthRequired(_) => ErrorCode::Unauthenticated,
        NcmError::RateLimited(_) => ErrorCode::RateLimited,
        NcmError::Timeout(_) => ErrorCode::DeadlineExceeded,
        NcmError::InvalidParam(_) | NcmError::ResponseTooLarge | NcmError::Json(_) => {
            ErrorCode::InvalidData
        }
        NcmError::Api { code: 403, .. } => ErrorCode::PermissionDenied,
        NcmError::Api { code: 404, .. } => ErrorCode::NotFound,
        NcmError::Api { code: 451, .. } => ErrorCode::RegionRestricted,
        NcmError::Api {
            code: 429 | 503, ..
        } => ErrorCode::RateLimited,
        NcmError::Api {
            code: 301 | 302, ..
        } => ErrorCode::Unauthenticated,
        _ => ErrorCode::Network,
    };
    code.into()
}

#[cfg(test)]
mod tests {
    use super::{
        account_profile, checked, music_checked, qr_key, song_aliases, song_information_from_json,
        ApiResponse,
    };
    use crate::netease::NeteaseAdapter;
    use nons_music_adapter_sdk::{
        account::SessionContext,
        adapter::{Cancellation, MusicAdapter, PageRequest, RequestContext},
        business::BusinessRequest,
        identity::{EntityKind, SourceId},
        resource::{ResolveRequest, ResourceAccess},
        ErrorCode,
    };
    use serde_json::{json, Value};
    use std::{
        io::{Read, Write},
        sync::Arc,
        time::{Duration, Instant},
    };

    struct Reply {
        path: &'static str,
        status: u16,
        location: Option<&'static str>,
        body: Vec<u8>,
        delay: Duration,
    }
    impl Reply {
        fn ok(path: &'static str, body: Vec<u8>) -> Self {
            Self {
                path,
                status: 200,
                location: None,
                body,
                delay: Duration::ZERO,
            }
        }
        fn redirect(path: &'static str, location: &'static str) -> Self {
            Self {
                path,
                status: 302,
                location: Some(location),
                body: Vec::new(),
                delay: Duration::ZERO,
            }
        }
        fn delayed(path: &'static str, body: Vec<u8>, delay: Duration) -> Self {
            Self {
                path,
                status: 200,
                location: None,
                body,
                delay,
            }
        }
    }

    fn server(build: impl FnOnce(&str) -> Vec<Reply>) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let replies = build(&base);
        let handle = std::thread::spawn(move || {
            for reply in replies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut received = Vec::new();
                let mut buffer = [0; 8192];
                while !received.windows(4).any(|value| value == b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    assert_ne!(read, 0);
                    received.extend_from_slice(&buffer[..read]);
                }
                let request = std::str::from_utf8(&received).unwrap();
                let path = request.split_whitespace().nth(1).unwrap();
                assert!(path.starts_with(reply.path), "{path} != {}", reply.path);
                if !reply.delay.is_zero() {
                    std::thread::sleep(reply.delay);
                }
                let reason = if reply.status == 302 { "Found" } else { "OK" };
                let mut header = format!(
                    "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    reply.status,
                    reason,
                    reply.body.len()
                );
                if let Some(location) = reply.location {
                    header.push_str(&format!("Location: {location}\r\n"));
                }
                header.push_str("Content-Type: application/json\r\n\r\n");
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&reply.body);
            }
        });
        (base, handle)
    }

    fn padded(mut body: Value, bytes: usize) -> Vec<u8> {
        body["padding"] = json!("x".repeat(bytes));
        serde_json::to_vec(&body).unwrap()
    }

    fn adapter(domain: &str) -> NeteaseAdapter {
        let mut api = super::Netease::with_cookie(None);
        api.domain = Some(domain.to_owned());
        NeteaseAdapter::new(Arc::new(api))
    }

    fn context(deadline: Option<Instant>, cancelled: bool) -> RequestContext {
        let cancellation = Cancellation::default();
        if cancelled {
            cancellation.cancel();
        }
        RequestContext {
            session: SessionContext {
                source: SourceId::try_from("netease".to_owned()).unwrap(),
                account: None,
                generation: 0,
            },
            adapter_generation: 1,
            deadline,
            cancellation,
        }
    }

    #[tokio::test]
    async fn redirects_are_followed_for_api_and_resource_urls() {
        let (base, server) = server(|_| {
            vec![
                Reply::redirect("/api/redirect", "/redirected"),
                Reply::ok("/redirected", br#"{"code":200,"ok":true}"#.to_vec()),
                Reply::redirect("/resolve", "/resolve-final"),
                Reply::ok("/resolve-final", Vec::new()),
            ]
        });
        let client = ncm_api_rs::create_client(None);
        let redirected = client
            .request(
                "/api/redirect",
                json!({}),
                ncm_api_rs::request::RequestOption {
                    crypto: ncm_api_rs::request::CryptoType::Api,
                    domain: Some(base.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(redirected.body["ok"], true);
        assert_eq!(
            client
                .resolve_url(&format!("{base}/resolve"))
                .await
                .unwrap(),
            format!("{base}/resolve-final")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn large_business_track_and_playback_bodies_reach_entrypoints() {
        let (base, server) = server(|base| {
            vec![
                Reply::ok(
                    "/weapi/v3/song/detail",
                    padded(
                        json!({"code":200,"songs":[{"id":1,"name":"Song","ar":[],"al":{}}]}),
                        3 * 1024 * 1024,
                    ),
                ),
                Reply::ok(
                    "/api/song/enhance/player/url/v1",
                    padded(
                        json!({"code":200,"data":[{"id":1,"url":format!("{base}/audio"),"level":"lossless"}]}),
                        3 * 1024 * 1024,
                    ),
                ),
                Reply::redirect("/audio", "/audio-final"),
                Reply::ok("/audio-final", Vec::new()),
            ]
        });
        let adapter = adapter(&base);
        let context = context(None, false);
        let track = crate::reference(1).unwrap();
        adapter.read_track(&track, &context).await.unwrap();
        let resource = adapter
            .resolve_playback(
                &ResolveRequest {
                    track,
                    preferred_quality: "lossless".into(),
                    allow_downgrade: false,
                },
                &context,
            )
            .await
            .unwrap();
        let ResourceAccess::Http(access) = resource.access else {
            panic!("HTTP resource required")
        };
        assert_eq!(access.url, format!("{base}/audio-final"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn large_business_response_reaches_the_business_entrypoint() {
        let (base, server) = server(|_| {
            vec![Reply::ok(
                "/eapi/v6/playlist/detail",
                padded(
                    json!({"code":200,"playlist":{"id":42,"trackIds":[],"tracks":[]}}),
                    3 * 1024 * 1024,
                ),
            )]
        });
        let adapter = adapter(&base);
        let response = adapter
            .business(
                &BusinessRequest::Tracks {
                    entity: crate::entity_reference(EntityKind::Playlist, 42).unwrap(),
                    page: PageRequest {
                        cursor: None,
                        limit: 30,
                    },
                },
                &context(None, false),
            )
            .await
            .unwrap();
        assert!(matches!(
            response,
            nons_music_adapter_sdk::business::BusinessResponse::Tracks(_)
        ));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn no_deadline_waits_past_the_old_twelve_seconds() {
        let (base, server) = server(|_| {
            vec![Reply::delayed(
                "/weapi/v3/song/detail",
                serde_json::to_vec(
                    &json!({"code":200,"songs":[{"id":1,"name":"Slow","ar":[],"al":{}}]}),
                )
                .unwrap(),
                Duration::from_secs(16),
            )]
        });
        let started = Instant::now();
        let track = adapter(&base)
            .read_track(&crate::reference(1).unwrap(), &context(None, false))
            .await
            .unwrap();
        assert!(started.elapsed() >= Duration::from_secs(15));
        assert_eq!(track.title, "Slow");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn explicit_deadline_is_enforced_without_shortening_long_limits() {
        let body = serde_json::to_vec(
            &json!({"code":200,"songs":[{"id":1,"name":"Slow","ar":[],"al":{}}]}),
        )
        .unwrap();
        let (base, first_server) = server({
            let body = body.clone();
            move |_| {
                vec![Reply::delayed(
                    "/weapi/v3/song/detail",
                    body,
                    Duration::from_secs(2),
                )]
            }
        });
        let track = crate::reference(1).unwrap();
        let started = Instant::now();
        let error = adapter(&base)
            .read_track(
                &track,
                &context(Some(Instant::now() + Duration::from_millis(150)), false),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::DeadlineExceeded);
        assert!(started.elapsed() < Duration::from_secs(1));
        first_server.join().unwrap();

        let (base, server) = server(move |_| {
            vec![Reply::delayed(
                "/weapi/v3/song/detail",
                body,
                Duration::from_secs(2),
            )]
        });
        let track = adapter(&base)
            .read_track(
                &track,
                &context(Some(Instant::now() + Duration::from_secs(5)), false),
            )
            .await
            .unwrap();
        assert_eq!(track.title, "Slow");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn cancellation_stops_a_pending_request() {
        let (base, server) = server(|_| {
            vec![Reply::delayed(
                "/weapi/v3/song/detail",
                serde_json::to_vec(
                    &json!({"code":200,"songs":[{"id":1,"name":"Slow","ar":[],"al":{}}]}),
                )
                .unwrap(),
                Duration::from_secs(2),
            )]
        });
        let cancellation = Cancellation::default();
        let cancel = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });
        let context = RequestContext {
            session: context(None, false).session,
            adapter_generation: 1,
            deadline: None,
            cancellation,
        };
        let error = adapter(&base)
            .read_track(&crate::reference(1).unwrap(), &context)
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn expired_deadline_and_cancellation_fail_before_network() {
        let adapter = adapter("http://127.0.0.1:9");
        let track = crate::reference(1).unwrap();
        let expired = adapter
            .read_track(
                &track,
                &context(Some(Instant::now() - Duration::from_secs(1)), false),
            )
            .await
            .unwrap_err();
        assert_eq!(expired.code, ErrorCode::DeadlineExceeded);
        let cancelled = adapter
            .read_track(&track, &context(None, true))
            .await
            .unwrap_err();
        assert_eq!(cancelled.code, ErrorCode::Cancelled);
    }

    #[tokio::test]
    async fn platform_failures_keep_the_original_cause_before_public_error_translation() {
        struct Capture;
        thread_local! {
            static RECORDS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        impl log::Log for Capture {
            fn enabled(&self, _: &log::Metadata<'_>) -> bool {
                true
            }
            fn log(&self, record: &log::Record<'_>) {
                RECORDS.with(|records| records.borrow_mut().push(record.args().to_string()));
            }
            fn flush(&self) {}
        }
        static LOGGER: Capture = Capture;
        log::set_logger(&LOGGER).unwrap();
        log::set_max_level(log::LevelFilter::Info);
        let error = music_checked(async {
            Err(ncm_api_rs::error::NcmError::Unknown(
                "transport disconnected".into(),
            ))
        })
        .await
        .unwrap_err();
        assert_eq!(error.code, nons_music_adapter_sdk::ErrorCode::Network);
        let error = super::timed(async {
            Err(ncm_api_rs::error::NcmError::Timeout(
                "socket read timed out".into(),
            ))
        })
        .await
        .unwrap_err();
        assert_eq!(error, "网易云请求失败，请检查网络后重试");
        let records = RECORDS.with(|records| records.borrow().join("\n"));
        assert!(
            records.contains("operation=music.request")
                && records.contains("transport disconnected"),
            "{records}"
        );
        assert!(
            records.contains("operation=login.request")
                && records.contains("socket read timed out"),
            "{records}"
        );
        assert!(
            records.replace('\\', "/").contains("platform/mod.rs:"),
            "{records}"
        );
    }

    #[tokio::test]
    async fn large_liked_playlist_raw_body_does_not_fail_the_small_preview() {
        // The platform returns full metadata before the host selects 12 preview IDs.
        let body = json!({"code":200,"playlist":{"trackIds":[{"id":1}],
            "tracks":[{"description":"x".repeat(3 * 1024 * 1024)}]}});
        // Exercise the scoped transport too: changing only post-decode validation
        // would still fail while the HTTP body is being accumulated.
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let raw = serde_json::to_vec(&body).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 8192];
            let _ = stream.read(&mut request).unwrap();
            let header = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", raw.len());
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&raw);
        });
        let client = ncm_api_rs::create_client(None);
        let music_client = client.clone();
        let api = super::Netease {
            client,
            music_client,
            cookie: Default::default(),
            domain: None,
            accounts: None,
            frozen: false,
            session_generation: Default::default(),
        };
        let context = nons_music_adapter_sdk::adapter::RequestContext {
            session: nons_music_adapter_sdk::account::SessionContext {
                source: nons_music_adapter_sdk::identity::SourceId::try_from("netease".to_owned())
                    .unwrap(),
                account: None,
                generation: 0,
            },
            adapter_generation: 1,
            deadline: None,
            cancellation: Default::default(),
        };
        let scoped = api.scoped(&context).unwrap();
        let received = checked(scoped.client.request(
            "/api/v6/playlist/detail",
            json!({"id":1}),
            ncm_api_rs::request::RequestOption {
                crypto: ncm_api_rs::request::CryptoType::Api,
                domain: Some(format!("http://{address}")),
                ..Default::default()
            },
        ))
        .await;
        server.join().unwrap();
        assert!(
            received.is_ok(),
            "scoped playlist transport must accept bounded large bodies"
        );
        let result = checked(async {
            Ok(ApiResponse {
                status: 200,
                body,
                cookie: vec![],
            })
        })
        .await;
        assert!(
            result.is_ok(),
            "valid large playlist must not surface invalidData in preview: {result:?}"
        );
        assert!(
            music_checked(async {
                Ok(ApiResponse {
                    status: 200,
                    body: json!({"code":200,"padding":"x".repeat(3 * 1024 * 1024)}),
                    cookie: vec![],
                })
            })
            .await
            .is_ok(),
            "single-track/playback responses are no longer capped"
        );
        assert!(
            checked(async {
                Ok(ApiResponse {
                    status: 200,
                    body: json!({"code":200,"padding":"x".repeat(16 * 1024 * 1024)}),
                    cookie: vec![],
                })
            })
            .await
            .is_ok(),
            "business responses are no longer capped"
        );
    }

    #[test]
    fn song_navigation_keeps_structured_ids_without_splitting_artist_names() {
        assert!(super::track_from_json(&json!({"id":0,"name":"Invalid","ar":[]})).is_none());
        let track = super::track_from_json(&json!({"id":1,"name":"Song",
            "ar":[{"id":9,"name":"AC/DC"},{"id":10,"name":"B"}],"al":{"id":20,"name":"Album"}}))
        .unwrap();
        assert_eq!(track.artists.len(), 2);
        assert_eq!(track.artists[0].name, "AC/DC");
        let crate::model::TrackSource::Online { track } = track.source else {
            panic!("online track required")
        };
        assert_eq!(
            track.artists[1].reference.as_ref().unwrap().id.as_str(),
            "10"
        );
        assert_eq!(track.album_reference.as_ref().unwrap().id.as_str(), "20");
    }

    #[test]
    fn song_information_keeps_multiple_artist_ids_and_missing_fields_distinct() {
        let value = song_information_from_json(
            &serde_json::json!({"ar":[{"name":"甲","id":11},{"name":"乙","id":12}],"al":{"id":21},"publishTime":1595520000000u64}),
        );
        assert_eq!(value.artists.len(), 2);
        assert_eq!(value.artists[1].id, Some(12));
        assert_eq!(value.album_id, Some(21));
        assert_eq!(value.published_at, Some(1595520000000));
        let absent = song_information_from_json(
            &serde_json::json!({"artists":[{"name":"未知","id":0}],"album":{"id":0},"publishTime":0}),
        );
        assert_eq!(absent.artists[0].id, None);
        assert_eq!(absent.album_id, None);
        assert_eq!(absent.published_at, None);
    }
    #[test]
    fn aliases_merge_translation_and_alias_shapes_without_duplicates() {
        let song = json!({"name":"Причал", "tns":["码头", " "], "transNames":["码头"], "alia":["别名", "Причал"], "alias":["别名", "其他"]});
        assert_eq!(song_aliases(&song), vec!["码头", "别名", "其他"]);
        assert!(song_aliases(&json!({"name":"Song"})).is_empty());
    }

    #[test]
    fn account_profile_accepts_both_response_shapes_and_rejects_logged_out() {
        let profile = json!({"userId": 123, "nickname": "Listener", "avatarUrl": "https://example.com/avatar.png"});
        for body in [
            json!({"profile": profile}),
            json!({"data": {"profile": profile}}),
        ] {
            let account = account_profile(&body).unwrap();
            assert_eq!(account.nickname, "Listener");
            assert_eq!(account.avatar_url, "https://example.com/avatar.png");
        }
        for body in [
            json!({"data": {"profile": null}}),
            json!({"profile": {"userId": 0}}),
        ] {
            assert!(account_profile(&body).is_none());
        }
    }

    #[test]
    fn qr_key_accepts_sdk_root_response() {
        assert_eq!(
            qr_key(&json!({"code": 200, "unikey": "test-key"})).unwrap(),
            "test-key"
        );
    }

    #[test]
    fn qr_key_accepts_wrapped_response() {
        assert_eq!(
            qr_key(&json!({"code": 200, "data": {"unikey": "test-key"}})).unwrap(),
            "test-key"
        );
    }

    #[test]
    fn qr_key_rejects_invalid_response() {
        for body in [
            json!({"code": 200}),
            json!({"unikey": ""}),
            json!({"unikey": 123}),
        ] {
            assert!(qr_key(&body).is_err());
        }
    }
}

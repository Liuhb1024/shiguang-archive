use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, REFERER, USER_AGENT};
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::Value;
use tauri::Manager;

use crate::{
    media_storage::{
        existing_video, remove_matching_files, valid_mp4, with_deadline, TemporaryMedia,
    },
    network_policy::{
        checked_response_size, pinned_client, validate_outbound_url, validate_redirect,
        EndpointClass,
    },
    qlogin::QLoginState,
    qzone,
};

const MAX_ARCHIVED_IMAGE_BYTES: usize = 50 * 1024 * 1024;
const MAX_ARCHIVED_VIDEO_BYTES: usize = 1024 * 1024 * 1024;
const MAX_MEDIA_REDIRECTS: usize = 3;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveProgress {
    status: &'static str,
    pages: u32,
    fetched: u64,
    saved: u64,
    skipped: u32,
    message: String,
    retry_at: Option<i64>,
    batch_retry: Option<BatchRetryProgress>,
}

/// 批量重试异常跳过记录时的实时进度。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRetryProgress {
    current: u32,
    total: u32,
    recovered: u32,
    failed: u32,
    recovered_records: u64,
}

impl Default for ArchiveProgress {
    fn default() -> Self {
        Self {
            status: "idle",
            pages: 0,
            fetched: 0,
            saved: 0,
            skipped: 0,
            message: "尚未开始归档".into(),
            retry_at: None,
            batch_retry: None,
        }
    }
}

pub struct ArchiveState {
    work: tokio::sync::Mutex<()>,
    progress: Mutex<ArchiveProgress>,
    cancel: AtomicBool,
    batch_retrying: AtomicBool,
    batch_cancel: AtomicBool,
    image_downloads: tokio::sync::Semaphore,
    video_downloads: tokio::sync::Semaphore,
    media_paused: AtomicBool,
}

impl ArchiveState {
    pub fn new() -> Self {
        Self {
            work: tokio::sync::Mutex::new(()),
            progress: Mutex::new(ArchiveProgress::default()),
            cancel: AtomicBool::new(false),
            batch_retrying: AtomicBool::new(false),
            batch_cancel: AtomicBool::new(false),
            image_downloads: tokio::sync::Semaphore::new(4),
            video_downloads: tokio::sync::Semaphore::new(1),
            media_paused: AtomicBool::new(false),
        }
    }
}

/// Cleanup also runs when a request is cancelled or its IPC future is dropped.
struct TaskCleanup<'a>(&'a ArchiveState);

impl TaskCleanup<'_> {
    fn finish<T>(&self, result: &Result<T, String>) {
        if let Err(error) = result {
            set_progress(self.0, |p| {
                if let Some(retry_at) = error.strip_prefix("ARCHIVE_RATE_LIMIT:") {
                    p.status = "limited";
                    p.retry_at = retry_at.parse().ok();
                    p.message = "已达到本工具的动态请求上限（含重试），任务已暂停，倒计时结束后可继续。此上限不代表腾讯认可的安全频率。".into();
                    return;
                }
                p.status = if error == crate::operations::CANCELLED {
                    "cancelled"
                } else {
                    "error"
                };
                p.message = concise_archive_error(error);
                p.retry_at = None;
            });
        }
    }
}

impl Drop for TaskCleanup<'_> {
    fn drop(&mut self) {
        self.0.batch_retrying.store(false, Ordering::Relaxed);
        self.0.batch_cancel.store(false, Ordering::Relaxed);
        set_progress(self.0, |p| {
            p.batch_retry = None;
            if p.status == "running" {
                p.status = "cancelled";
                p.message = "任务已停止，已保存的进度会保留".into();
                p.retry_at = None;
            }
        });
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveItem {
    #[serde(skip)]
    owner_uin: String,
    id: i64,
    cell_id: String,
    published_at: i64,
    content: Option<String>,
    content_recovered: bool,
    is_blog: bool,
    can_open_original: bool,
    author_uin: Option<String>,
    author_name: Option<String>,
    picture_urls: Vec<String>,
    video_url: Option<String>,
    video_urls: Vec<String>,
    video_cover_url: Option<String>,
    like_count: i64,
    comment_count: i64,
    likes: Vec<LikeUser>,
    comments: Vec<ArchiveComment>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveComment {
    #[serde(skip)]
    comment_id: Option<String>,
    uin: Option<String>,
    nickname: Option<String>,
    content: String,
    created_at: i64,
    replies: Vec<ArchiveReply>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveReply {
    uin: Option<String>,
    nickname: Option<String>,
    reply_to_uin: Option<String>,
    reply_to_nickname: Option<String>,
    content: String,
    created_at: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LikeUser {
    uin: Option<String>,
    nickname: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Interactor {
    uin: String,
    nickname: String,
    likes: u64,
    comments: u64,
    total: u64,
    last_at: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveOverview {
    dynamics: u64,
    pictures: u64,
    comments: u64,
    likes: u64,
    database_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractionRank {
    uin: String,
    nickname: String,
    interactions: u64,
    likes: u64,
    comments: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveMediaItem {
    key: String,
    dynamic_id: i64,
    media_type: &'static str,
    picture_index: Option<usize>,
    url: String,
    cover_url: Option<String>,
    published_at: i64,
    author_uin: Option<String>,
    author_name: Option<String>,
    content: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveMediaPage {
    items: Vec<ArchiveMediaItem>,
    total: usize,
    years: Vec<i32>,
}

struct ParsedFeed {
    feed_key: String,
    cell_id: Option<String>,
    event_type: i64,
    event_time: i64,
    title: Option<String>,
    content: Option<String>,
    event_summary: Option<String>,
    actor_uin: Option<String>,
    actor_name: Option<String>,
    original_author_uin: Option<String>,
    original_author_name: Option<String>,
    picture_count: i64,
    pictures_json: Option<String>,
    video_json: Option<String>,
    comments_json: Option<String>,
    raw_json: String,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveSkipItem {
    id: i64,
    page_number: u32,
    cursor_offset: i64,
    offset_advance: i64,
    base_time: i64,
    error: String,
    skipped_at: i64,
    retry_count: u32,
    last_retry_at: Option<i64>,
    resolved_at: Option<i64>,
    recovered_records: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveSkipRetryResult {
    success: bool,
    message: String,
    recovered_records: u64,
}

fn stable_feed_hash(value: &Value) -> u64 {
    // FNV-1a keeps fallback keys deterministic without adding a hashing dependency.
    value
        .to_string()
        .bytes()
        .fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        })
}

fn archive_page_delay_ms(interval_ms: u64) -> u64 {
    let interval_ms = interval_ms.clamp(2_000, 30_000);
    let jitter_range = (interval_ms / 4).max(1);
    let subsecond_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    interval_ms + subsecond_nanos % (jitter_range + 1)
}

#[cfg(unix)]
fn secure_local_path_permissions(path: &Path, is_directory: bool) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let mode = if is_directory { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("无法收紧本地归档权限：{error}"))
}

#[cfg(not(unix))]
fn secure_local_path_permissions(_path: &Path, _is_directory: bool) -> Result<(), String> {
    // Windows application-data directories inherit the current user's ACL.
    Ok(())
}

pub(crate) fn secure_app_directories(app: &tauri::AppHandle) -> Result<(), String> {
    let mut directories = vec![
        app.path()
            .app_data_dir()
            .map_err(|error| format!("无法获取应用数据目录：{error}"))?,
        app.path()
            .app_cache_dir()
            .map_err(|error| format!("无法获取应用缓存目录：{error}"))?,
    ];
    #[cfg(target_os = "macos")]
    {
        let webkit_root = app
            .path()
            .home_dir()
            .map_err(|error| format!("无法获取用户目录：{error}"))?
            .join("Library")
            .join("WebKit")
            .join(&app.config().identifier);
        directories.push(webkit_root.join("WebsiteData"));
        directories.push(webkit_root);
    }
    for directory in directories {
        fs::create_dir_all(&directory).map_err(|error| format!("无法创建本地应用目录：{error}"))?;
        secure_local_path_permissions(&directory, true)?;
    }
    Ok(())
}

fn database_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法获取应用数据目录：{error}"))?;
    fs::create_dir_all(&dir).map_err(|error| format!("无法创建应用数据目录：{error}"))?;
    secure_local_path_permissions(&dir, true)?;
    Ok(dir.join("qzone-archive.sqlite3"))
}

fn open_database(app: &tauri::AppHandle) -> Result<Connection, String> {
    let path = database_path(app)?;
    let mut connection =
        Connection::open(&path).map_err(|error| format!("无法打开归档数据库：{error}"))?;
    secure_local_path_permissions(&path, false)?;
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         CREATE TABLE IF NOT EXISTS archive_feeds (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           owner_uin TEXT NOT NULL,
           feed_key TEXT NOT NULL,
           cell_id TEXT,
           event_type INTEGER NOT NULL DEFAULT 0,
           event_time INTEGER NOT NULL DEFAULT 0,
           title TEXT,
           content TEXT,
           event_summary TEXT,
           actor_uin TEXT,
           actor_name TEXT,
           original_author_uin TEXT,
           original_author_name TEXT,
           picture_count INTEGER NOT NULL DEFAULT 0,
           pictures_json TEXT,
           video_json TEXT,
           comments_json TEXT,
           raw_json TEXT NOT NULL,
           archived_at INTEGER NOT NULL,
           UNIQUE(owner_uin, feed_key)
         );
         CREATE INDEX IF NOT EXISTS idx_archive_feeds_owner_time
           ON archive_feeds(owner_uin, event_time DESC);
         CREATE INDEX IF NOT EXISTS idx_archive_feeds_dynamic_type
           ON archive_feeds(owner_uin, cell_id, event_type);
         CREATE TABLE IF NOT EXISTS archive_dynamics (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           owner_uin TEXT NOT NULL,
           cell_id TEXT NOT NULL,
           published_at INTEGER NOT NULL DEFAULT 0,
           content TEXT,
           author_uin TEXT,
           author_name TEXT,
           category TEXT NOT NULL DEFAULT '',
           pictures_json TEXT,
           video_json TEXT,
           raw_original_json TEXT NOT NULL,
           archived_at INTEGER NOT NULL,
           UNIQUE(owner_uin, cell_id)
         );
         CREATE INDEX IF NOT EXISTS idx_archive_dynamics_owner_time
           ON archive_dynamics(owner_uin, published_at DESC);
         CREATE TABLE IF NOT EXISTS archive_checkpoints (
           owner_uin TEXT PRIMARY KEY,
           attach_info TEXT NOT NULL,
           pages INTEGER NOT NULL DEFAULT 0,
           fetched INTEGER NOT NULL DEFAULT 0,
           saved INTEGER NOT NULL DEFAULT 0,
           updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS archive_rate_limits (
           owner_uin TEXT PRIMARY KEY,
           window_started_at INTEGER NOT NULL,
           requested_pages INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS archive_skips (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           owner_uin TEXT NOT NULL,
           cursor TEXT NOT NULL,
           resume_cursor TEXT NOT NULL,
           page_number INTEGER NOT NULL,
           cursor_offset INTEGER NOT NULL,
           offset_advance INTEGER NOT NULL,
           base_time INTEGER NOT NULL,
           error TEXT NOT NULL,
           skipped_at INTEGER NOT NULL,
           retry_count INTEGER NOT NULL DEFAULT 0,
           last_retry_at INTEGER,
           resolved_at INTEGER,
           recovered_records INTEGER NOT NULL DEFAULT 0,
           UNIQUE(owner_uin, cursor_offset, base_time)
         );",
        )
        .map_err(|error| format!("初始化归档数据库失败：{error}"))?;
    if connection
        .prepare("SELECT pages,fetched,saved FROM archive_checkpoints LIMIT 0")
        .is_err()
    {
        connection
            .execute_batch(
                "ALTER TABLE archive_checkpoints ADD COLUMN pages INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE archive_checkpoints ADD COLUMN fetched INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE archive_checkpoints ADD COLUMN saved INTEGER NOT NULL DEFAULT 0;",
            )
            .map_err(|error| format!("升级归档续传统计失败：{error}"))?;
    }
    if connection
        .prepare("SELECT category FROM archive_dynamics LIMIT 0")
        .is_err()
    {
        connection
            .execute(
                "ALTER TABLE archive_dynamics ADD COLUMN category TEXT NOT NULL DEFAULT ''",
                [],
            )
            .map_err(|error| format!("升级归档分类失败：{error}"))?;
    }
    migrate_legacy_dynamics(&mut connection)?;
    migrate_dynamic_categories(&mut connection)?;
    Ok(connection)
}

fn migrate_dynamic_categories(connection: &mut Connection) -> Result<(), String> {
    let pending: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM archive_dynamics WHERE category=''",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("检查归档分类迁移状态失败：{error}"))?;
    if pending == 0 {
        return Ok(());
    }
    let feeds = {
        let mut statement = connection
            .prepare("SELECT owner_uin,raw_json FROM archive_feeds")
            .map_err(|error| format!("读取待分类归档失败：{error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| format!("查询待分类归档失败：{error}"))?;
        rows.filter_map(Result::ok).collect::<Vec<_>>()
    };
    let transaction = connection
        .transaction()
        .map_err(|error| format!("开始归档分类迁移失败：{error}"))?;
    for (owner_uin, raw_json) in feeds {
        if let Ok(feed) = serde_json::from_str::<Value>(&raw_json) {
            save_original_dynamic(&transaction, &owner_uin, &feed)?;
        }
    }
    transaction.execute(
        "UPDATE archive_dynamics SET category=CASE WHEN author_uin=owner_uin THEN 'self' ELSE 'other' END WHERE category=''",
        [],
    ).map_err(|error| format!("补全归档分类失败：{error}"))?;
    transaction
        .commit()
        .map_err(|error| format!("提交归档分类迁移失败：{error}"))
}

fn migrate_legacy_dynamics(connection: &mut Connection) -> Result<(), String> {
    let dynamic_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM archive_dynamics", [], |row| {
            row.get(0)
        })
        .map_err(|error| format!("检查原动态迁移状态失败：{error}"))?;
    if dynamic_count > 0 {
        return Ok(());
    }
    let legacy = {
        let mut statement = connection
            .prepare("SELECT owner_uin,raw_json FROM archive_feeds")
            .map_err(|error| format!("读取旧归档失败：{error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| format!("查询旧归档失败：{error}"))?;
        rows.filter_map(Result::ok).collect::<Vec<_>>()
    };
    if legacy.is_empty() {
        return Ok(());
    }
    let transaction = connection
        .transaction()
        .map_err(|error| format!("开始旧归档迁移失败：{error}"))?;
    for (owner_uin, raw_json) in legacy {
        if let Ok(feed) = serde_json::from_str::<Value>(&raw_json) {
            save_original_dynamic(&transaction, &owner_uin, &feed)?;
        }
    }
    transaction
        .commit()
        .map_err(|error| format!("提交旧归档迁移失败：{error}"))
}

fn text_at(value: &Value, pointer: &str) -> Option<String> {
    value.pointer(pointer).and_then(|value| match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    })
}

fn parse_feed(feed: &Value) -> Result<ParsedFeed, String> {
    let cell_id = text_at(feed, "/original/cell_id/cellid");
    let event_time = feed
        .pointer("/comm/time")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let event_type = feed
        .pointer("/comm/subid")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let actor_uin = text_at(feed, "/userinfo/user/uin");
    let feed_key = text_at(feed, "/comm/feedskey")
        .or_else(|| text_at(feed, "/original/cell_comm/feedskey"))
        .or_else(|| {
            cell_id.as_ref().map(|id| {
                format!(
                    "{event_type}:{id}:{event_time}:{}",
                    actor_uin.as_deref().unwrap_or("unknown")
                )
            })
        })
        .unwrap_or_else(|| {
            format!(
                "fallback:{event_type}:{event_time}:{}:{:016x}",
                actor_uin.as_deref().unwrap_or("unknown"),
                stable_feed_hash(feed)
            )
        });
    let pictures = feed.pointer("/original/cell_pic");
    let picture_count = pictures
        .and_then(|value| value.pointer("/picdata/pic"))
        .and_then(Value::as_array)
        .map(|items| items.len() as i64)
        .unwrap_or(0);
    let video = feed
        .pointer("/original/cell_video")
        .filter(|value| !value.is_null());
    let comments = feed
        .pointer("/original/cell_comment")
        .filter(|value| !value.is_null());
    Ok(ParsedFeed {
        feed_key,
        cell_id,
        event_type,
        event_time,
        title: text_at(feed, "/title/title"),
        content: text_at(feed, "/original/cell_summary/summary"),
        event_summary: text_at(feed, "/summary/summary"),
        actor_uin,
        actor_name: text_at(feed, "/userinfo/user/nickname"),
        original_author_uin: text_at(feed, "/original/cell_userinfo/user/uin"),
        original_author_name: text_at(feed, "/original/cell_userinfo/user/nickname"),
        picture_count,
        pictures_json: pictures.map(Value::to_string),
        video_json: video.map(Value::to_string),
        comments_json: comments.map(Value::to_string),
        raw_json: feed.to_string(),
    })
}

fn save_feed_rows(
    transaction: &rusqlite::Transaction<'_>,
    owner_uin: &str,
    feeds: &[Value],
) -> Result<u64, String> {
    let mut saved = 0;
    for feed in feeds {
        save_original_dynamic(transaction, owner_uin, feed)?;
        let feed = parse_feed(feed)?;
        let changed = transaction.execute(
            "INSERT INTO archive_feeds
             (owner_uin, feed_key, cell_id, event_type, event_time, title, content, event_summary,
              actor_uin, actor_name, original_author_uin, original_author_name, picture_count,
              pictures_json, video_json, comments_json, raw_json, archived_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
             ON CONFLICT(owner_uin, feed_key) DO UPDATE SET
              cell_id=excluded.cell_id,event_type=excluded.event_type,event_time=excluded.event_time,
              title=excluded.title,content=excluded.content,event_summary=excluded.event_summary,
              actor_uin=excluded.actor_uin,actor_name=excluded.actor_name,
              original_author_uin=excluded.original_author_uin,original_author_name=excluded.original_author_name,
              picture_count=excluded.picture_count,pictures_json=excluded.pictures_json,
              video_json=excluded.video_json,comments_json=excluded.comments_json,
              raw_json=excluded.raw_json,archived_at=excluded.archived_at",
            params![owner_uin, feed.feed_key, feed.cell_id, feed.event_type, feed.event_time,
                feed.title, feed.content, feed.event_summary, feed.actor_uin, feed.actor_name,
                feed.original_author_uin, feed.original_author_name, feed.picture_count,
                feed.pictures_json, feed.video_json, feed.comments_json, feed.raw_json, now()],
        ).map_err(|error| format!("保存动态失败：{error}"))?;
        saved += changed as u64;
    }
    Ok(saved)
}

fn save_page(
    app: &tauri::AppHandle,
    owner_uin: &str,
    feeds: &[Value],
    next_cursor: Option<&str>,
    reset_checkpoint_stats: bool,
) -> Result<u64, String> {
    let mut connection = open_database(app)?;
    let transaction = connection
        .transaction()
        .map_err(|error| format!("无法开始数据库事务：{error}"))?;
    let saved = save_feed_rows(&transaction, owner_uin, feeds)?;
    if let Some(cursor) = next_cursor {
        if reset_checkpoint_stats {
            transaction.execute(
                "INSERT INTO archive_checkpoints(owner_uin,attach_info,pages,fetched,saved,updated_at) VALUES (?1,?2,1,?3,?4,?5)
                 ON CONFLICT(owner_uin) DO UPDATE SET attach_info=excluded.attach_info,
                  pages=1,fetched=excluded.fetched,saved=excluded.saved,updated_at=excluded.updated_at",
                params![owner_uin, cursor, feeds.len() as u64, saved, now()],
            ).map_err(|error| format!("重置归档续传位置失败：{error}"))?;
        } else {
            transaction.execute(
                "INSERT INTO archive_checkpoints(owner_uin,attach_info,pages,fetched,saved,updated_at) VALUES (?1,?2,1,?3,?4,?5)
                 ON CONFLICT(owner_uin) DO UPDATE SET attach_info=excluded.attach_info,
                  pages=archive_checkpoints.pages+1,fetched=archive_checkpoints.fetched+excluded.fetched,
                  saved=archive_checkpoints.saved+excluded.saved,updated_at=excluded.updated_at",
                params![owner_uin, cursor, feeds.len() as u64, saved, now()],
            ).map_err(|error| format!("保存归档续传位置失败：{error}"))?;
        }
    } else {
        transaction
            .execute(
                "DELETE FROM archive_checkpoints WHERE owner_uin=?1",
                params![owner_uin],
            )
            .map_err(|error| format!("清除归档续传位置失败：{error}"))?;
    }
    transaction
        .commit()
        .map_err(|error| format!("提交归档事务失败：{error}"))?;
    Ok(saved)
}

fn save_retried_page(
    app: &tauri::AppHandle,
    owner_uin: &str,
    feeds: &[Value],
) -> Result<u64, String> {
    let mut connection = open_database(app)?;
    let transaction = connection
        .transaction()
        .map_err(|error| format!("无法开始重试事务：{error}"))?;
    let saved = save_feed_rows(&transaction, owner_uin, feeds)?;
    transaction
        .commit()
        .map_err(|error| format!("提交重试事务失败：{error}"))?;
    Ok(saved)
}

struct ArchiveCheckpoint {
    cursor: String,
    pages: u32,
    fetched: u64,
    saved: u64,
    updated_at: i64,
}

const ARCHIVE_RATE_WINDOW_SECONDS: i64 = 10 * 60;
const ARCHIVE_RATE_PAGE_LIMIT: i64 = 300;
const ARCHIVE_CURSOR_MAX_AGE_SECONDS: i64 = 10 * 60;
const ARCHIVE_SKIP_MAX_OFFSET_ADVANCE: i64 = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FeedCursorDetails {
    offset: i64,
    base_time: i64,
    load_count: i64,
}

fn parse_query_pairs(value: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(value.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

fn serialize_query_pairs(pairs: &[(String, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

fn pair_value<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(candidate, _)| candidate == key)
        .map(|(_, value)| value.as_str())
}

fn set_pair_value(pairs: &mut [(String, String)], key: &str, value: String) -> Result<(), String> {
    let pair = pairs
        .iter_mut()
        .find(|(candidate, _)| candidate == key)
        .ok_or_else(|| format!("分页游标缺少 {key}"))?;
    pair.1 = value;
    Ok(())
}

fn set_or_append_pair_value(pairs: &mut Vec<(String, String)>, key: &str, value: String) {
    if let Some(pair) = pairs.iter_mut().find(|(candidate, _)| candidate == key) {
        pair.1 = value;
    } else {
        pairs.push((key.to_owned(), value));
    }
}

fn parse_feed_cursor(cursor: &str) -> Result<FeedCursorDetails, String> {
    let outer = parse_query_pairs(cursor);
    let attach = pair_value(&outer, "att").ok_or("分页游标缺少 att")?;
    let attach = parse_query_pairs(attach);
    let backend = pair_value(&attach, "back_server_info").ok_or("分页游标缺少 back_server_info")?;
    let backend = parse_query_pairs(backend);
    let parse_number = |pairs: &[(String, String)], key: &str| {
        pair_value(pairs, key)
            .ok_or_else(|| format!("分页游标缺少 {key}"))?
            .parse::<i64>()
            .map_err(|_| format!("分页游标中的 {key} 不是有效数字"))
    };
    let load_count = pair_value(&outer, "loadcount")
        .or_else(|| pair_value(&attach, "loadcount"))
        .map(|value| {
            value
                .parse::<i64>()
                .map_err(|_| "分页游标中的 loadcount 不是有效数字".to_owned())
        })
        .transpose()?
        .unwrap_or(0);
    Ok(FeedCursorDetails {
        offset: parse_number(&backend, "offset")?,
        base_time: parse_number(&backend, "basetime")?,
        load_count,
    })
}

fn advance_feed_cursor(cursor: &str, offset_advance: i64) -> Result<String, String> {
    if offset_advance <= 0 {
        return Err("跳过偏移量必须大于 0".into());
    }
    let details = parse_feed_cursor(cursor)?;
    let mut outer = parse_query_pairs(cursor);
    let mut attach = parse_query_pairs(pair_value(&outer, "att").ok_or("分页游标缺少 att")?);
    let mut backend = parse_query_pairs(
        pair_value(&attach, "back_server_info").ok_or("分页游标缺少 back_server_info")?,
    );
    let load_count_in_outer = pair_value(&outer, "loadcount").is_some();
    set_pair_value(
        &mut backend,
        "offset",
        details.offset.saturating_add(offset_advance).to_string(),
    )?;
    set_pair_value(
        &mut attach,
        "back_server_info",
        serialize_query_pairs(&backend),
    )?;
    if !load_count_in_outer {
        set_or_append_pair_value(
            &mut attach,
            "loadcount",
            details.load_count.saturating_add(1).to_string(),
        );
    }
    set_pair_value(&mut outer, "att", serialize_query_pairs(&attach))?;
    if load_count_in_outer {
        set_pair_value(
            &mut outer,
            "loadcount",
            details.load_count.saturating_add(1).to_string(),
        )?;
    }
    Ok(serialize_query_pairs(&outer))
}

fn unresolved_skip_count(app: &tauri::AppHandle, owner_uin: &str) -> Result<u32, String> {
    let connection = open_database(app)?;
    connection
        .query_row(
            "SELECT COUNT(*) FROM archive_skips WHERE owner_uin=?1 AND resolved_at IS NULL",
            params![owner_uin],
            |row| row.get(0),
        )
        .map_err(|error| format!("读取异常跳过数量失败：{error}"))
}

fn known_skip_advance(
    app: &tauri::AppHandle,
    owner_uin: &str,
    details: FeedCursorDetails,
) -> Result<Option<(i64, String)>, String> {
    let connection = open_database(app)?;
    match connection.query_row(
        "SELECT offset_advance,error FROM archive_skips
         WHERE owner_uin=?1 AND cursor_offset=?2 AND base_time=?3 AND resolved_at IS NULL",
        params![owner_uin, details.offset, details.base_time],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ) {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(format!("读取已知异常位置失败：{error}")),
    }
}

struct SkipRecord<'a> {
    cursor: &'a str,
    resume_cursor: &'a str,
    page_number: u32,
    details: FeedCursorDetails,
    offset_advance: i64,
    error: &'a str,
}

fn record_archive_skip(
    app: &tauri::AppHandle,
    owner_uin: &str,
    record: SkipRecord<'_>,
) -> Result<(), String> {
    let connection = open_database(app)?;
    connection.execute(
        "INSERT INTO archive_skips
         (owner_uin,cursor,resume_cursor,page_number,cursor_offset,offset_advance,base_time,error,skipped_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(owner_uin,cursor_offset,base_time) DO UPDATE SET
          cursor=excluded.cursor,resume_cursor=excluded.resume_cursor,page_number=excluded.page_number,
          offset_advance=excluded.offset_advance,error=excluded.error,skipped_at=excluded.skipped_at,
          resolved_at=NULL,recovered_records=0",
        params![
            owner_uin,
            record.cursor,
            record.resume_cursor,
            record.page_number,
            record.details.offset,
            record.offset_advance,
            record.details.base_time,
            concise_archive_error(record.error),
            now(),
        ],
    ).map_err(|error| format!("保存异常跳过记录失败：{error}"))?;
    Ok(())
}

fn checkpoint_is_stale(checkpoint: &ArchiveCheckpoint, current: i64) -> bool {
    current.saturating_sub(checkpoint.updated_at) >= ARCHIVE_CURSOR_MAX_AGE_SECONDS
}

fn reserve_archive_page(app: &tauri::AppHandle, owner_uin: &str) -> Result<Option<i64>, String> {
    let connection = open_database(app)?;
    let current = now();
    let state = connection.query_row(
        "SELECT window_started_at,requested_pages FROM archive_rate_limits WHERE owner_uin=?1",
        params![owner_uin],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    );
    match state {
        Ok((started_at, pages))
            if current - started_at < ARCHIVE_RATE_WINDOW_SECONDS
                && pages >= ARCHIVE_RATE_PAGE_LIMIT =>
        {
            Ok(Some(started_at + ARCHIVE_RATE_WINDOW_SECONDS))
        }
        Ok((started_at, _)) if current - started_at >= ARCHIVE_RATE_WINDOW_SECONDS => {
            connection.execute(
                "UPDATE archive_rate_limits SET window_started_at=?2,requested_pages=1 WHERE owner_uin=?1",
                params![owner_uin, current],
            ).map_err(|error| format!("重置归档频率窗口失败：{error}"))?;
            Ok(None)
        }
        Ok(_) => {
            connection.execute(
                "UPDATE archive_rate_limits SET requested_pages=requested_pages+1 WHERE owner_uin=?1",
                params![owner_uin],
            ).map_err(|error| format!("记录归档请求频率失败：{error}"))?;
            Ok(None)
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            connection.execute(
                "INSERT INTO archive_rate_limits(owner_uin,window_started_at,requested_pages) VALUES (?1,?2,1)",
                params![owner_uin, current],
            ).map_err(|error| format!("创建归档频率窗口失败：{error}"))?;
            Ok(None)
        }
        Err(error) => Err(format!("读取归档请求频率失败：{error}")),
    }
}

fn reserve_archive_request(app: &tauri::AppHandle, owner_uin: &str) -> Result<(), String> {
    match reserve_archive_page(app, owner_uin)? {
        Some(retry_at) => Err(format!("ARCHIVE_RATE_LIMIT:{retry_at}")),
        None => Ok(()),
    }
}

fn load_checkpoint(
    app: &tauri::AppHandle,
    owner_uin: &str,
) -> Result<Option<ArchiveCheckpoint>, String> {
    let connection = open_database(app)?;
    match connection.query_row(
        "SELECT attach_info,pages,fetched,saved,updated_at FROM archive_checkpoints WHERE owner_uin=?1",
        params![owner_uin],
        |row| {
            Ok(ArchiveCheckpoint {
                cursor: row.get(0)?,
                pages: row.get(1)?,
                fetched: row.get(2)?,
                saved: row.get(3)?,
                updated_at: row.get(4)?,
            })
        },
    ) {
        Ok(checkpoint) if !checkpoint.cursor.trim().is_empty() => Ok(Some(checkpoint)),
        Ok(_) => Ok(None),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(format!("读取归档续传位置失败：{error}")),
    }
}

fn save_original_dynamic(
    transaction: &rusqlite::Transaction<'_>,
    owner_uin: &str,
    feed: &Value,
) -> Result<(), String> {
    let Some(original) = feed.get("original") else {
        return Ok(());
    };
    let Some(cell_id) = text_at(original, "/cell_id/cellid") else {
        return Ok(());
    };
    let original_appid = original
        .pointer("/cell_comm/appid")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let original_key = text_at(original, "/cell_comm/feedskey").unwrap_or_default();
    let is_guestbook = original_appid == 334 || original_key.starts_with("334_");
    let published_at = original
        .pointer("/cell_comm/time")
        .and_then(Value::as_i64)
        .or_else(|| feed.pointer("/comm/time").and_then(Value::as_i64))
        .unwrap_or(0);
    let content = if is_guestbook {
        text_at(feed, "/summary/summary")
    } else {
        text_at(original, "/cell_summary/summary")
    };
    let author_uin = if is_guestbook {
        text_at(feed, "/userinfo/user/uin")
    } else {
        text_at(original, "/cell_userinfo/user/uin")
    };
    let author_name = if is_guestbook {
        text_at(feed, "/userinfo/user/nickname")
    } else {
        text_at(original, "/cell_userinfo/user/nickname")
    };
    let category = if is_guestbook {
        "guestbook"
    } else if author_uin.as_deref() == Some(owner_uin) {
        "self"
    } else {
        "other"
    };
    let pictures_json = original
        .get("cell_pic")
        .filter(|value| !value.is_null())
        .map(Value::to_string);
    let video_json = original
        .get("cell_video")
        .filter(|value| !value.is_null())
        .map(Value::to_string);
    transaction.execute(
        "INSERT INTO archive_dynamics
         (owner_uin,cell_id,published_at,content,author_uin,author_name,category,pictures_json,video_json,raw_original_json,archived_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
         ON CONFLICT(owner_uin,cell_id) DO UPDATE SET
          published_at=excluded.published_at,content=excluded.content,author_uin=excluded.author_uin,
          author_name=excluded.author_name,category=excluded.category,pictures_json=COALESCE(excluded.pictures_json,archive_dynamics.pictures_json),
          video_json=COALESCE(excluded.video_json,archive_dynamics.video_json),
          raw_original_json=excluded.raw_original_json,archived_at=excluded.archived_at",
        params![owner_uin,cell_id,published_at,content,author_uin,author_name,category,pictures_json,video_json,original.to_string(),now()],
    ).map_err(|error| format!("保存原动态失败：{error}"))?;
    Ok(())
}

fn normalize_media_candidate(input: String) -> String {
    // Upgrade only within the existing media allowlist. Never send an HTTP
    // request, and preserve signed query bytes rather than parsing/re-encoding.
    let upgraded = if input.starts_with("//") {
        format!("https:{input}")
    } else if let Some(rest) = input.strip_prefix("http://") {
        format!("https://{rest}")
    } else {
        return input;
    };
    if validate_outbound_url(&upgraded, EndpointClass::Media).is_ok() {
        upgraded
    } else {
        input
    }
}

fn picture_url_candidates(json: Option<String>) -> Vec<Vec<String>> {
    let Some(value) = json.and_then(|text| serde_json::from_str::<Value>(&text).ok()) else {
        return vec![];
    };
    value
        .pointer("/picdata/pic")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|pic| {
            let photo_urls = pic.get("photourl").unwrap_or(&Value::Null);
            let values = match photo_urls {
                Value::Array(items) => items.iter().collect::<Vec<_>>(),
                Value::Object(items) => items.values().collect::<Vec<_>>(),
                _ => vec![],
            };
            let candidates = values
                .into_iter()
                .filter_map(|item| {
                    let url = item.get("url")?.as_str()?.trim();
                    if url.is_empty() {
                        return None;
                    }
                    Some(url.to_owned())
                })
                .collect::<Vec<_>>();
            let mut candidates = candidates;
            if let Some(url) = pic
                .pointer("/busi_param/-1")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|url| !url.is_empty())
            {
                candidates.push(url.to_owned());
            }
            let mut seen = HashSet::new();
            let urls = candidates
                .into_iter()
                .map(normalize_media_candidate)
                .filter(|url| seen.insert(url.clone()))
                .collect::<Vec<_>>();
            (!urls.is_empty()).then_some(urls)
        })
        .collect()
}

fn picture_urls(json: Option<String>) -> Vec<String> {
    picture_url_candidates(json)
        .into_iter()
        .filter_map(|urls| urls.into_iter().next())
        .collect()
}

fn image_file_stem(json: Option<&str>, id: i64, picture_index: usize) -> String {
    let value = json.and_then(|text| serde_json::from_str::<Value>(text).ok());
    let mut display_index = 0;
    let mut legacy_index = 0;
    for pic in value
        .as_ref()
        .and_then(|value| value.pointer("/picdata/pic"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let single = serde_json::json!({"picdata": {"pic": [pic]}});
        if picture_url_candidates(Some(single.to_string())).is_empty() {
            continue;
        }
        // Older versions skipped entries without photourl, so new recovered
        // entries must not shift the physical names of already cached photos.
        let existed_before = pic.get("photourl").is_some();
        if display_index == picture_index {
            return if existed_before {
                format!("{id}-{legacy_index}")
            } else {
                format!("{id}-recovered-{picture_index}")
            };
        }
        display_index += 1;
        if existed_before {
            legacy_index += 1;
        }
    }
    format!("{id}-recovered-{picture_index}")
}

fn archived_image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("jpg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.starts_with(b"BM") {
        Some("bmp")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.get(4..12).is_some_and(|value| {
        value.starts_with(b"ftyp") && (&value[4..8] == b"avif" || &value[4..8] == b"avis")
    }) {
        Some("avif")
    } else {
        None
    }
}

fn is_qq_missing_image_placeholder(bytes: &[u8]) -> bool {
    bytes.get(6..10).is_some_and(|size| {
        let width = u16::from_le_bytes([size[0], size[1]]);
        let height = u16::from_le_bytes([size[2], size[3]]);
        (bytes.len() == 2_038 && bytes.starts_with(b"GIF89a") && width == 340 && height == 320)
            || (bytes.len() == 2_687
                && bytes.starts_with(b"GIF89a")
                && width == 340
                && height == 320)
            || (bytes.len() == 1_643 && bytes.starts_with(b"GIF87a") && width == 99 && height == 99)
            || (bytes.len() == 1_547 && bytes.starts_with(b"GIF87a") && width == 98 && height == 98)
    })
}

fn existing_archived_image(image_dir: &std::path::Path, file_stem: &str) -> Option<PathBuf> {
    ["jpg", "png", "gif", "webp", "avif", "bmp"]
        .into_iter()
        .map(|extension| image_dir.join(format!("{file_stem}.{extension}")))
        .find_map(|path| {
            if !path.metadata().is_ok_and(|metadata| metadata.len() > 32) {
                return None;
            }
            if path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("gif"))
                && fs::read(&path).is_ok_and(|bytes| is_qq_missing_image_placeholder(&bytes))
            {
                let _ = fs::remove_file(&path);
                return None;
            }
            Some(path)
        })
}

fn expose_local_media(
    app: &tauri::AppHandle,
    login: &QLoginState,
    path: &Path,
) -> Result<String, String> {
    let root = app
        .path()
        .app_data_dir()
        .map_err(|_| "无法获取本地预览目录")?
        .join("session-previews");
    fs::create_dir_all(&root).map_err(|_| "无法创建本地预览目录")?;
    secure_local_path_permissions(&root, true)?;
    secure_local_path_permissions(path, false)?;
    let mut previews = login.previews.lock().map_err(|_| "本地预览状态不可用")?;
    let alias = previews.expose(path, &root, |alias| {
        app.asset_protocol_scope()
            .allow_file(alias)
            .map_err(|_| "无法授权本地媒体预览".to_owned())
    })?;
    Ok(alias.to_string_lossy().into_owned())
}

pub(crate) fn revoke_local_media(
    app: &tauri::AppHandle,
    login: &QLoginState,
) -> Result<(), String> {
    login
        .previews
        .lock()
        .map_err(|_| "本地预览状态不可用")?
        .revoke(|path| {
            app.asset_protocol_scope()
                .forbid_file(path)
                .map_err(|_| "无法撤销旧会话的媒体权限".to_owned())
        })
}

fn media_request_headers(user_agent: &str, accept: &str) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(user_agent).map_err(|_| "媒体请求标识无效".to_owned())?,
    );
    headers.insert(
        ACCEPT,
        HeaderValue::from_str(accept).map_err(|_| "媒体接收类型无效".to_owned())?,
    );
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("zh-CN,zh;q=0.9,en;q=0.8"),
    );
    headers.insert(
        REFERER,
        HeaderValue::from_static("https://user.qzone.qq.com/"),
    );
    Ok(headers)
}

async fn request_qq_media(
    input: &str,
    user_agent: &str,
    accept: &str,
    timeout: std::time::Duration,
) -> Result<reqwest::Response, String> {
    let normalized = normalize_media_candidate(input.to_owned());
    let mut current = validate_outbound_url(&normalized, EndpointClass::Media)?;
    let headers = media_request_headers(user_agent, accept)?;
    for redirect_count in 0..=MAX_MEDIA_REDIRECTS {
        let (url, client) = pinned_client(
            current.as_str(),
            EndpointClass::Media,
            std::time::Duration::from_secs(20),
            timeout,
        )
        .await?;
        let response = client
            .get(url.clone())
            .headers(headers.clone())
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    "QQ 媒体请求超时".to_owned()
                } else if error.is_connect() {
                    "无法连接 QQ 媒体服务器".to_owned()
                } else {
                    "QQ 媒体网络请求失败".to_owned()
                }
            })?;
        if !response.status().is_redirection() {
            return Ok(response);
        }
        if redirect_count == MAX_MEDIA_REDIRECTS {
            return Err("QQ 媒体跳转次数超过安全限制".into());
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or("QQ 媒体返回了无效跳转")?;
        current = validate_redirect(&url, location, EndpointClass::Media)?;
    }
    Err("QQ 媒体请求未完成".into())
}

async fn read_limited_response(
    mut response: reqwest::Response,
    maximum: usize,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(format!("响应超过 {} MB 安全限制", maximum / 1024 / 1024));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "读取 QQ 媒体响应失败".to_owned())?
    {
        checked_response_size(bytes.len(), chunk.len(), maximum)?;
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn write_limited_response(
    mut response: reqwest::Response,
    temporary: &mut TemporaryMedia,
    maximum: usize,
) -> Result<(usize, Vec<u8>), String> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(format!("响应超过 {} MB 安全限制", maximum / 1024 / 1024));
    }
    let mut received = 0usize;
    let mut prefix = Vec::with_capacity(16);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "读取 QQ 视频响应失败".to_owned())?
    {
        received = checked_response_size(received, chunk.len(), maximum)?;
        if prefix.len() < 16 {
            let take = (16 - prefix.len()).min(chunk.len());
            prefix.extend_from_slice(&chunk[..take]);
        }
        temporary.write(&chunk)?;
    }
    Ok((received, prefix))
}

#[tauri::command]
pub async fn load_archived_image(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    state: tauri::State<'_, ArchiveState>,
    id: i64,
    picture_index: usize,
) -> Result<String, String> {
    let mut operation = login.inner().operations.read().await;
    let result = operation
        .run(with_deadline(
            std::time::Duration::from_secs(45),
            load_archived_image_inner(app, login, state, id, picture_index),
        ))
        .await;
    result
}

async fn load_archived_image_inner(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    state: tauri::State<'_, ArchiveState>,
    id: i64,
    picture_index: usize,
) -> Result<String, String> {
    let auth = login.qzone_auth().await?;
    let image_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法获取图片归档目录：{error}"))?
        .join("images")
        .join(&auth.uin);
    fs::create_dir_all(&image_dir).map_err(|error| format!("无法创建图片归档目录：{error}"))?;
    secure_local_path_permissions(&image_dir, true)?;
    let pictures_json = {
        let connection = open_database(&app)?;
        connection
            .query_row(
                "SELECT pictures_json FROM archive_dynamics WHERE id=?1 AND owner_uin=?2",
                params![id, auth.uin],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => "当前账号中不存在这条图片归档".into(),
                _ => format!("读取图片归档失败：{error}"),
            })?
    };
    let file_stem = image_file_stem(pictures_json.as_deref(), id, picture_index);
    let candidates = picture_url_candidates(pictures_json)
        .into_iter()
        .nth(picture_index)
        .ok_or("该图片没有保存可用的 QQ 地址")?;
    // Reading an already saved file must not queue behind network downloads.
    if let Some(path) = existing_archived_image(&image_dir, &file_stem) {
        return expose_local_media(&app, &login, &path);
    }
    let _permit = state
        .image_downloads
        .acquire()
        .await
        .map_err(|_| "图片下载队列已关闭")?;
    if let Some(path) = existing_archived_image(&image_dir, &file_stem) {
        secure_local_path_permissions(&path, false)?;
        return expose_local_media(&app, &login, &path);
    }
    let mut last_error = String::new();
    if state.media_paused.load(Ordering::Relaxed) {
        return Err("HTTP 429：媒体下载已暂停，请稍后重新登录确认后再试".into());
    }
    for (attempt, url) in candidates.into_iter().take(8).enumerate() {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let response = match request_qq_media(
            &url,
            &auth.user_agent,
            "image/avif,image/webp,image/png,image/jpeg,image/*,*/*;q=0.8",
            std::time::Duration::from_secs(12),
        )
        .await
        {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                stop_on_media_limit(response.status(), &state, &login)?;
                last_error = format!("HTTP {}", response.status());
                continue;
            }
            Err(error) => {
                last_error = error;
                continue;
            }
        };
        let bytes = match read_limited_response(response, MAX_ARCHIVED_IMAGE_BYTES).await {
            Ok(bytes) => bytes,
            Err(error) => {
                last_error = error;
                continue;
            }
        };
        let Some(extension) = archived_image_extension(&bytes) else {
            last_error = "QQ 返回了非图片内容".into();
            continue;
        };
        if is_qq_missing_image_placeholder(&bytes) {
            last_error = "QQ 返回了图片不存在占位图".into();
            continue;
        }
        let path = image_dir.join(format!("{file_stem}.{extension}"));
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = image_dir.join(format!("{file_stem}-{nonce}.part"));
        let mut staged = TemporaryMedia::create(temporary)?;
        staged.write(&bytes)?;
        staged.commit(&path)?;
        return expose_local_media(&app, &login, &path);
    }
    Err(format!("所有 QQ 图片地址均加载失败：{last_error}"))
}

fn video_urls(json: Option<String>) -> Vec<String> {
    let Some(value) = json.and_then(|text| serde_json::from_str::<Value>(&text).ok()) else {
        return vec![];
    };
    let mut urls = Vec::new();
    if let Some(url) = value.get("videourl").and_then(Value::as_str) {
        urls.push(url.to_owned());
    }
    if let Some(items) = value.get("videourls").and_then(Value::as_object) {
        for url in items
            .values()
            .filter_map(|item| item.get("url").and_then(Value::as_str))
        {
            if !urls.iter().any(|saved| saved == url) {
                urls.push(url.to_owned());
            }
        }
    }
    urls
}

fn public_media_references(
    id: i64,
    pictures_json: Option<String>,
    video_json: Option<String>,
) -> (Vec<String>, Option<String>, Vec<String>, Option<String>) {
    let pictures = picture_urls(pictures_json)
        .into_iter()
        .enumerate()
        .map(|(index, _)| format!("local-picture-{id}-{index}"))
        .collect();
    let video = (!video_urls(video_json).is_empty()).then(|| format!("local-video-{id}"));
    let videos = video.iter().cloned().collect();
    (pictures, video, videos, None)
}

#[tauri::command]
pub async fn load_archived_video(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    state: tauri::State<'_, ArchiveState>,
    id: i64,
) -> Result<String, String> {
    let mut operation = login.inner().operations.read().await;
    let result = operation
        .run(with_deadline(
            std::time::Duration::from_secs(600),
            load_archived_video_inner(app, login, state, id),
        ))
        .await;
    result
}

async fn load_archived_video_inner(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    state: tauri::State<'_, ArchiveState>,
    id: i64,
) -> Result<String, String> {
    let auth = login.qzone_auth().await?;
    let video_json = {
        let connection = open_database(&app)?;
        connection
            .query_row(
                "SELECT video_json FROM archive_dynamics WHERE id=?1 AND owner_uin=?2",
                params![id, auth.uin],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => "当前账号中不存在这条视频归档".into(),
                _ => format!("读取视频归档失败：{error}"),
            })?
    };
    let cache_dir = app
        .path()
        .app_data_dir()
        .map_err(|_| "无法获取视频归档目录")?
        .join("videos")
        .join(&auth.uin);
    fs::create_dir_all(&cache_dir).map_err(|_| "无法创建视频归档目录")?;
    secure_local_path_permissions(&cache_dir, true)?;
    let cache_path = cache_dir.join(format!("{id}.mp4"));
    if existing_video(&cache_path, MAX_ARCHIVED_VIDEO_BYTES) {
        return expose_local_media(&app, &login, &cache_path);
    }
    let _permit = state
        .video_downloads
        .acquire()
        .await
        .map_err(|_| "视频下载队列已关闭")?;
    if existing_video(&cache_path, MAX_ARCHIVED_VIDEO_BYTES) {
        return expose_local_media(&app, &login, &cache_path);
    }
    // Copy valid legacy cache into the archive; keep the original untouched.
    let legacy = app
        .path()
        .app_cache_dir()
        .map_err(|_| "无法获取旧视频目录")?
        .join("videos")
        .join(format!("{}-{id}.mp4", auth.uin));
    if existing_video(&legacy, MAX_ARCHIVED_VIDEO_BYTES) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = cache_dir.join(format!("{id}-{}-{nonce}.part", std::process::id()));
        let mut staged = TemporaryMedia::create(temporary)?;
        let mut input = fs::File::open(&legacy).map_err(|_| "无法读取旧视频缓存")?;
        use std::io::Read;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = input.read(&mut buffer).map_err(|_| "读取旧视频缓存失败")?;
            if read == 0 {
                break;
            }
            staged.write(&buffer[..read])?;
        }
        staged.commit(&cache_path)?;
        return expose_local_media(&app, &login, &cache_path);
    }
    let candidates = video_urls(video_json);
    if candidates.is_empty() {
        return Err("该归档没有可用的视频地址".into());
    }
    let mut last_error = String::new();
    let mut rejected = false;
    if state.media_paused.load(Ordering::Relaxed) {
        return Err("HTTP 429：媒体下载已暂停，请稍后重新登录确认后再试".into());
    }
    for (attempt, url) in candidates.into_iter().take(8).enumerate() {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let response = match request_qq_media(
            &url,
            &auth.user_agent,
            "video/mp4,video/*;q=0.9,application/octet-stream;q=0.8,*/*;q=0.5",
            std::time::Duration::from_secs(180),
        )
        .await
        {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                stop_on_media_limit(response.status(), &state, &login)?;
                rejected |= response.status() == reqwest::StatusCode::FORBIDDEN;
                last_error = format!("HTTP {}", response.status());
                continue;
            }
            Err(error) => {
                last_error = error;
                continue;
            }
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = cache_dir.join(format!("{}-{id}-{nonce}.part", auth.uin));
        let mut staged = TemporaryMedia::create(temporary)?;
        let (received, prefix) =
            match write_limited_response(response, &mut staged, MAX_ARCHIVED_VIDEO_BYTES).await {
                Ok(result) => result,
                Err(error) => {
                    last_error = error;
                    continue;
                }
            };
        if !valid_mp4(&prefix, received) {
            last_error = "QQ 返回了非视频内容或不支持的视频格式".into();
            continue;
        }
        staged.commit(&cache_path)?;
        return expose_local_media(&app, &login, &cache_path);
    }
    if rejected {
        Err("QQ 拒绝了视频请求（HTTP 403），该归档的视频临时签名可能已经过期，请重新归档以更新视频地址".into())
    } else {
        Err(format!("所有视频地址均加载失败：{last_error}"))
    }
}

fn set_progress(state: &ArchiveState, update: impl FnOnce(&mut ArchiveProgress)) {
    if let Ok(mut progress) = state.progress.lock() {
        update(&mut progress);
    }
}

pub(crate) fn reset_session_progress(app: &tauri::AppHandle) {
    let state = app.state::<ArchiveState>();
    state.media_paused.store(false, Ordering::Relaxed);
    set_progress(&state, |progress| *progress = ArchiveProgress::default());
}

fn stop_on_media_limit(
    status: reqwest::StatusCode,
    state: &ArchiveState,
    login: &QLoginState,
) -> Result<(), String> {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        state.media_paused.store(true, Ordering::Relaxed);
        login.operations.cancel();
        return Err("HTTP 429：媒体下载已暂停，请稍后重新登录确认后再试".into());
    }
    Ok(())
}

fn concise_archive_error(error: &str) -> String {
    let normalized = error.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let summary = chars.by_ref().take(240).collect::<String>();
    if chars.next().is_some() {
        format!("{summary}…")
    } else {
        summary
    }
}

async fn fetch_after_skipped_cursor(
    app: &tauri::AppHandle,
    login: &QLoginState,
    archive: &ArchiveState,
    owner_uin: &str,
    cursor: &str,
    first_advance: i64,
    interval_ms: u64,
) -> Result<(qzone::FeedPage, String, i64), String> {
    let first_advance = first_advance.clamp(1, ARCHIVE_SKIP_MAX_OFFSET_ADVANCE);
    let mut last_error = None;
    let mut last_failed_advance = first_advance.saturating_sub(1);
    let mut best: Option<(qzone::FeedPage, String, i64)> = None;
    for offset_advance in skip_probe_offsets(first_advance) {
        set_progress(archive, |progress| {
            progress.message =
                format!("已记录异常位置，正在尝试从偏移 +{offset_advance} 恢复归档…");
        });
        let candidate = advance_feed_cursor(cursor, offset_advance)?;
        match qzone::fetch_feeds_once(login, "2", Some(&candidate), || {
            reserve_archive_request(app, owner_uin)
        })
        .await
        {
            Ok(page) => {
                best = Some((page, candidate, offset_advance));
                break;
            }
            Err(error) if qzone::feed_error_can_skip(&error) => {
                last_failed_advance = offset_advance;
                last_error = Some(error);
                tokio::time::sleep(std::time::Duration::from_millis(archive_page_delay_ms(
                    interval_ms,
                )))
                .await;
            }
            Err(error) => return Err(error),
        }
    }
    let Some(mut best) = best else {
        return Err(format!(
            "异常位置已保存到待重试列表，但向后探测至偏移 +{} 后仍无法取得下一页：{}",
            ARCHIVE_SKIP_MAX_OFFSET_ADVANCE,
            concise_archive_error(last_error.as_deref().unwrap_or("未知接口错误"))
        ));
    };

    let mut low = last_failed_advance.saturating_add(1);
    let mut high = best.2.saturating_sub(1);
    while low <= high {
        let offset_advance = low + (high - low) / 2;
        set_progress(archive, |progress| {
            progress.message =
                format!("已找到可恢复位置，正在缩小跳过范围（偏移 +{offset_advance}）…");
        });
        let candidate = advance_feed_cursor(cursor, offset_advance)?;
        match qzone::fetch_feeds_once(login, "2", Some(&candidate), || {
            reserve_archive_request(app, owner_uin)
        })
        .await
        {
            Ok(page) => {
                best = (page, candidate, offset_advance);
                high = offset_advance.saturating_sub(1);
            }
            Err(error) if qzone::feed_error_can_skip(&error) => {
                low = offset_advance.saturating_add(1);
            }
            Err(error) => return Err(error),
        }
        tokio::time::sleep(std::time::Duration::from_millis(archive_page_delay_ms(
            interval_ms,
        )))
        .await;
    }
    Ok(best)
}

fn skip_probe_offsets(first_advance: i64) -> Vec<i64> {
    let first_advance = first_advance.clamp(1, ARCHIVE_SKIP_MAX_OFFSET_ADVANCE);
    let mut offsets = vec![first_advance];
    let mut candidate = 1_i64;
    while candidate <= first_advance && candidate < ARCHIVE_SKIP_MAX_OFFSET_ADVANCE {
        candidate = candidate.saturating_mul(2);
    }
    while candidate < ARCHIVE_SKIP_MAX_OFFSET_ADVANCE {
        offsets.push(candidate);
        candidate = candidate.saturating_mul(2);
    }
    if offsets.last().copied() != Some(ARCHIVE_SKIP_MAX_OFFSET_ADVANCE) {
        offsets.push(ARCHIVE_SKIP_MAX_OFFSET_ADVANCE);
    }
    offsets
}

#[tauri::command]
pub async fn start_feed_archive(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    archive: tauri::State<'_, ArchiveState>,
    interval_ms: u64,
) -> Result<ArchiveProgress, String> {
    let mut operation = login.inner().operations.read().await;
    let _work = archive
        .inner()
        .work
        .try_lock()
        .map_err(|_| "已有归档或重试任务正在运行")?;
    let cleanup = TaskCleanup(archive.inner());
    let result = operation
        .run(start_feed_archive_inner(app, login, archive, interval_ms))
        .await;
    cleanup.finish(&result);
    result
}

async fn start_feed_archive_inner(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    archive: tauri::State<'_, ArchiveState>,
    interval_ms: u64,
) -> Result<ArchiveProgress, String> {
    let interval_ms = interval_ms.clamp(2_000, 30_000);
    if archive.batch_retrying.load(Ordering::Relaxed) {
        return Err("正在批量重试异常位置，请等待完成或停止后再开始归档".into());
    }
    {
        let mut progress = archive.progress.lock().map_err(|_| "归档状态锁已损坏")?;
        if progress.status == "running" {
            return Err("已有归档任务正在运行".into());
        }
        *progress = ArchiveProgress {
            status: "running",
            pages: 0,
            fetched: 0,
            saved: 0,
            skipped: 0,
            message: "正在准备归档…".into(),
            retry_at: None,
            batch_retry: None,
        };
    }
    archive.cancel.store(false, Ordering::Relaxed);
    let owner_uin = login.qzone_auth().await?.uin;
    let saved_skip_count = unresolved_skip_count(&app, &owner_uin)?;
    set_progress(&archive, |progress| progress.skipped = saved_skip_count);
    let checkpoint = load_checkpoint(&app, &owner_uin)?;
    let stale_checkpoint = checkpoint
        .as_ref()
        .is_some_and(|value| checkpoint_is_stale(value, now()));
    let mut reset_checkpoint_stats = stale_checkpoint;
    let mut cursor = checkpoint
        .as_ref()
        .filter(|_| !stale_checkpoint)
        .map(|value| value.cursor.clone());
    let mut seen_cursors = HashSet::new();
    if stale_checkpoint {
        set_progress(&archive, |progress| {
            progress.message =
                "上次分页位置已超过 10 分钟，正在从第一页重新校验；已保存记录会自动去重。".into();
        });
    } else if let Some(checkpoint) = checkpoint.as_ref() {
        let saved_cursor = &checkpoint.cursor;
        seen_cursors.insert(saved_cursor.clone());
        set_progress(&archive, |progress| {
            progress.pages = checkpoint.pages;
            progress.fetched = checkpoint.fetched;
            progress.saved = checkpoint.saved;
            progress.message = format!("已恢复上次进度：{} 页，正在继续归档…", checkpoint.pages);
        });
    }
    let result: Result<(), String> = async {
        loop {
            if archive.cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let mut skipped_page: Option<(String, String, FeedCursorDetails, i64, String)> = None;
            let page = if let Some(current_cursor) = cursor.as_deref() {
                let known_skip = match parse_feed_cursor(current_cursor) {
                    Ok(details) => {
                        known_skip_advance(&app, &owner_uin, details)?.map(|known| (details, known))
                    }
                    Err(_) => None,
                };
                if let Some((details, (known_advance, known_error))) = known_skip {
                    let (page, resume_cursor, offset_advance) = fetch_after_skipped_cursor(
                        &app,
                        &login,
                        &archive,
                        &owner_uin,
                        current_cursor,
                        known_advance,
                        interval_ms,
                    )
                    .await?;
                    skipped_page = Some((
                        current_cursor.to_owned(),
                        resume_cursor,
                        details,
                        offset_advance,
                        known_error,
                    ));
                    page
                } else {
                    match qzone::fetch_feeds(&login, "2", Some(current_cursor), || reserve_archive_request(&app, &owner_uin)).await {
                        Ok(page) => page,
                        Err(error) if qzone::feed_error_can_skip(&error) => {
                            let details =
                                parse_feed_cursor(current_cursor).map_err(|cursor_error| {
                                    format!("{error}；且无法自动跳过该页：{cursor_error}")
                                })?;
                            let page_number = archive
                                .progress
                                .lock()
                                .map_err(|_| "归档状态锁已损坏")?
                                .pages
                                .saturating_add(1);
                            record_archive_skip(
                                &app,
                                &owner_uin,
                                SkipRecord {
                                    cursor: current_cursor,
                                    resume_cursor: current_cursor,
                                    page_number,
                                    details,
                                    offset_advance: 0,
                                    error: &error,
                                },
                            )?;
                            let skip_count = unresolved_skip_count(&app, &owner_uin)?;
                            set_progress(&archive, |progress| {
                                progress.skipped = skip_count;
                                progress.message = format!(
                                    "第 {page_number} 页发生异常，已加入待重试列表，正在寻找后续可恢复位置…"
                                );
                            });
                            let (page, resume_cursor, offset_advance) = fetch_after_skipped_cursor(
                                &app,
                                &login,
                                &archive,
                                &owner_uin,
                                current_cursor,
                                1,
                                interval_ms,
                            )
                            .await?;
                            skipped_page = Some((
                                current_cursor.to_owned(),
                                resume_cursor,
                                details,
                                offset_advance,
                                error,
                            ));
                            page
                        }
                        Err(error) => return Err(error),
                    }
                }
            } else {
                qzone::fetch_feeds(&login, "1", None, || reserve_archive_request(&app, &owner_uin)).await?
            };
            let fetched = page.feeds.len() as u64;
            let next = if page.has_more {
                Some(
                    page.attach_info
                        .as_deref()
                        .ok_or("接口表示还有数据，但未返回分页游标")?,
                )
            } else {
                None
            };
            if let Some(next_cursor) = next {
                if !seen_cursors.insert(next_cursor.to_owned()) {
                    return Err("检测到重复分页游标，已停止以避免死循环".into());
                }
            }
            let did_skip = skipped_page.is_some();
            if let Some((failed_cursor, resume_cursor, details, offset_advance, error)) =
                skipped_page.as_ref()
            {
                let page_number = archive
                    .progress
                    .lock()
                    .map_err(|_| "归档状态锁已损坏")?
                    .pages
                    .saturating_add(1);
                record_archive_skip(
                    &app,
                    &owner_uin,
                    SkipRecord {
                        cursor: failed_cursor,
                        resume_cursor,
                        page_number,
                        details: *details,
                        offset_advance: *offset_advance,
                        error,
                    },
                )?;
            }
            let saved = save_page(&app, &owner_uin, &page.feeds, next, reset_checkpoint_stats)?;
            reset_checkpoint_stats = false;
            let skip_count = unresolved_skip_count(&app, &owner_uin)?;
            set_progress(&archive, |progress| {
                progress.pages += 1;
                progress.fetched += fetched;
                progress.saved += saved;
                progress.skipped = skip_count;
                progress.message = if did_skip {
                    format!(
                        "已跳过 1 个异常位置并继续归档；当前 {} 页，共 {} 条记录",
                        progress.pages, progress.fetched
                    )
                } else {
                    format!(
                        "已归档 {} 页，共 {} 条记录",
                        progress.pages, progress.fetched
                    )
                };
            });
            if !page.has_more {
                return Ok(());
            }
            cursor = next.map(str::to_owned);
            tokio::time::sleep(std::time::Duration::from_millis(archive_page_delay_ms(
                interval_ms,
            )))
            .await;
        }
    }
    .await;
    match &result {
        Ok(()) if archive.cancel.load(Ordering::Relaxed) => set_progress(&archive, |p| {
            p.status = "cancelled";
            p.message = "归档已取消".into();
            p.retry_at = None;
        }),
        Ok(()) => set_progress(&archive, |p| {
            p.status = "completed";
            p.message = if p.skipped > 0 {
                format!(
                    "归档完成，共保存 {} 条记录；另有 {} 个异常位置已跳过，可在下方单独重试",
                    p.saved, p.skipped
                )
            } else {
                format!("归档完成，共保存 {} 条记录", p.saved)
            };
            p.retry_at = None;
        }),
        Err(error) if error.starts_with("ARCHIVE_RATE_LIMIT:") => set_progress(&archive, |p| {
            let retry_at = error
                .trim_start_matches("ARCHIVE_RATE_LIMIT:")
                .parse::<i64>()
                .ok();
            p.status = "limited";
            p.retry_at = retry_at;
            p.message = "为防止接口请求过于频繁，每 10 分钟最多发送 300 次动态请求（含重试）。达到限制后已安全暂停，倒计时结束即可从当前进度继续归档。".into();
        }),
        Err(_error) => set_progress(&archive, |p| {
            eprintln!("[QzoneArchive] 归档任务失败，详细信息仅显示在应用界面");
            p.status = "error";
            p.message = format!("归档失败：{}", concise_archive_error(_error));
            p.retry_at = None;
        }),
    }
    let progress = archive
        .progress
        .lock()
        .map_err(|_| "归档状态锁已损坏")?
        .clone();
    if result
        .as_ref()
        .is_err_and(|error| error.starts_with("ARCHIVE_RATE_LIMIT:"))
    {
        Ok(progress)
    } else {
        result.map(|_| progress)
    }
}

#[tauri::command]
pub fn get_archive_progress(
    state: tauri::State<'_, ArchiveState>,
) -> Result<ArchiveProgress, String> {
    state
        .progress
        .lock()
        .map(|value| value.clone())
        .map_err(|_| "归档状态锁已损坏".into())
}

#[tauri::command]
pub async fn list_archive_skips(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
) -> Result<Vec<ArchiveSkipItem>, String> {
    let _operation = login.operations.read().await;
    let owner_uin = login.qzone_auth().await?.uin;
    let connection = open_database(&app)?;
    let mut statement = connection
        .prepare(
            "SELECT id,page_number,cursor_offset,offset_advance,base_time,error,skipped_at,
                    retry_count,last_retry_at,resolved_at,recovered_records
             FROM archive_skips WHERE owner_uin=?1
             ORDER BY resolved_at IS NOT NULL, skipped_at DESC",
        )
        .map_err(|error| format!("读取异常跳过列表失败：{error}"))?;
    let rows = statement
        .query_map(params![owner_uin], |row| {
            Ok(ArchiveSkipItem {
                id: row.get(0)?,
                page_number: row.get(1)?,
                cursor_offset: row.get(2)?,
                offset_advance: row.get(3)?,
                base_time: row.get(4)?,
                error: row.get(5)?,
                skipped_at: row.get(6)?,
                retry_count: row.get(7)?,
                last_retry_at: row.get(8)?,
                resolved_at: row.get(9)?,
                recovered_records: row.get(10)?,
            })
        })
        .map_err(|error| format!("查询异常跳过列表失败：{error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("解析异常跳过列表失败：{error}"))
}

#[tauri::command]
pub async fn clear_resolved_archive_skips(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
) -> Result<u64, String> {
    let _operation = login.operations.read().await;
    let owner_uin = login.qzone_auth().await?.uin;
    let connection = open_database(&app)?;
    let removed = connection
        .execute(
            "DELETE FROM archive_skips WHERE owner_uin=?1 AND resolved_at IS NOT NULL",
            params![owner_uin],
        )
        .map_err(|error| format!("清理已恢复的异常跳过记录失败：{error}"))?;
    Ok(removed as u64)
}

#[tauri::command]
pub async fn retry_archive_skip(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    archive: tauri::State<'_, ArchiveState>,
    id: i64,
) -> Result<ArchiveSkipRetryResult, String> {
    let mut operation = login.inner().operations.read().await;
    let _work = archive
        .inner()
        .work
        .try_lock()
        .map_err(|_| "已有归档或重试任务正在运行")?;
    let result = operation
        .run(retry_archive_skip_inner(app, login, archive, id))
        .await;
    result
}

async fn retry_archive_skip_inner(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    archive: tauri::State<'_, ArchiveState>,
    id: i64,
) -> Result<ArchiveSkipRetryResult, String> {
    let owner_uin = login.qzone_auth().await?.uin;
    retry_single_skip(&app, &login, &archive, &owner_uin, id).await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveSkipBatchRetryResult {
    total: u32,
    recovered: u32,
    failed: u32,
    recovered_records: u64,
}

#[tauri::command]
pub async fn retry_all_archive_skips(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    archive: tauri::State<'_, ArchiveState>,
    interval_ms: u64,
) -> Result<ArchiveSkipBatchRetryResult, String> {
    let mut operation = login.inner().operations.read().await;
    let _work = archive
        .inner()
        .work
        .try_lock()
        .map_err(|_| "已有归档或重试任务正在运行")?;
    let cleanup = TaskCleanup(archive.inner());
    let result = operation
        .run(retry_all_archive_skips_inner(
            app,
            login,
            archive,
            interval_ms,
        ))
        .await;
    cleanup.finish(&result);
    result
}

async fn retry_all_archive_skips_inner(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    archive: tauri::State<'_, ArchiveState>,
    interval_ms: u64,
) -> Result<ArchiveSkipBatchRetryResult, String> {
    let owner_uin = login.qzone_auth().await?.uin;
    ensure_archive_idle(&archive)?;
    if archive
        .batch_retrying
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return Err("已有批量重试在进行中".into());
    }
    let pending_ids = {
        let connection = open_database(&app)?;
        let mut statement = connection
            .prepare(
                "SELECT id FROM archive_skips WHERE owner_uin=?1 AND resolved_at IS NULL
                 ORDER BY skipped_at ASC",
            )
            .map_err(|error| format!("读取异常跳过列表失败：{error}"))?;
        let rows = statement
            .query_map(params![owner_uin], |row| row.get(0))
            .map_err(|error| format!("查询异常跳过列表失败：{error}"))?;
        rows.collect::<Result<Vec<i64>, _>>()
            .map_err(|error| format!("解析异常跳过列表失败：{error}"))?
    };
    let total = pending_ids.len() as u32;
    let mut result = ArchiveSkipBatchRetryResult {
        total,
        recovered: 0,
        failed: 0,
        recovered_records: 0,
    };
    archive.batch_cancel.store(false, Ordering::Relaxed);
    for (index, id) in pending_ids.into_iter().enumerate() {
        if archive.batch_cancel.load(Ordering::Relaxed) {
            break;
        }
        set_progress(&archive, |progress| {
            progress.batch_retry = Some(BatchRetryProgress {
                current: index as u32 + 1,
                total,
                recovered: result.recovered,
                failed: result.failed,
                recovered_records: result.recovered_records,
            });
        });
        match retry_single_skip(&app, &login, &archive, &owner_uin, id).await {
            Ok(outcome) => {
                if outcome.success {
                    result.recovered += 1;
                    result.recovered_records += outcome.recovered_records;
                } else {
                    result.failed += 1;
                }
                set_progress(&archive, |progress| {
                    progress.batch_retry = Some(BatchRetryProgress {
                        current: index as u32 + 1,
                        total,
                        recovered: result.recovered,
                        failed: result.failed,
                        recovered_records: result.recovered_records,
                    });
                });
            }
            Err(error) => {
                if error.starts_with("请求频率保护中") || error.starts_with("归档任务运行中")
                {
                    break;
                }
                if error.starts_with("ARCHIVE_RATE_LIMIT:")
                    || error.starts_with("QQ_ACCOUNT_STOP:")
                    || error.contains("HTTP 429")
                    || error.contains("HTTP 403")
                    || error.contains("HTTP 401")
                {
                    return Err(error);
                }
                result.failed += 1;
                set_progress(&archive, |progress| {
                    progress.batch_retry = Some(BatchRetryProgress {
                        current: index as u32 + 1,
                        total,
                        recovered: result.recovered,
                        failed: result.failed,
                        recovered_records: result.recovered_records,
                    });
                });
            }
        }
        // 与归档任务保持一致的节奏，避免批量重试触发频率保护；
        // 分片睡眠让"停止重试"能在当前请求结束后立即生效
        let mut remaining_delay = archive_page_delay_ms(interval_ms);
        while remaining_delay > 0 {
            if archive.batch_cancel.load(Ordering::Relaxed) {
                break;
            }
            let slice = remaining_delay.min(200);
            tokio::time::sleep(std::time::Duration::from_millis(slice)).await;
            remaining_delay -= slice;
        }
    }
    archive.batch_retrying.store(false, Ordering::Relaxed);
    archive.batch_cancel.store(false, Ordering::Relaxed);
    set_progress(&archive, |progress| {
        progress.batch_retry = None;
    });
    Ok(result)
}

async fn retry_single_skip(
    app: &tauri::AppHandle,
    login: &tauri::State<'_, QLoginState>,
    archive: &tauri::State<'_, ArchiveState>,
    owner_uin: &str,
    id: i64,
) -> Result<ArchiveSkipRetryResult, String> {
    let connection = open_database(app)?;
    let (cursor, resolved_at) = connection
        .query_row(
            "SELECT cursor,resolved_at FROM archive_skips WHERE id=?1 AND owner_uin=?2",
            params![id, owner_uin],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
        )
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => "找不到这条异常跳过记录".into(),
            _ => format!("读取异常跳过记录失败：{error}"),
        })?;
    if resolved_at.is_some() {
        return Ok(ArchiveSkipRetryResult {
            success: true,
            message: "该异常位置已经重试成功".into(),
            recovered_records: 0,
        });
    }
    let attempted_at = now();
    match qzone::fetch_feeds(login, "2", Some(&cursor), || {
        reserve_archive_request(app, owner_uin)
    })
    .await
    {
        Ok(page) => {
            let recovered_records = page.feeds.len() as u64;
            save_retried_page(app, owner_uin, &page.feeds)?;
            let connection = open_database(app)?;
            connection
                .execute(
                    "UPDATE archive_skips SET retry_count=retry_count+1,last_retry_at=?2,
                  resolved_at=?2,recovered_records=?3 WHERE id=?1 AND owner_uin=?4",
                    params![id, attempted_at, recovered_records, owner_uin],
                )
                .map_err(|error| format!("更新异常重试结果失败：{error}"))?;
            let remaining = unresolved_skip_count(app, owner_uin)?;
            set_progress(archive, |progress| progress.skipped = remaining);
            Ok(ArchiveSkipRetryResult {
                success: true,
                message: format!("重试成功，已恢复 {recovered_records} 条接口记录"),
                recovered_records,
            })
        }
        Err(error) => {
            let summary = concise_archive_error(&error);
            if error.starts_with("ARCHIVE_RATE_LIMIT:")
                || error.starts_with("QQ_ACCOUNT_STOP:")
                || error.contains("HTTP 429")
                || error.contains("HTTP 403")
                || error.contains("HTTP 401")
            {
                return Err(error);
            }
            let connection = open_database(app)?;
            connection
                .execute(
                    "UPDATE archive_skips SET retry_count=retry_count+1,last_retry_at=?2,error=?3
                 WHERE id=?1 AND owner_uin=?4",
                    params![id, attempted_at, summary, owner_uin],
                )
                .map_err(|reason| format!("保存异常重试失败结果失败：{reason}"))?;
            Ok(ArchiveSkipRetryResult {
                success: false,
                message: format!("重试仍然失败：{summary}"),
                recovered_records: 0,
            })
        }
    }
}

#[tauri::command]
pub fn cancel_feed_archive(
    state: tauri::State<'_, ArchiveState>,
    login: tauri::State<'_, QLoginState>,
) {
    login.operations.cancel();
    state.cancel.store(true, Ordering::Relaxed);
    state.batch_cancel.store(true, Ordering::Relaxed);
}

#[tauri::command]
pub async fn list_archived_feeds(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    limit: u32,
    offset: u32,
    category: String,
) -> Result<Vec<ArchiveItem>, String> {
    let _operation = login.operations.read().await;
    validate_category(&category)?;
    let owner_uin = login.qzone_auth().await?.uin;
    tauri::async_runtime::spawn_blocking(move || {
    let connection = open_database(&app)?;
    let mut statement = connection
        .prepare(
            "SELECT d.id,d.owner_uin,d.cell_id,d.published_at,d.content,d.author_uin,d.author_name,d.pictures_json,d.video_json,
              (SELECT COUNT(*) FROM archive_feeds f WHERE f.owner_uin=d.owner_uin AND f.cell_id=d.cell_id AND f.event_type=217),
              (SELECT COUNT(*) FROM archive_feeds f WHERE f.owner_uin=d.owner_uin AND f.cell_id=d.cell_id AND f.event_type IN (2,311))
             FROM archive_dynamics d WHERE d.owner_uin=?1 AND d.category=?2 ORDER BY d.published_at ASC LIMIT ?3 OFFSET ?4",
        )
        .map_err(|error| format!("读取归档失败：{error}"))?;
    let rows = statement
        .query_map(
            params![owner_uin, category, limit.clamp(1, 200), offset],
            |row| {
                let id = row.get(0)?;
                let video_json = row.get::<_, Option<String>>(8)?;
                let (picture_urls, video_url, video_urls, video_cover_url) =
                    public_media_references(id, row.get(7)?, video_json);
                Ok(ArchiveItem {
                    id,
                    owner_uin: row.get(1)?,
                    cell_id: row.get(2)?,
                    published_at: row.get(3)?,
                    content: row.get(4)?,
                    content_recovered: false,
                    is_blog: false,
                    can_open_original: false,
                    author_uin: row.get(5)?,
                    author_name: row.get(6)?,
                    picture_urls,
                    video_url,
                    video_urls,
                    video_cover_url,
                    like_count: row.get(9)?,
                    comment_count: row.get(10)?,
                    likes: vec![],
                    comments: vec![],
                })
            },
        )
        .map_err(|error| format!("查询归档失败：{error}"))?;
    let mut items = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取归档记录失败：{error}"))?;
    drop(statement);
    let mut comment_statement = connection
        .prepare(
            "SELECT comments_json,actor_uin,actor_name,event_summary,event_time FROM archive_feeds
         WHERE owner_uin=?1 AND cell_id=?2 AND event_type IN (2,311) ORDER BY event_time ASC",
        )
        .map_err(|error| format!("准备评论查询失败：{error}"))?;
    for item in &mut items {
        recover_observed_text(&connection, item)?;
        let comments = comment_statement
            .query_map(params![item.owner_uin, item.cell_id], |row| {
                Ok(comment_from_values(
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .map_err(|error| format!("查询动态评论失败：{error}"))?;
        item.comments = merge_comments(comments.filter_map(Result::ok));
    }
    drop(comment_statement);
    let mut like_statement = connection
        .prepare(
            "SELECT actor_uin,actor_name FROM archive_feeds
         WHERE owner_uin=?1 AND cell_id=?2 AND event_type=217 ORDER BY event_time ASC",
        )
        .map_err(|error| format!("准备点赞查询失败：{error}"))?;
    for item in &mut items {
        let likes = like_statement
            .query_map(params![item.owner_uin, item.cell_id], |row| {
                Ok(LikeUser {
                    uin: row.get(0)?,
                    nickname: row.get(1)?,
                })
            })
            .map_err(|error| format!("查询点赞用户失败：{error}"))?;
        item.likes = likes.filter_map(Result::ok).collect();
    }
    Ok(items)
    })
    .await
    .map_err(|error| format!("归档查询任务异常退出：{error}"))?
}

#[tauri::command]
pub async fn list_archived_media(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    limit: u32,
    offset: u32,
    year: Option<i32>,
) -> Result<ArchiveMediaPage, String> {
    let _operation = login.operations.read().await;
    let owner_uin = login.qzone_auth().await?.uin;
    let connection = open_database(&app)?;
    let mut year_statement = connection.prepare(
        "SELECT DISTINCT CAST(strftime('%Y',published_at,'unixepoch','localtime') AS INTEGER) FROM archive_dynamics
         WHERE owner_uin=?1 AND category IN ('self','other') AND (pictures_json IS NOT NULL OR video_json IS NOT NULL)
         ORDER BY 1 DESC",
    ).map_err(|error| format!("读取媒体年份失败：{error}"))?;
    let years = year_statement
        .query_map(params![owner_uin], |row| row.get::<_, i32>(0))
        .map_err(|error| format!("查询媒体年份失败：{error}"))?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    drop(year_statement);

    let mut statement = connection.prepare(
        "SELECT id,published_at,content,author_uin,author_name,pictures_json,video_json FROM archive_dynamics
         WHERE owner_uin=?1 AND category IN ('self','other')
           AND (pictures_json IS NOT NULL OR video_json IS NOT NULL)
           AND (?2 IS NULL OR CAST(strftime('%Y',published_at,'unixepoch','localtime') AS INTEGER)=?2)
         ORDER BY published_at ASC,id ASC",
    ).map_err(|error| format!("读取媒体归档失败：{error}"))?;
    let rows = statement
        .query_map(params![owner_uin, year], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })
        .map_err(|error| format!("查询媒体归档失败：{error}"))?;
    let mut all = Vec::new();
    for row in rows {
        let (id, published_at, content, author_uin, author_name, pictures_json, video_json) =
            row.map_err(|error| format!("读取媒体记录失败：{error}"))?;
        for index in 0..picture_urls(pictures_json).len() {
            let key = format!("{id}-photo-{index}");
            all.push(ArchiveMediaItem {
                key: key.clone(),
                dynamic_id: id,
                media_type: "photo",
                picture_index: Some(index),
                url: key,
                cover_url: None,
                published_at,
                author_uin: author_uin.clone(),
                author_name: author_name.clone(),
                content: content.clone(),
            });
        }
        if !video_urls(video_json).is_empty() {
            let key = format!("{id}-video");
            all.push(ArchiveMediaItem {
                key: key.clone(),
                dynamic_id: id,
                media_type: "video",
                picture_index: None,
                url: key,
                cover_url: None,
                published_at,
                author_uin,
                author_name,
                content,
            });
        }
    }
    let total = all.len();
    let start = (offset as usize).min(total);
    let end = (start + limit.clamp(1, 100) as usize).min(total);
    let items = all.drain(start..end).collect();
    Ok(ArchiveMediaPage {
        items,
        total,
        years,
    })
}

fn longer_observed_text(
    db: &Connection,
    owner: &str,
    cell: &str,
    content: &str,
) -> Result<Option<String>, String> {
    let trimmed = content.trim_end();
    if !trimmed.ends_with("...") && !trimmed.ends_with('…') {
        return Ok(None);
    }
    let prefix = trimmed.trim_end_matches(['.', '…']);
    let prefix_chars = prefix.chars().count();
    if prefix_chars < 8 {
        return Ok(None);
    }
    use rusqlite::OptionalExtension;
    db.query_row(
        "SELECT content FROM archive_feeds WHERE owner_uin=?1 AND cell_id=?2
         AND length(content)>?3 AND length(content)<=262144 AND substr(content,1,?4)=?5
         ORDER BY length(content) DESC LIMIT 1",
        params![owner, cell, content.chars().count(), prefix_chars, prefix],
        |row| row.get(0),
    )
    .optional()
    .map_err(|_| "读取已保存的正文片段失败".to_owned())
}

fn recover_observed_text(db: &Connection, item: &mut ArchiveItem) -> Result<(), String> {
    // Invalid legacy source metadata must not hide otherwise readable text.
    // The explicit open command still rejects missing or invalid metadata.
    if let Ok(raw) = archived_original(db, &item.owner_uin, item.id) {
        item.is_blog = raw.pointer("/cell_comm/appid").and_then(Value::as_u64) == Some(2);
        item.can_open_original = crate::content_source::original_blog_url(&raw).is_some();
    }
    if let Some(content) = item.content.as_deref() {
        if let Some(longer) = longer_observed_text(db, &item.owner_uin, &item.cell_id, content)? {
            item.content = Some(longer);
            item.content_recovered = true;
        }
    }
    Ok(())
}

fn archived_original(db: &Connection, owner: &str, id: i64) -> Result<Value, String> {
    let text: String = db
        .query_row(
            "SELECT raw_original_json FROM archive_dynamics WHERE owner_uin=?1 AND id=?2",
            params![owner, id],
            |row| row.get(0),
        )
        .map_err(|_| "当前账号中不存在这条原始记录".to_owned())?;
    serde_json::from_str(&text).map_err(|_| "原始记录格式无效".to_owned())
}

#[tauri::command]
pub async fn open_archived_original(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    id: i64,
) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let _operation = login.operations.read().await;
    let owner = login.qzone_auth().await?.uin;
    let raw = archived_original(&open_database(&app)?, &owner, id)?;
    let url =
        crate::content_source::original_blog_url(&raw).ok_or("该记录没有经过验证的官方日志链接")?;
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|_| "无法打开官方原文页面".to_owned())
}

#[tauri::command]
pub async fn get_archived_feed(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    id: i64,
) -> Result<ArchiveItem, String> {
    let _operation = login.operations.read().await;
    let owner_uin = login.qzone_auth().await?.uin;
    let connection = open_database(&app)?;
    let mut item = connection.query_row(
        "SELECT d.id,d.owner_uin,d.cell_id,d.published_at,d.content,d.author_uin,d.author_name,d.pictures_json,d.video_json,
          (SELECT COUNT(*) FROM archive_feeds f WHERE f.owner_uin=d.owner_uin AND f.cell_id=d.cell_id AND f.event_type=217),
          (SELECT COUNT(*) FROM archive_feeds f WHERE f.owner_uin=d.owner_uin AND f.cell_id=d.cell_id AND f.event_type IN (2,311))
         FROM archive_dynamics d WHERE d.owner_uin=?1 AND d.id=?2",
        params![owner_uin, id], |row| {
            let id = row.get(0)?;
            let video_json = row.get::<_, Option<String>>(8)?;
            let (picture_urls, video_url, video_urls, video_cover_url) =
                public_media_references(id, row.get(7)?, video_json);
            Ok(ArchiveItem { id, owner_uin: row.get(1)?, cell_id: row.get(2)?, published_at: row.get(3)?,
                content: row.get(4)?, content_recovered: false, is_blog: false, can_open_original: false, author_uin: row.get(5)?, author_name: row.get(6)?, picture_urls,
                video_url, video_urls, video_cover_url,
                like_count: row.get(9)?, comment_count: row.get(10)?, likes: vec![], comments: vec![] })
        },
    ).map_err(|error| match error { rusqlite::Error::QueryReturnedNoRows => "原始动态不存在或已删除".into(), _ => format!("读取原始动态失败：{error}") })?;
    recover_observed_text(&connection, &mut item)?;
    let mut comments = connection
        .prepare(
            "SELECT comments_json,actor_uin,actor_name,event_summary,event_time FROM archive_feeds
         WHERE owner_uin=?1 AND cell_id=?2 AND event_type IN (2,311) ORDER BY event_time ASC",
        )
        .map_err(|error| format!("准备评论查询失败：{error}"))?;
    item.comments = merge_comments(
        comments
            .query_map(params![item.owner_uin, item.cell_id], |row| {
                Ok(comment_from_values(
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .map_err(|error| format!("查询动态评论失败：{error}"))?
            .filter_map(Result::ok),
    );
    drop(comments);
    let mut likes_stmt = connection
        .prepare(
            "SELECT actor_uin,actor_name FROM archive_feeds
         WHERE owner_uin=?1 AND cell_id=?2 AND event_type=217 ORDER BY event_time ASC",
        )
        .map_err(|error| format!("准备点赞查询失败：{error}"))?;
    item.likes = likes_stmt
        .query_map(params![item.owner_uin, item.cell_id], |row| {
            Ok(LikeUser {
                uin: row.get(0)?,
                nickname: row.get(1)?,
            })
        })
        .map_err(|error| format!("查询点赞用户失败：{error}"))?
        .filter_map(Result::ok)
        .collect();
    Ok(item)
}

fn comment_from_values(
    json: Option<String>,
    fallback_uin: Option<String>,
    fallback_name: Option<String>,
    fallback_content: Option<String>,
    fallback_time: i64,
) -> ArchiveComment {
    let value = json.and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let main = value.as_ref().and_then(|value| value.get("main_comment"));
    let comment_id = main.and_then(|value| text_at(value, "/commentid"));
    let main_uin = main.and_then(|value| text_at(value, "/user/uin"));
    let main_name = main.and_then(|value| text_at(value, "/user/nickname"));
    let main_content = main.and_then(|value| text_at(value, "/content"));
    let main_time = main
        .and_then(|value| value.get("date"))
        .and_then(Value::as_i64)
        .unwrap_or(fallback_time);
    let mut replies: Vec<ArchiveReply> = main
        .and_then(|value| value.get("replys"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(reply_from_value)
        .collect();
    if let Some(comment_id) = comment_id.as_deref() {
        let related_replies = value
            .as_ref()
            .and_then(|value| value.get("comments"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|comment| text_at(comment, "/commentid").as_deref() == Some(comment_id))
            .filter_map(|comment| comment.get("replys"))
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(reply_from_value);
        for reply in related_replies {
            let duplicate = replies.iter().any(|candidate| {
                candidate.uin == reply.uin
                    && candidate.content == reply.content
                    && candidate.created_at == reply.created_at
            });
            if !duplicate {
                replies.push(reply);
            }
        }
    }

    // Reply notifications keep the parent in main_comment but put the actual
    // reply text and author at the feed level. When the parent author replies
    // again, target the latest preceding reply from the other participant.
    let is_reply_notification = main
        .and_then(|value| value.get("replynum"))
        .and_then(Value::as_i64)
        .is_some_and(|count| count > 0)
        && main_uin.is_some()
        && fallback_uin.is_some()
        && main_content.as_deref() != fallback_content.as_deref()
        && fallback_time > main_time;
    if is_reply_notification {
        if let Some(content) = fallback_content.clone() {
            let duplicate = replies
                .iter()
                .any(|reply| reply.uin == fallback_uin && reply.content == content);
            if !duplicate {
                let reply_target = replies
                    .iter()
                    .filter(|reply| reply.uin != fallback_uin && reply.created_at <= fallback_time)
                    .max_by_key(|reply| reply.created_at);
                replies.push(ArchiveReply {
                    uin: fallback_uin.clone(),
                    nickname: fallback_name.clone(),
                    reply_to_uin: reply_target
                        .and_then(|reply| reply.uin.clone())
                        .or_else(|| main_uin.clone()),
                    reply_to_nickname: reply_target
                        .and_then(|reply| reply.nickname.clone())
                        .or_else(|| main_name.clone()),
                    content,
                    created_at: fallback_time,
                });
            }
        }
    }

    ArchiveComment {
        comment_id,
        uin: main_uin.or(fallback_uin),
        nickname: main_name.or(fallback_name),
        content: main_content
            .or(fallback_content)
            .unwrap_or_else(|| "评论了这条动态".into()),
        created_at: main_time,
        replies,
    }
}

fn reply_from_value(value: &Value) -> Option<ArchiveReply> {
    let content = text_at(value, "/content")?;
    Some(ArchiveReply {
        uin: text_at(value, "/user/uin").or_else(|| text_at(value, "/replyuser/uin")),
        nickname: text_at(value, "/user/nickname")
            .or_else(|| text_at(value, "/replyuser/nickname")),
        reply_to_uin: text_at(value, "/replyuser/uin")
            .or_else(|| text_at(value, "/targetuser/uin"))
            .or_else(|| text_at(value, "/target/uin")),
        reply_to_nickname: text_at(value, "/replyuser/nickname")
            .or_else(|| text_at(value, "/targetuser/nickname"))
            .or_else(|| text_at(value, "/target/nickname")),
        content,
        created_at: value.get("date").and_then(Value::as_i64).unwrap_or(0),
    })
}

fn merge_comments(comments: impl IntoIterator<Item = ArchiveComment>) -> Vec<ArchiveComment> {
    let mut merged: Vec<ArchiveComment> = Vec::new();
    for mut comment in comments {
        let existing = merged.iter_mut().find(|candidate| {
            (comment.comment_id.is_some() && candidate.comment_id == comment.comment_id)
                || (candidate.uin == comment.uin
                    && candidate.content == comment.content
                    && candidate.created_at == comment.created_at)
        });
        if let Some(existing) = existing {
            for reply in comment.replies.drain(..) {
                let duplicate = existing.replies.iter().any(|candidate| {
                    candidate.uin == reply.uin
                        && candidate.content == reply.content
                        && candidate.created_at == reply.created_at
                });
                if !duplicate {
                    existing.replies.push(reply);
                }
            }
            existing.replies.sort_by_key(|reply| reply.created_at);
        } else {
            comment.replies.sort_by_key(|reply| reply.created_at);
            merged.push(comment);
        }
    }
    merged
}

fn validate_category(category: &str) -> Result<(), String> {
    match category {
        "self" | "other" | "guestbook" => Ok(()),
        _ => Err("无效的归档分类".into()),
    }
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn qzone_text_html(value: Option<&str>) -> String {
    let text = value
        .unwrap_or("")
        .trim_start_matches(['：', ':'])
        .trim_start();
    let pattern =
        regex::Regex::new(r"@\{uin:([^,}]+),nick:([^}]+)\}").expect("fixed mention regex");
    let mut html = String::new();
    let mut cursor = 0;
    for captures in pattern.captures_iter(text) {
        let matched = captures.get(0).expect("full capture");
        html.push_str(&html_escape(&text[cursor..matched.start()]));
        html.push_str("<span class=\"mention\" title=\"QQ ");
        html.push_str(&html_escape(&captures[1]));
        html.push_str("\">@");
        html.push_str(&html_escape(&captures[2]));
        html.push_str("</span>");
        cursor = matched.end();
    }
    html.push_str(&html_escape(&text[cursor..]));
    if html.is_empty() {
        "<span class=\"muted\">该动态没有文字内容</span>".into()
    } else {
        html
    }
}

fn archive_items_for_export(
    connection: &Connection,
    owner_uin: &str,
    category: &str,
    selected_ids: Option<&HashSet<i64>>,
) -> Result<Vec<ArchiveItem>, String> {
    let mut statement = connection.prepare(
        "SELECT d.id,d.owner_uin,d.cell_id,d.published_at,d.content,d.author_uin,d.author_name,d.pictures_json,d.video_json,
          (SELECT COUNT(*) FROM archive_feeds f WHERE f.owner_uin=d.owner_uin AND f.cell_id=d.cell_id AND f.event_type=217),
          (SELECT COUNT(*) FROM archive_feeds f WHERE f.owner_uin=d.owner_uin AND f.cell_id=d.cell_id AND f.event_type IN (2,311))
         FROM archive_dynamics d WHERE d.owner_uin=?1 AND d.category=?2 ORDER BY d.published_at ASC"
    ).map_err(|error| format!("准备导出查询失败：{error}"))?;
    let rows = statement
        .query_map(params![owner_uin, category], |row| {
            let id = row.get(0)?;
            let video_json = row.get::<_, Option<String>>(8)?;
            let (picture_urls, video_url, video_urls, video_cover_url) =
                public_media_references(id, row.get(7)?, video_json);
            Ok(ArchiveItem {
                id,
                owner_uin: row.get(1)?,
                cell_id: row.get(2)?,
                published_at: row.get(3)?,
                content: row.get(4)?,
                content_recovered: false,
                is_blog: false,
                can_open_original: false,
                author_uin: row.get(5)?,
                author_name: row.get(6)?,
                picture_urls,
                video_url,
                video_urls,
                video_cover_url,
                like_count: row.get(9)?,
                comment_count: row.get(10)?,
                likes: vec![],
                comments: vec![],
            })
        })
        .map_err(|error| format!("查询导出内容失败：{error}"))?;
    let mut items = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取导出内容失败：{error}"))?;
    if let Some(ids) = selected_ids {
        items.retain(|item| ids.contains(&item.id));
    }
    drop(statement);
    let mut comments = connection
        .prepare(
            "SELECT comments_json,actor_uin,actor_name,event_summary,event_time FROM archive_feeds
         WHERE owner_uin=?1 AND cell_id=?2 AND event_type IN (2,311) ORDER BY event_time ASC",
        )
        .map_err(|error| format!("准备导出评论失败：{error}"))?;
    for item in &mut items {
        recover_observed_text(connection, item)?;
        let rows = comments
            .query_map(params![item.owner_uin, item.cell_id], |row| {
                Ok(comment_from_values(
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .map_err(|error| format!("查询导出评论失败：{error}"))?;
        item.comments = merge_comments(rows.filter_map(Result::ok));
    }
    drop(comments);
    let mut export_likes = connection
        .prepare(
            "SELECT actor_uin,actor_name FROM archive_feeds
         WHERE owner_uin=?1 AND cell_id=?2 AND event_type=217 ORDER BY event_time ASC",
        )
        .map_err(|error| format!("准备导出点赞查询失败：{error}"))?;
    for item in &mut items {
        let likes = export_likes
            .query_map(params![item.owner_uin, item.cell_id], |row| {
                Ok(LikeUser {
                    uin: row.get(0)?,
                    nickname: row.get(1)?,
                })
            })
            .map_err(|error| format!("查询导出点赞用户失败：{error}"))?;
        item.likes = likes.filter_map(Result::ok).collect();
    }
    Ok(items)
}

#[tauri::command]
pub async fn export_archived_html(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    category: String,
    ids: Option<Vec<i64>>,
) -> Result<String, String> {
    let _operation = login.operations.read().await;
    validate_category(&category)?;
    let owner_uin = login.qzone_auth().await?.uin;
    let selected = ids.map(|values| values.into_iter().collect::<HashSet<_>>());
    if selected.as_ref().is_some_and(HashSet::is_empty) {
        return Err("请先选择需要导出的归档".into());
    }
    let connection = open_database(&app)?;
    let items = archive_items_for_export(&connection, &owner_uin, &category, selected.as_ref())?;
    if items.is_empty() {
        return Err("当前分类没有可以导出的归档".into());
    }
    let category_name = match category.as_str() {
        "self" => "本人动态",
        "other" => "其他动态",
        _ => "留言",
    };
    let mut cards = String::new();
    for item in &items {
        let author = item
            .author_name
            .as_deref()
            .or(item.author_uin.as_deref())
            .unwrap_or("QQ 用户");
        cards.push_str(
            "<article class=\"card\"><header><span class=\"avatar\">👤</span><div><strong>",
        );
        cards.push_str(&html_escape(author));
        cards.push_str("</strong><small>");
        if let Some(author_uin) = &item.author_uin {
            cards.push_str("QQ ");
            cards.push_str(&html_escape(author_uin));
            cards.push_str(" · ");
        }
        cards.push_str("<time data-time=\"");
        cards.push_str(&item.published_at.to_string());
        cards.push_str("\"></time></small></div></header><div class=\"content\">");
        cards.push_str(&qzone_text_html(item.content.as_deref()));
        cards.push_str("</div>");
        cards.push_str("<p class=\"media-note\">");
        if item.is_blog {
            cards.push_str("日志 · ");
        }
        cards.push_str(crate::content_source::provenance_note(
            item.content.as_deref(),
            item.content_recovered,
        ));
        cards.push_str("</p>");
        if !item.picture_urls.is_empty() {
            cards.push_str("<p class=\"media-note\">🖼 ");
            cards.push_str(&item.picture_urls.len().to_string());
            cards.push_str(" 张图片（严格本地导出不包含远程地址，请在应用内查看）</p>");
        }
        if item.video_url.is_some() {
            cards.push_str(
                "<p class=\"media-note\">▶ 视频（严格本地导出不包含远程地址，请在应用内查看）</p>",
            );
        }
        cards.push_str("<div class=\"stats\">");
        if !item.likes.is_empty() {
            cards.push_str("♥ ");
            let names: Vec<String> = item
                .likes
                .iter()
                .take(10)
                .map(|l| {
                    html_escape(
                        l.nickname
                            .as_deref()
                            .or(l.uin.as_deref())
                            .unwrap_or("QQ用户"),
                    )
                })
                .collect();
            cards.push_str(&names.join("、"));
            if item.likes.len() > 10 {
                cards.push_str(" 等 ");
                cards.push_str(&item.like_count.to_string());
                cards.push_str(" 人赞了");
            } else {
                cards.push_str(" 赞了");
            }
        }
        cards.push_str("　💬 ");
        cards.push_str(&item.comment_count.to_string());
        cards.push_str(" 条评论</div>");
        if !item.comments.is_empty() {
            cards.push_str("<section class=\"comments\">");
            for comment in &item.comments {
                let comment_name = comment
                    .nickname
                    .as_deref()
                    .or(comment.uin.as_deref())
                    .unwrap_or("QQ 用户");
                cards.push_str("<div class=\"comment\"><div class=\"comment-meta\"><b>");
                cards.push_str(&html_escape(comment_name));
                cards.push_str("</b> 评论于 <time data-time=\"");
                cards.push_str(&comment.created_at.to_string());
                cards.push_str("\"></time></div>");
                cards.push_str(&qzone_text_html(Some(&comment.content)));
                if !comment.replies.is_empty() {
                    cards.push_str("<div class=\"replies\">");
                    for reply in &comment.replies {
                        let reply_name = reply
                            .nickname
                            .as_deref()
                            .or(reply.uin.as_deref())
                            .unwrap_or("QQ 用户");
                        cards.push_str("<div><div class=\"comment-meta\"><b>");
                        cards.push_str(&html_escape(reply_name));
                        cards.push_str("</b> 回复 ");
                        cards.push_str(&html_escape(
                            reply
                                .reply_to_nickname
                                .as_deref()
                                .or(reply.reply_to_uin.as_deref())
                                .unwrap_or(comment_name),
                        ));
                        cards.push_str(" · <time data-time=\"");
                        cards.push_str(&reply.created_at.to_string());
                        cards.push_str("\"></time></div>");
                        cards.push_str(&qzone_text_html(Some(&reply.content)));
                        cards.push_str("</div>");
                    }
                    cards.push_str("</div>");
                }
                cards.push_str("</div>");
            }
            cards.push_str("</section>");
        }
        cards.push_str("</article>");
    }
    Ok(format!(
        r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data:"><meta name="viewport" content="width=device-width,initial-scale=1"><title>QQ空间归档 - {category_name}</title><style>*{{box-sizing:border-box}}body{{margin:0;background:#f3f6fb;color:#243247;font:14px/1.7 system-ui,-apple-system,"Microsoft YaHei",sans-serif}}main{{width:min(820px,calc(100% - 24px));margin:30px auto}}h1{{margin:0}}.intro{{color:#758298;margin:0 0 20px}}.card{{background:#fff;border:1px solid #e5eaf2;border-radius:16px;padding:20px;margin:14px 0;box-shadow:0 8px 25px #2038580b}}header{{display:flex;gap:11px;align-items:center}}.avatar{{display:grid;width:44px;height:44px;place-items:center;background:#f3f6fb;border-radius:50%}}header strong,header small{{display:block}}small,.muted,.stats{{color:#7e899a}}.content{{margin:14px 0;white-space:pre-wrap;overflow-wrap:anywhere}}.mention{{color:#2684ff}}.media-note{{padding:8px 10px;color:#758298;background:#f6f8fb;border-radius:8px}}.stats{{margin-top:12px}}.comments{{margin-top:12px;padding:12px;background:#f6f8fb;border-radius:10px}}.comment{{margin:8px 0}}.comment-meta{{margin-bottom:3px;color:#7e899a;font-size:11px}}.comment-meta b{{color:#2684ff}}.replies{{margin:6px 0 0 18px;padding:7px 10px;border-left:2px solid #c9dcf6;background:#fff;border-radius:0 7px 7px 0}}@media(max-width:600px){{main{{margin:16px auto}}.card{{padding:15px}}}}</style></head><body><main><h1>QQ空间归档 · {category_name}</h1><p class="intro">账号 {owner} · 共 {count} 条 · 导出时间 <span id="export-time"></span></p>{cards}</main><script>document.querySelector('#export-time').textContent=new Date().toLocaleString();document.querySelectorAll('time[data-time]').forEach(e=>e.textContent=new Date(Number(e.dataset.time)*1000).toLocaleString());</script></body></html>"#,
        owner = html_escape(&owner_uin),
        count = items.len()
    ))
}

#[tauri::command]
pub async fn count_archived_feeds(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    category: String,
) -> Result<u64, String> {
    let _operation = login.operations.read().await;
    validate_category(&category)?;
    let owner_uin = login.qzone_auth().await?.uin;
    tauri::async_runtime::spawn_blocking(move || {
        let connection = open_database(&app)?;
        connection
            .query_row(
                "SELECT COUNT(*) FROM archive_dynamics WHERE owner_uin=?1 AND category=?2",
                params![owner_uin, category],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count.max(0) as u64)
            .map_err(|error| format!("统计归档数量失败：{error}"))
    })
    .await
    .map_err(|error| format!("归档统计任务异常退出：{error}"))?
}

#[tauri::command]
pub async fn get_archive_overview(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
) -> Result<ArchiveOverview, String> {
    let _operation = login.operations.read().await;
    let owner_uin = login.qzone_auth().await?.uin;
    let database = database_path(&app)?;
    let connection = open_database(&app)?;
    let dynamics = connection
        .query_row(
            "SELECT COUNT(*) FROM archive_dynamics WHERE owner_uin=?1",
            params![owner_uin],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| format!("统计原动态失败：{error}"))?
        .max(0) as u64;
    let (likes, comments) = connection
        .query_row(
            "SELECT COALESCE(SUM(CASE WHEN event_type=217 THEN 1 ELSE 0 END),0),
                COALESCE(SUM(CASE WHEN event_type IN (2,311) THEN 1 ELSE 0 END),0)
         FROM archive_feeds WHERE owner_uin=?1",
            params![owner_uin],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(|error| format!("统计互动记录失败：{error}"))?;
    let mut statement = connection.prepare("SELECT pictures_json FROM archive_dynamics WHERE owner_uin=?1 AND pictures_json IS NOT NULL")
        .map_err(|error| format!("读取图片统计失败：{error}"))?;
    let pictures = statement
        .query_map(params![owner_uin], |row| row.get::<_, Option<String>>(0))
        .map_err(|error| format!("查询图片统计失败：{error}"))?
        .filter_map(Result::ok)
        .map(|json| picture_urls(json).len() as u64)
        .sum();
    let database_bytes = fs::metadata(database).map(|value| value.len()).unwrap_or(0);
    Ok(ArchiveOverview {
        dynamics,
        pictures,
        comments: comments.max(0) as u64,
        likes: likes.max(0) as u64,
        database_bytes,
    })
}

#[tauri::command]
pub async fn get_interaction_ranking(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    limit: u32,
) -> Result<Vec<InteractionRank>, String> {
    let _operation = login.operations.read().await;
    let owner_uin = login.qzone_auth().await?.uin;
    let connection = open_database(&app)?;
    let mut statement = connection
        .prepare(
            "SELECT actor_uin,COALESCE(MAX(NULLIF(actor_name,'')),actor_uin),COUNT(*),
                SUM(CASE WHEN event_type=217 THEN 1 ELSE 0 END),
                SUM(CASE WHEN event_type IN (2,311) THEN 1 ELSE 0 END)
         FROM archive_feeds
         WHERE owner_uin=?1 AND actor_uin IS NOT NULL AND actor_uin<>'' AND actor_uin<>?1
           AND event_type IN (2,217,311)
         GROUP BY actor_uin
         ORDER BY COUNT(*) DESC,MAX(event_time) DESC
         LIMIT ?2",
        )
        .map_err(|error| format!("准备互动排行榜查询失败：{error}"))?;
    let rows = statement
        .query_map(params![owner_uin, limit.clamp(1, 50)], |row| {
            Ok(InteractionRank {
                uin: row.get(0)?,
                nickname: row.get(1)?,
                interactions: row.get::<_, i64>(2)?.max(0) as u64,
                likes: row.get::<_, i64>(3)?.max(0) as u64,
                comments: row.get::<_, i64>(4)?.max(0) as u64,
            })
        })
        .map_err(|error| format!("查询互动排行榜失败：{error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取互动排行榜失败：{error}"))
}

fn ensure_archive_idle(state: &ArchiveState) -> Result<(), String> {
    let progress = state.progress.lock().map_err(|_| "归档状态锁已损坏")?;
    if progress.status == "running" {
        return Err("归档任务运行时不能删除数据，请先取消任务".into());
    }
    Ok(())
}

#[tauri::command]
pub async fn delete_archived_feeds(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    state: tauri::State<'_, ArchiveState>,
    ids: Vec<i64>,
) -> Result<u64, String> {
    let _exclusive = login.operations.try_exclusive()?;
    ensure_archive_idle(&state)?;
    let owner_uin = login.qzone_auth().await?.uin;
    if ids.is_empty() {
        return Ok(0);
    }
    if ids.len() > 500 {
        return Err("单次最多删除 500 条归档记录".into());
    }
    if ids.iter().any(|id| *id <= 0) {
        return Err("归档记录编号无效".into());
    }
    revoke_local_media(&app, &login)?;
    let data_dir = app.path().app_data_dir().map_err(|_| "无法获取归档目录")?;
    let legacy_dir = app
        .path()
        .app_cache_dir()
        .map_err(|_| "无法获取旧视频目录")?
        .join("videos");
    let mut connection = open_database(&app)?;
    let transaction = connection
        .transaction()
        .map_err(|error| format!("开始删除事务失败：{error}"))?;
    let mut count = 0;
    for id in &ids {
        transaction.execute(
            "DELETE FROM archive_feeds WHERE owner_uin=?1 AND cell_id=(SELECT cell_id FROM archive_dynamics WHERE id=?2 AND owner_uin=?1)",
            params![owner_uin, id],
        ).map_err(|error| format!("删除动态互动失败：{error}"))?;
        count += transaction
            .execute(
                "DELETE FROM archive_dynamics WHERE id=?1 AND owner_uin=?2",
                params![id, owner_uin],
            )
            .map_err(|error| format!("批量删除归档失败：{error}"))?;
    }
    transaction
        .commit()
        .map_err(|error| format!("提交删除事务失败：{error}"))?;
    // Keep record deletion atomic. If filesystem cleanup fails, retrying the
    // same IDs can still remove orphaned media in this account's own directory.
    let image_prefixes = ids.iter().map(|id| format!("{id}-")).collect::<Vec<_>>();
    let video_names = ids
        .iter()
        .map(|id| format!("{id}.mp4"))
        .collect::<HashSet<_>>();
    let legacy_names = ids
        .iter()
        .map(|id| format!("{owner_uin}-{id}.mp4"))
        .collect::<HashSet<_>>();
    let cleanup = || -> Result<(), String> {
        remove_matching_files(&data_dir.join("images").join(&owner_uin), |name| {
            image_prefixes.iter().any(|prefix| name.starts_with(prefix))
        })?;
        remove_matching_files(&data_dir.join("videos").join(&owner_uin), |name| {
            video_names.contains(name)
        })?;
        remove_matching_files(&legacy_dir, |name| legacy_names.contains(name))
    };
    cleanup().map_err(|_| "记录已删除，但媒体清理未完成；可重试删除或清空当前账号归档")?;
    Ok(count as u64)
}

fn clear_account_rows(connection: &mut Connection, owner_uin: &str) -> Result<u64, String> {
    let transaction = connection.transaction().map_err(|_| "无法开始清空事务")?;
    let mut dynamics = 0;
    for table in [
        "archive_dynamics",
        "archive_feeds",
        "archive_checkpoints",
        "archive_skips",
    ] {
        let count = transaction
            .execute(
                &format!("DELETE FROM {table} WHERE owner_uin=?1"),
                params![owner_uin],
            )
            .map_err(|_| "清空账号记录失败，未删除任何记录".to_owned())?;
        if table == "archive_dynamics" {
            dynamics = count;
        }
    }
    transaction
        .commit()
        .map_err(|_| "清空事务提交失败".to_owned())?;
    Ok(dynamics as u64)
}

#[tauri::command]
pub async fn clear_archived_feeds(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    state: tauri::State<'_, ArchiveState>,
) -> Result<u64, String> {
    let _exclusive = login.operations.try_exclusive()?;
    ensure_archive_idle(&state)?;
    let owner_uin = login.qzone_auth().await?.uin;
    revoke_local_media(&app, &login)?;
    let mut connection = open_database(&app)?;
    let dynamics = clear_account_rows(&mut connection, &owner_uin)?;
    for kind in ["images", "videos"] {
        let path = app
            .path()
            .app_data_dir()
            .map_err(|_| "无法获取归档目录")?
            .join(kind)
            .join(&owner_uin);
        if path.exists() {
            fs::remove_dir_all(&path)
                .map_err(|_| "记录已清空，但媒体文件清理失败，请重试清空操作")?;
        }
    }
    let legacy_dir = app
        .path()
        .app_cache_dir()
        .map_err(|_| "无法获取旧视频目录")?
        .join("videos");
    let prefix = format!("{owner_uin}-");
    remove_matching_files(&legacy_dir, |name| name.starts_with(&prefix))
        .map_err(|_| "记录已清空，但旧视频缓存清理失败，请重试清空操作")?;
    Ok(dynamics)
}

#[tauri::command]
pub async fn delete_all_app_data(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    state: tauri::State<'_, ArchiveState>,
) -> Result<(), String> {
    let _exclusive = login.operations.try_exclusive()?;
    ensure_archive_idle(&state)?;
    login.clear_session().await;
    revoke_local_media(&app, &login)?;
    let database = database_path(&app)?;
    for path in [
        database.clone(),
        PathBuf::from(format!("{}-wal", database.display())),
        PathBuf::from(format!("{}-shm", database.display())),
    ] {
        if path.exists() {
            fs::remove_file(&path).map_err(|error| format!("删除应用数据库失败：{error}"))?;
        }
    }
    let videos = app
        .path()
        .app_cache_dir()
        .map_err(|error| format!("无法获取缓存目录：{error}"))?
        .join("videos");
    if videos.exists() {
        fs::remove_dir_all(videos).map_err(|error| format!("删除视频缓存失败：{error}"))?;
    }
    let images = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法获取图片归档目录：{error}"))?
        .join("images");
    if images.exists() {
        fs::remove_dir_all(images).map_err(|error| format!("删除图片归档失败：{error}"))?;
    }
    for kind in ["videos", "session-previews"] {
        let path = app
            .path()
            .app_data_dir()
            .map_err(|_| "无法获取归档目录")?
            .join(kind);
        if path.exists() {
            fs::remove_dir_all(path).map_err(|_| "清理本地媒体目录失败")?;
        }
    }
    if let Ok(mut progress) = state.progress.lock() {
        *progress = ArchiveProgress::default();
    }
    Ok(())
}

#[tauri::command]
pub async fn list_interactors(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
) -> Result<Vec<Interactor>, String> {
    let _operation = login.operations.read().await;
    let owner_uin = login.qzone_auth().await?.uin;
    let connection = open_database(&app)?;
    let mut statement = connection
        .prepare(
            "SELECT actor_uin, COALESCE(MAX(NULLIF(actor_name,'')),actor_uin),
                    COUNT(*),
                    SUM(CASE WHEN event_type=217 THEN 1 ELSE 0 END),
                    SUM(CASE WHEN event_type IN (2,311) THEN 1 ELSE 0 END),
                    MAX(event_time)
             FROM archive_feeds
             WHERE owner_uin=?1 AND actor_uin IS NOT NULL AND actor_uin<>'' AND actor_uin<>?1
               AND event_type IN (2,217,311)
             GROUP BY actor_uin
             ORDER BY COUNT(*) DESC",
        )
        .map_err(|error| format!("准备联系人查询失败：{error}"))?;
    let rows = statement
        .query_map(params![owner_uin], |row| {
            Ok(Interactor {
                uin: row.get(0)?,
                nickname: row.get(1)?,
                total: row.get::<_, i64>(2)?.max(0) as u64,
                likes: row.get::<_, i64>(3)?.max(0) as u64,
                comments: row.get::<_, i64>(4)?.max(0) as u64,
                last_at: row.get(5)?,
            })
        })
        .map_err(|error| format!("查询联系人失败：{error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取联系人失败：{error}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn original_source_lookup_cannot_cross_account_boundary() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE archive_dynamics(id INTEGER,owner_uin TEXT,raw_original_json TEXT);
          INSERT INTO archive_dynamics VALUES(1,'10001','{}'),(2,'10002','invalid');",
        )
        .unwrap();
        assert!(super::archived_original(&db, "10001", 1).is_ok());
        assert!(super::archived_original(&db, "10002", 1).is_err());
        assert!(super::archived_original(&db, "10002", 2).is_err());
    }

    #[test]
    fn longer_text_must_be_observed_for_same_account_and_record() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE archive_feeds(owner_uin TEXT,cell_id TEXT,content TEXT);")
            .unwrap();
        let prefix = "这是一条用于核验本地正文补全的合成记录";
        let longer = format!("{prefix}，这是另一条通知中已经保存的后续文字。");
        for (owner, cell, text) in [
            ("10001", "one", longer.clone()),
            ("10002", "one", format!("{longer}别人的更长文本不能取用")),
            ("10001", "two", format!("{longer}另一条记录不能取用")),
        ] {
            db.execute(
                "INSERT INTO archive_feeds VALUES (?1,?2,?3)",
                rusqlite::params![owner, cell, text],
            )
            .unwrap();
        }
        assert_eq!(
            super::longer_observed_text(&db, "10001", "one", &format!("{prefix}...")).unwrap(),
            Some(longer)
        );
        assert_eq!(
            super::longer_observed_text(&db, "10001", "one", "不匹配的另一种正文...").unwrap(),
            None
        );
        assert_eq!(
            super::longer_observed_text(&db, "10001", "one", prefix).unwrap(),
            None,
            "unmarked source must not be silently replaced"
        );
    }
    #[test]
    fn recovered_picture_slots_do_not_reuse_another_photos_legacy_cache() {
        let json = serde_json::json!({"picdata":{"pic":[
            {"busi_param":{"-1":"https://a.qpic.cn/recovered"}},
            {"photourl":[{"url":"https://a.qpic.cn/existing"}]}
        ]}})
        .to_string();
        assert_eq!(super::image_file_stem(Some(&json), 10, 0), "10-recovered-0");
        assert_eq!(super::image_file_stem(Some(&json), 10, 1), "10-0");
    }
    #[test]
    fn media_upgrade_does_not_expand_allowed_destinations() {
        for url in [
            "http://evil.example/a",
            "http://a.qpic.cn.evil.example/a",
            "http://user:secret@a.qpic.cn/a",
            "http://127.0.0.1/a",
            "http://a.qpic.cn:80/a",
        ] {
            let candidate = super::normalize_media_candidate(url.to_owned());
            assert!(crate::network_policy::validate_outbound_url(
                &candidate,
                crate::network_policy::EndpointClass::Media
            )
            .is_err());
        }
    }

    #[test]
    fn clearing_account_keeps_other_accounts_and_request_budget() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        for table in [
            "archive_dynamics",
            "archive_feeds",
            "archive_checkpoints",
            "archive_skips",
            "archive_rate_limits",
        ] {
            db.execute_batch(&format!("CREATE TABLE {table}(owner_uin TEXT); INSERT INTO {table} VALUES('10001'),('10002');")).unwrap();
        }
        assert_eq!(super::clear_account_rows(&mut db, "10001").unwrap(), 1);
        let remaining: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM archive_dynamics WHERE owner_uin='10002'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 1);
        let budget: i64 = db
            .query_row("SELECT COUNT(*) FROM archive_rate_limits", [], |r| r.get(0))
            .unwrap();
        assert_eq!(budget, 2);
    }
    #[test]
    fn core_fix_clear_account_rows_rolls_back_on_failure() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        for table in [
            "archive_dynamics",
            "archive_feeds",
            "archive_checkpoints",
            "archive_skips",
        ] {
            db.execute_batch(&format!("CREATE TABLE {table}(owner_uin TEXT); INSERT INTO {table} VALUES('10001'),('10002');")).unwrap();
        }
        db.execute_batch("CREATE TRIGGER fail_delete BEFORE DELETE ON archive_feeds BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
        assert!(super::clear_account_rows(&mut db, "10001").is_err());
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM archive_dynamics", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 2,
            "first table must roll back if a later table fails"
        );
    }

    #[test]
    fn core_fix_task_cleanup_recovers_initialization_and_batch_failures() {
        let state = super::ArchiveState::new();
        super::set_progress(&state, |p| p.status = "running");
        state
            .batch_retrying
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let cleanup = super::TaskCleanup(&state);
        cleanup.finish::<()>(&Err("synthetic initialization failure".into()));
        drop(cleanup);
        assert_eq!(state.progress.lock().unwrap().status, "error");
        assert!(!state
            .batch_retrying
            .load(std::sync::atomic::Ordering::Relaxed));
        assert!(state.work.try_lock().is_ok());
    }

    #[test]
    fn batch_limit_cleanup_retains_countdown_state() {
        let state = super::ArchiveState::new();
        let cleanup = super::TaskCleanup(&state);
        cleanup.finish::<()>(&Err("ARCHIVE_RATE_LIMIT:1234567890".into()));
        drop(cleanup);
        let progress = state.progress.lock().unwrap();
        assert_eq!(progress.status, "limited");
        assert_eq!(progress.retry_at, Some(1234567890));
    }
    #[test]
    fn core_fix_accepts_busi_only_pictures_without_losing_the_slot() {
        let urls = super::picture_url_candidates(Some(
            serde_json::json!({"picdata":{"pic":[
                {"busi_param":{"-1":"https://a.qpic.cn/one"}},
                {"photourl":[{"url":"https://a.qpic.cn/two"}]}
            ]}})
            .to_string(),
        ));
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0][0], "https://a.qpic.cn/one");
    }

    #[test]
    fn core_fix_http_media_is_upgraded_without_changing_signed_query() {
        let urls = super::picture_url_candidates(Some(
            serde_json::json!({"picdata":{"pic":[
                {"photourl":[{"url":"http://a.photo.store.qq.com/image?sign=a%2Fb+Z&x=1&x=2"}]}
            ]}})
            .to_string(),
        ));
        assert_eq!(
            urls[0][0],
            "https://a.photo.store.qq.com/image?sign=a%2Fb+Z&x=1&x=2"
        );
    }
    use super::{
        advance_feed_cursor, archive_page_delay_ms, checkpoint_is_stale, comment_from_values,
        html_escape, media_request_headers, merge_comments, parse_feed, parse_feed_cursor,
        public_media_references, qzone_text_html, serialize_query_pairs, skip_probe_offsets,
        ArchiveCheckpoint, FeedCursorDetails,
    };
    use serde_json::json;

    #[cfg(unix)]
    #[test]
    fn local_archive_paths_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "qzonearchive-permissions-{}-{}",
            std::process::id(),
            super::now()
        ));
        let file = root.join("archive.sqlite3");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&file, b"private archive").unwrap();

        super::secure_local_path_permissions(&root, true).unwrap();
        super::secure_local_path_permissions(&file, false).unwrap();

        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn media_download_headers_never_include_login_credentials() {
        let headers = media_request_headers("Mozilla/5.0", "image/*").unwrap();
        assert!(!headers.contains_key(reqwest::header::COOKIE));
        assert!(!headers.contains_key(reqwest::header::AUTHORIZATION));
        assert_eq!(
            headers.get(reqwest::header::REFERER).unwrap(),
            "https://user.qzone.qq.com/"
        );
    }

    #[test]
    fn serialized_media_references_never_expose_qq_signed_urls() {
        let pictures = json!({"picdata":{"pic":[{"photourl":[{"url":"https://m.qpic.cn/private.jpg?signature=PICTURE_SECRET"}]}]}}).to_string();
        let video = json!({
            "videourl": "https://photovideo.photo.qq.com/private.mp4?signature=VIDEO_SECRET",
            "coverurl": [{"url": "https://m.qpic.cn/cover.jpg?signature=COVER_SECRET"}]
        })
        .to_string();

        let references = public_media_references(42, Some(pictures), Some(video));
        let serialized = serde_json::to_string(&references).unwrap();

        assert_eq!(references.0, vec!["local-picture-42-0"]);
        assert_eq!(references.1.as_deref(), Some("local-video-42"));
        assert_eq!(references.2, vec!["local-video-42"]);
        assert!(references.3.is_none());
        for forbidden in ["https://", "PICTURE_SECRET", "VIDEO_SECRET", "COVER_SECRET"] {
            assert!(!serialized.contains(forbidden), "泄露了 {forbidden}");
        }
    }

    #[test]
    fn exported_html_escapes_untrusted_qzone_text_and_attributes() {
        let rendered = qzone_text_html(Some(
            r#"<script>steal()</script>@{uin:1\" onmouseover=\"steal(),nick:<img src=x>}"#,
        ));
        assert!(!rendered.contains("<script>"));
        assert!(!rendered.contains("<img"));
        assert!(!rendered.contains("onmouseover=\"steal()\""));
        assert!(rendered.contains("&lt;script&gt;"));
        assert_eq!(html_escape("'\"<&>"), "&#39;&quot;&lt;&amp;&gt;");
    }

    #[test]
    fn parses_like_event_sample_shape() {
        let feed = json!({"comm":{"feedskey":"217_3_key","subid":217,"time":1752553379},
          "original":{"cell_id":{"cellid":"mood1"},"cell_summary":{"summary":"：纪念"},
          "cell_userinfo":{"user":{"uin":"1","nickname":"主人"}},"cell_video":{"videoid":"v1"}},
          "title":{"title":"赞了我"},"userinfo":{"user":{"uin":"2","nickname":"访客"}}});
        let parsed = parse_feed(&feed).unwrap();
        assert_eq!(parsed.feed_key, "217_3_key");
        assert_eq!(parsed.event_type, 217);
        assert!(parsed.video_json.is_some());
    }

    #[test]
    fn parses_comment_and_picture_sample_shape() {
        let feed = json!({"comm":{"feedskey":"311_2_key","subid":2,"time":1751637966},
          "original":{"cell_id":{"cellid":"mood2"},"cell_summary":{"summary":"：哼哧哼哧"},
          "cell_pic":{"picdata":{"pic":[{},{}]}},"cell_comment":{"main_comment":{"content":"评论"}}},
          "summary":{"summary":"又幸福上了"},"userinfo":{"user":{"uin":"3","nickname":"评论者"}}});
        let parsed = parse_feed(&feed).unwrap();
        assert_eq!(parsed.event_type, 2);
        assert_eq!(parsed.picture_count, 2);
        assert!(parsed.comments_json.is_some());
    }

    #[test]
    fn nests_feed_level_reply_under_its_parent_comment() {
        let comments = json!({
            "main_comment": {
                "content": "给我我好想要",
                "date": 1785795539_i64,
                "replynum": 1,
                "replys": null,
                "user": { "uin": "718038005", "nickname": "此刻春和景明_" }
            }
        });

        let comment = comment_from_values(
            Some(comments.to_string()),
            Some("1027704977".into()),
            Some("轻鹄".into()),
            Some("[em]e10324[/em]".into()),
            1785807754,
        );

        assert_eq!(comment.content, "给我我好想要");
        assert_eq!(comment.replies.len(), 1);
        assert_eq!(comment.replies[0].nickname.as_deref(), Some("轻鹄"));
        assert_eq!(comment.replies[0].content, "[em]e10324[/em]");
        assert_eq!(comment.replies[0].created_at, 1785807754);
    }

    #[test]
    fn does_not_turn_a_regular_comment_into_its_own_reply() {
        let comments = json!({
            "main_comment": {
                "content": "入才",
                "date": 1743068483_i64,
                "replynum": 0,
                "replys": null,
                "user": { "uin": "1027704977", "nickname": "轻鹄" }
            }
        });

        let comment = comment_from_values(
            Some(comments.to_string()),
            Some("1027704977".into()),
            Some("轻鹄".into()),
            Some("入才".into()),
            1743068483,
        );

        assert!(comment.replies.is_empty());
    }

    #[test]
    fn merges_multi_round_replies_and_preserves_each_target() {
        let first = json!({
            "main_comment": {
                "commentid": "parent-1",
                "content": "父评论",
                "date": 100_i64,
                "replynum": 1,
                "replys": [{
                    "content": "第一轮",
                    "date": 110_i64,
                    "user": { "uin": "2", "nickname": "乙" },
                    "replyuser": { "uin": "1", "nickname": "甲" }
                }],
                "user": { "uin": "1", "nickname": "甲" }
            }
        });
        let second = json!({
            "main_comment": {
                "commentid": "parent-1",
                "content": "父评论",
                "date": 100_i64,
                "replynum": 1,
                "replys": [{
                    "content": "第二轮",
                    "date": 120_i64,
                    "user": { "uin": "1", "nickname": "甲" },
                    "replyuser": { "uin": "2", "nickname": "乙" }
                }],
                "user": { "uin": "1", "nickname": "甲" }
            }
        });

        let comments = merge_comments([
            comment_from_values(Some(first.to_string()), None, None, None, 0),
            comment_from_values(Some(second.to_string()), None, None, None, 0),
        ]);

        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].replies.len(), 2);
        assert_eq!(comments[0].replies[0].nickname.as_deref(), Some("乙"));
        assert_eq!(
            comments[0].replies[0].reply_to_nickname.as_deref(),
            Some("甲")
        );
        assert_eq!(comments[0].replies[1].nickname.as_deref(), Some("甲"));
        assert_eq!(
            comments[0].replies[1].reply_to_nickname.as_deref(),
            Some("乙")
        );
        assert_eq!(comments[0].replies[1].created_at, 120);
    }

    #[test]
    fn includes_owner_reply_stored_in_comments_array() {
        let comments = json!({
            "comments": [{
                "commentid": "1",
                "content": "嘎嘎咕咕",
                "date": 1786197473_i64,
                "user": { "uin": "1027704977", "nickname": "轻鹄" },
                "replys": [{
                    "replyid": "1",
                    "content": "咕咕咕咕嘎嘎",
                    "date": 1786197525_i64,
                    "user": { "uin": "718038005", "nickname": "此刻春和景明_" },
                    "target": { "uin": "1027704977", "nickname": "轻鹄" }
                }]
            }],
            "main_comment": {
                "commentid": "1",
                "content": "嘎嘎咕咕",
                "date": 1786197473_i64,
                "replynum": 1,
                "replys": null,
                "user": { "uin": "1027704977", "nickname": "轻鹄" }
            }
        });

        let comment = comment_from_values(
            Some(comments.to_string()),
            Some("1027704977".into()),
            Some("轻鹄".into()),
            Some("嘎嘎咕咕".into()),
            1786197473,
        );

        assert_eq!(comment.content, "嘎嘎咕咕");
        assert_eq!(comment.replies.len(), 1);
        assert_eq!(
            comment.replies[0].nickname.as_deref(),
            Some("此刻春和景明_")
        );
        assert_eq!(comment.replies[0].content, "咕咕咕咕嘎嘎");
        assert_eq!(
            comment.replies[0].reply_to_nickname.as_deref(),
            Some("轻鹄")
        );
        assert_eq!(comment.replies[0].created_at, 1786197525);
    }

    #[test]
    fn attaches_parent_author_follow_up_to_latest_child_reply() {
        let comments = json!({
            "comments": [{
                "commentid": "1",
                "content": "嘎嘎咕咕",
                "date": 1786197473_i64,
                "user": { "uin": "1027704977", "nickname": "轻鹄" },
                "replys": [
                    {
                        "replyid": "2",
                        "content": "咕咕嘎嘎咕咕嘎嘎",
                        "date": 1786199046_i64,
                        "user": { "uin": "718038005", "nickname": "此刻春和景明_" },
                        "target": { "uin": "1027704977", "nickname": "轻鹄" }
                    },
                    {
                        "replyid": "3",
                        "content": "凑凑凑凑凑企鹅",
                        "date": 1786199059_i64,
                        "user": { "uin": "718038005", "nickname": "此刻春和景明_" },
                        "target": { "uin": "1027704977", "nickname": "轻鹄" }
                    }
                ]
            }],
            "main_comment": {
                "commentid": "1",
                "content": "嘎嘎咕咕",
                "date": 1786197473_i64,
                "replynum": 4,
                "replys": null,
                "user": { "uin": "1027704977", "nickname": "轻鹄" }
            }
        });

        let comment = comment_from_values(
            Some(comments.to_string()),
            Some("1027704977".into()),
            Some("轻鹄".into()),
            Some("人才咕嘎咕嘎".into()),
            1786199104,
        );

        assert_eq!(comment.replies.len(), 3);
        let follow_up = &comment.replies[2];
        assert_eq!(follow_up.nickname.as_deref(), Some("轻鹄"));
        assert_eq!(follow_up.content, "人才咕嘎咕嘎");
        assert_eq!(
            follow_up.reply_to_nickname.as_deref(),
            Some("此刻春和景明_")
        );
        assert_eq!(follow_up.created_at, 1786199104);
    }

    #[test]
    fn creates_stable_key_for_feed_without_server_identifiers() {
        let feed = json!({
          "comm":{"subid":999,"time":1751637966},
          "summary":{"summary":"一种没有 feedskey 和 cell_id 的特殊互动"},
          "userinfo":{"user":{"uin":"3","nickname":"互动用户"}}
        });

        let first = parse_feed(&feed).expect("特殊互动不应中断整页归档");
        let second = parse_feed(&feed).expect("同一互动应当能重复解析");

        assert!(first.feed_key.starts_with("fallback:999:1751637966:3:"));
        assert_eq!(first.feed_key, second.feed_key);
    }

    #[test]
    fn expires_old_resume_cursor_without_discarding_archive_rows() {
        let checkpoint = ArchiveCheckpoint {
            cursor: "temporary-cursor".into(),
            pages: 78,
            fetched: 706,
            saved: 706,
            updated_at: 1_000,
        };

        assert!(!checkpoint_is_stale(&checkpoint, 1_599));
        assert!(checkpoint_is_stale(&checkpoint, 1_600));
    }

    #[test]
    fn configured_interval_is_never_shortened_by_jitter() {
        let delay = archive_page_delay_ms(3_000);
        assert!((3_000..=3_750).contains(&delay));
    }

    #[test]
    fn advances_nested_qzone_cursor_without_changing_its_time_boundary() {
        let cursor = "att=back%5Fserver%5Finfo%3Doffset%253D1168%2526total%253D4%2526basetime%253D1495974154%2526feedsource%253D1&lastrefreshtime=1785906139&lastseparatortime=0&loadcount=77&refresh_id=1785906139&tl=1495974154";

        assert_eq!(
            parse_feed_cursor(cursor).unwrap(),
            FeedCursorDetails {
                offset: 1168,
                base_time: 1495974154,
                load_count: 77,
            }
        );
        let advanced = advance_feed_cursor(cursor, 2).unwrap();
        assert_eq!(
            parse_feed_cursor(&advanced).unwrap(),
            FeedCursorDetails {
                offset: 1170,
                base_time: 1495974154,
                load_count: 78,
            }
        );
    }

    #[test]
    fn accepts_loadcount_inside_att_and_preserves_that_shape() {
        let backend = serialize_query_pairs(&[
            ("offset".into(), "1168".into()),
            ("basetime".into(), "1495974154".into()),
        ]);
        let attach = serialize_query_pairs(&[
            ("back_server_info".into(), backend),
            ("loadcount".into(), "0".into()),
        ]);
        let cursor =
            serialize_query_pairs(&[("att".into(), attach), ("tl".into(), "1495974154".into())]);

        let advanced = advance_feed_cursor(&cursor, 1).unwrap();
        assert_eq!(
            parse_feed_cursor(&advanced).unwrap(),
            FeedCursorDetails {
                offset: 1169,
                base_time: 1495974154,
                load_count: 1,
            }
        );
        let outer = super::parse_query_pairs(&advanced);
        assert!(super::pair_value(&outer, "loadcount").is_none());
        let attach = super::parse_query_pairs(super::pair_value(&outer, "att").unwrap());
        assert_eq!(super::pair_value(&attach, "loadcount"), Some("1"));
    }

    #[test]
    fn defaults_missing_loadcount_and_adds_it_inside_att() {
        let backend = serialize_query_pairs(&[
            ("offset".into(), "1168".into()),
            ("basetime".into(), "1495974154".into()),
        ]);
        let attach = serialize_query_pairs(&[("back_server_info".into(), backend)]);
        let cursor = serialize_query_pairs(&[("att".into(), attach)]);

        assert_eq!(parse_feed_cursor(&cursor).unwrap().load_count, 0);
        let advanced = advance_feed_cursor(&cursor, 1).unwrap();
        assert_eq!(parse_feed_cursor(&advanced).unwrap().load_count, 1);
    }

    #[test]
    fn probes_large_skip_ranges_exponentially() {
        assert_eq!(
            skip_probe_offsets(1),
            vec![1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096]
        );
        assert_eq!(
            skip_probe_offsets(20),
            vec![20, 32, 64, 128, 256, 512, 1024, 2048, 4096]
        );
    }
}

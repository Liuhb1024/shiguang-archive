mod archive;
mod content_source;
mod media_storage;
mod network_policy;
mod operations;
mod qlogin;
mod qzone;

use tauri::Manager;
use tauri_plugin_opener::OpenerExt;

#[tauri::command]
fn exit_app(app: tauri::AppHandle) {
    app.exit(0);
}

#[tauri::command]
async fn open_qzone(
    app: tauri::AppHandle,
    login: tauri::State<'_, qlogin::QLoginState>,
) -> Result<(), String> {
    let _operation = login.operations.read().await;
    let url = login.official_qzone_home_url().await?;
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|_| "无法使用系统浏览器打开 QQ 空间".to_owned())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(archive::ArchiveState::new())
        .manage(qlogin::QLoginState::new())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            archive::secure_app_directories(app.handle()).map_err(std::io::Error::other)?;
            Ok(())
        })
        .on_page_load(|webview, _payload| {
            if archive::secure_app_directories(webview.app_handle()).is_err() {
                eprintln!("[QzoneArchive] 无法在页面加载后再次收紧本地目录权限");
            }
        })
        .invoke_handler(tauri::generate_handler![
            exit_app,
            open_qzone,
            qlogin::start_qr_login,
            qlogin::poll_qr_login,
            qlogin::get_login_status,
            qlogin::logout_qzone,
            archive::start_feed_archive,
            archive::get_archive_progress,
            archive::cancel_feed_archive,
            archive::list_archive_skips,
            archive::clear_resolved_archive_skips,
            archive::retry_all_archive_skips,
            archive::retry_archive_skip,
            archive::list_archived_feeds,
            archive::list_archived_media,
            archive::get_archived_feed,
            archive::open_archived_original,
            archive::count_archived_feeds,
            archive::export_archived_html,
            archive::load_archived_image,
            archive::load_archived_video,
            archive::get_archive_overview,
            archive::list_interactors,
            archive::get_interaction_ranking,
            archive::delete_archived_feeds,
            archive::clear_archived_feeds,
            archive::delete_all_app_data,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

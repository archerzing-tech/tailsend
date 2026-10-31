// Shared entry: desktop binary and mobile (Android/iOS) library both start here.
mod commands;
mod discovery;
mod netcore;
pub mod selftest;
mod state;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(state::AppState::new())
        .plugin(tauri_plugin_tsnet::init())
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::list_peers,
            commands::ping_peer,
            commands::discover_peers,
            commands::restart_receiver,
            commands::set_receiver_token,
            commands::set_confirm,
            commands::request_send,
            commands::request_respond,
            commands::pick_files,
            commands::pick_folders,
            commands::stat_paths,
            commands::file_icons,
            commands::send_files,
            commands::send_data_begin,
            commands::send_data_chunk,
            commands::send_data_end,
            commands::inbox_list,
            commands::inbox_delete,
            commands::open_inbox,
            commands::node_status,
            commands::node_ensure,
            commands::node_start_receiver,
            commands::node_stop_receiver,
            commands::node_pending,
            commands::node_respond,
            commands::node_ping_target,
            commands::node_send_files,
            commands::node_open_login,
        ])
        .setup(|app| {
            // Android: dirs::download_dir() has no meaning inside an app
            // sandbox — pin the receiver's save dir to app data so received
            // files and the embedded node's inbox both land somewhere real.
            #[cfg(target_os = "android")]
            {
                use tauri::Manager;
                if let Ok(dir) = app.path().app_data_dir() {
                    let dir = dir.join("Tailsend");
                    let _ = std::fs::create_dir_all(&dir);
                    let st = app.state::<state::AppState>();
                    st.recv.lock().unwrap().save_dir = dir;
                }
            }
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                state::bootstrap(handle).await;
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("tailsend 启动失败");
}

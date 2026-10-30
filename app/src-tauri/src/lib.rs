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
            commands::send_files,
            commands::send_data_begin,
            commands::send_data_chunk,
            commands::send_data_end,
            commands::inbox_list,
            commands::inbox_delete,
            commands::open_inbox,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                state::bootstrap(handle).await;
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("tailsend 启动失败");
}

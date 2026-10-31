// Bridge to the embedded tsnet node: Rust commands ↔ run_mobile_plugin ↔
// the Kotlin TsnetNodePlugin ↔ the gomobile AAR. Desktop builds get a stub —
// there the GUI talks to Tailscale through the OS client instead.
use serde_json::Value;
use tauri::{
    plugin::{Builder as PluginBuilder, TauriPlugin},
    Manager, Runtime,
};

#[cfg(target_os = "android")]
const PLUGIN_IDENTIFIER: &str = "io.tailsend.plugin.tsnet";

#[cfg(target_os = "android")]
mod bridge;
#[cfg(target_os = "android")]
use bridge::NodeBridge;
#[cfg(not(target_os = "android"))]
mod desktop;
#[cfg(not(target_os = "android"))]
use desktop::NodeBridge;

/// Managed state: one node bridge per app process.
pub struct TsnetNode<R: Runtime>(NodeBridge<R>);

impl<R: Runtime> TsnetNode<R> {
    /// Runs a Kotlin plugin command; payloads and replies are plain JSON.
    pub async fn call(&self, cmd: &str, payload: Value) -> Result<Value, String> {
        self.0.call(cmd, payload).await
    }
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    PluginBuilder::new("tsnet-node")
        .setup(|app, api| {
            #[cfg(not(target_os = "android"))]
            let _ = api;
            #[cfg(target_os = "android")]
            {
                let state = TsnetNode(bridge::NodeBridge::new(
                    api.register_android_plugin(PLUGIN_IDENTIFIER, "TsnetNodePlugin")?,
                ));
                // Consent relay: the Go gate blocks inside Prompt.ask, whose
                // answer lives in the Kotlin plugin's pending map. Poll it
                // and surface each ask to the webview as an incoming-request
                // event (same shape the Rust receiver emits), so the GUI's
                // confirm dialog works unchanged for embedded-node traffic.
                let relay = state.0.clone();
                // Setup's first arg is already &AppHandle.
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    relay_loop(handle, relay).await;
                });
                app.manage(state);
            }
            #[cfg(not(target_os = "android"))]
            app.manage(TsnetNode::<R>(NodeBridge::new()));
            Ok(())
        })
        .build()
}

/// Polls Kotlin for pending transfer asks and re-emits new ones to the
/// webview. Deadlines stay in Go; this only forwards.
#[cfg(target_os = "android")]
async fn relay_loop<R: tauri::Runtime>(app: tauri::AppHandle<R>, bridge: bridge::NodeBridge<R>) {
    use std::collections::HashSet;
    use tauri::Emitter;

    let mut seen: HashSet<String> = HashSet::new();
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let asks = match bridge.call("pending", serde_json::json!({})).await {
            Ok(v) => v
                .get("asks")
                .and_then(|a| a.as_array())
                .cloned()
                .unwrap_or_default(),
            Err(_) => continue, // node off / bridge busy
        };
        let mut live: HashSet<String> = HashSet::new();
        for ask in &asks {
            let Some(id) = ask.get("id").and_then(|i| i.as_str()) else {
                continue;
            };
            live.insert(id.to_string());
            if !seen.insert(id.to_string()) {
                continue;
            }
            // Embedded-node asks carry file names only (Go's gate passes no
            // sizes); render size-less tiles like the Rust preview does.
            let files: Vec<serde_json::Value> = ask
                .get("names")
                .and_then(|n| n.as_array())
                .map(|names| {
                    names
                        .iter()
                        .map(|n| serde_json::json!({ "name": n.as_str().unwrap_or_default() }))
                        .collect()
                })
                .unwrap_or_default();
            let _ = app.emit(
                "incoming-request",
                serde_json::json!({
                    "id": id,
                    "sender": ask.get("sender").cloned().unwrap_or_default(),
                    "count": ask.get("count").cloned().unwrap_or_default(),
                    "total": ask.get("total").cloned().unwrap_or_default(),
                    "files": files,
                }),
            );
        }
        // Answers and timeouts remove asks; forget them here too.
        seen.retain(|id| live.contains(id));
    }
}

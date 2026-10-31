use serde_json::Value;
use tauri::{plugin::PluginHandle, Runtime};

pub struct NodeBridge<R: Runtime>(PluginHandle<R>);

// Manual impl: derive(Clone) would demand R: Clone, which PluginHandle never
// needs (it holds an Arc'd AppHandle).
impl<R: Runtime> Clone for NodeBridge<R> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<R: Runtime> NodeBridge<R> {
    pub fn new(handle: PluginHandle<R>) -> Self {
        Self(handle)
    }

    pub async fn call(&self, cmd: &str, payload: Value) -> Result<Value, String> {
        self.0
            .run_mobile_plugin_async::<Value>(cmd, payload)
            .await
            .map_err(|e| e.to_string())
    }
}

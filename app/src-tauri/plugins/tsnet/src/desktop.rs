use serde_json::Value;
use tauri::Runtime;

pub struct NodeBridge<R: Runtime>(std::marker::PhantomData<fn() -> R>);

impl<R: Runtime> NodeBridge<R> {
    pub fn new() -> Self {
        Self(std::marker::PhantomData)
    }

    pub async fn call(&self, _cmd: &str, _payload: Value) -> Result<Value, String> {
        Err("内置节点仅在移动端可用".into())
    }
}

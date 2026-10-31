fn main() {
    let result = tauri_plugin::Builder::new(&[])
        .android_path("android")
        .try_build();

    // Documentation builds for Android can't run the native setup (no
    // tauri library path); only docs.rs hits this and we don't care there.
    if !(std::env::var_os("DOCS_RS").is_some()
        && std::env::var("TARGET").unwrap().contains("android"))
    {
        result.unwrap();
    }
}

fn main() {
    // Only the listed app commands exist for the ACL (capabilities/default.json
    // grants exactly these); nothing else is invokable from the WebView.
    #[cfg(feature = "desktop")]
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "net_init",
            "lobby_create",
            "lobby_list",
            "lobby_get",
            "lobby_join",
            "lobby_leave",
        ]),
    ))
    .expect("tauri build");
}

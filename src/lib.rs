slint::include_modules!();

pub fn create_app() -> Result<AppWindow, slint::PlatformError> {
    AppWindow::new()
}

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: slint::android::AndroidApp) {
    slint::android::init(app).unwrap();
    let main_window = create_app().unwrap();
    main_window.run().unwrap();
}

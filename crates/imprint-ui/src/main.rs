//! imprint 桌面端入口。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod fonts;
mod job;
mod preview;

use app::ImprintApp;

fn main() -> eframe::Result {
    env_logger::init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("imprint · 图片视频水印"),
        ..Default::default()
    };

    eframe::run_native(
        "imprint",
        options,
        Box::new(|cc| Ok(Box::new(ImprintApp::new(cc)))),
    )
}

/// Android 入口。
///
/// eframe 通过 `NativeOptions::android_app` 接收 winit 需要的 `AndroidApp`，
/// 用 `cargo apk` 构建。注意 eframe 默认 feature 含 `accesskit`，它与
/// `android-native-activity` 组合会触发 `compile_error!`，Android 必须走
/// `android-game-activity`。
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: winit::platform::android::activity::AndroidApp) {
    use winit::platform::android::EventLoopBuilderExtAndroid as _;

    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );

    let options = eframe::NativeOptions {
        android_app: Some(app),
        ..Default::default()
    };
    let _ = eframe::run_native(
        "imprint",
        options,
        Box::new(|cc| Ok(Box::new(ImprintApp::new(cc)))),
    );
}

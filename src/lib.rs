pub use slint::ComponentHandle;

pub mod android_window;
pub mod cover;
pub mod mxprobe;
pub mod player;
pub mod video_decoder;
pub mod video_surface;
pub mod feed;

slint::include_modules!();

pub fn create_app() -> Result<AppWindow, slint::PlatformError> {
    AppWindow::new()
}

/// 应用完整色板：明暗双套 + Monet 覆盖
///
/// Slint 的 global（Theme）无法从 Rust 侧按名字访问 ——
/// include_modules!() 只 re-export 具名 struct/enum，不含 global。
/// 所以走「Slint 侧读窗口属性」的路子：这里把每个色槽写成 AppWindow 的
/// in-out <color> 属性，UI 侧在 apply_palette() 里一次性搬运到 Theme。
pub fn apply_palette(window: &AppWindow) {
    let dark = android_window::is_dark();
    let palette = android_window::fetch_monet();

    // ARGB -> slint::Color
    let argb = |argb: u32| {
        let a = ((argb >> 24) & 0xff) as u8;
        let r = ((argb >> 16) & 0xff) as u8;
        let g = ((argb >> 8) & 0xff) as u8;
        let b = (argb & 0xff) as u8;
        slint::Color::from_argb_u8(if a == 0 { 255 } else { a }, r, g, b)
    };
    // #RRGGBB 字面量 -> Color
    let hex = |v: u32| {
        slint::Color::from_rgb_u8(
            ((v >> 16) & 0xff) as u8,
            ((v >> 8) & 0xff) as u8,
            (v & 0xff) as u8,
        )
    };
    let monet = |name: &str| {
        palette
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, c)| argb(*c))
    };

    // ------------------------------------------------------------------------
    // 亮色板
    // ------------------------------------------------------------------------
    let light = Palette {
        background: hex(0xFFFFFF),
        surface: hex(0xFFFFFF),
        surface_container: hex(0xEFF3FE),      // 极淡的蓝，顶栏与抽屉共用
        surface_container_high: hex(0xE2E9FD), // 抽屉内浮起卡片
        surface_variant: hex(0xEAF0FD),        // 封面占位
        search_bg: hex(0xFFFFFF),              // 搜索栏内部纯白
        on_surface: hex(0x16181D),
        on_surface_variant: hex(0x5F6672),
        outline: hex(0xC7D2E8),                // 卡片描边 / tab 胶囊轮廓
        outline_variant: hex(0xE5EBF7),
        primary: hex(0x426EFE),                // DeepSeek 亮色主色
        on_primary: hex(0xFFFFFF),
        primary_container: hex(0xDCE4FF),
        on_primary_container: hex(0x001A57),
        secondary: hex(0x4A5578),
        secondary_container: hex(0xDBE1F9),
        scrim: hex(0x000000),                  // 由 UI 侧加透明度，见下
        dark: false,
    };

    // ------------------------------------------------------------------------
    // 暗色板
    // ------------------------------------------------------------------------
    let darkp = Palette {
        background: hex(0x000000),             // 纯黑
        surface: hex(0x0E1014),
        surface_container: hex(0x161A22),      // 顶栏与抽屉共用
        surface_container_high: hex(0x1F242E),
        surface_variant: hex(0x232833),
        search_bg: hex(0x1E232D),              // 比顶栏亮一档的凹槽
        on_surface: hex(0xEDF0F5),
        on_surface_variant: hex(0xA8B0BF),
        outline: hex(0x3B4352),
        outline_variant: hex(0x262C36),
        primary: hex(0x507BF2),                // DeepSeek 暗色主色
        on_primary: hex(0xFFFFFF),
        primary_container: hex(0x1E3773),
        on_primary_container: hex(0xDCE4FF),
        secondary: hex(0xB9C3DC),
        secondary_container: hex(0x323C55),
        scrim: hex(0x000000),
        dark: true,
    };

    let mut p = if dark { darkp } else { light };

    // ------------------------------------------------------------------------
    // Monet 覆盖：只覆盖"品牌色槽"，不覆盖 background/surface 的明暗极性。
    // 原因：Monet 的 neutral 色阶虽也跟随壁纸，但直接把 surface 换成
    // neutral1_900 会让"亮色模式 = 白底"这一条被破坏（neutral1_900 是深色）。
    // 保持明暗底色由设计决定，品牌色跟壁纸走 —— 这是 B 站这类内容型
    // 应用的常规做法（内容可读性优先于纯粹的主题一致性）。
    // ------------------------------------------------------------------------
    if !palette.is_empty() {
        if let Some(c) = monet(if dark { "accent1_200" } else { "accent1_700" }) {
            p.primary = c;
        }
        if let Some(c) = monet(if dark { "accent1_100" } else { "accent1_100" }) {
            p.on_primary = if dark { p.on_primary } else { c };
        }
        if let Some(c) = monet("accent1_900") {
            p.primary_container = if dark { c } else { p.primary_container };
        }
        if let Some(c) = monet("accent2_200") {
            p.secondary = if dark { c } else { p.secondary };
        }
    }

    // ------------------------------------------------------------------------
    // 写入窗口属性
    // ------------------------------------------------------------------------
    window.set_pal_background(p.background);
    window.set_pal_surface(p.surface);
    window.set_pal_surface_container(p.surface_container);
    window.set_pal_surface_container_high(p.surface_container_high);
    window.set_pal_surface_variant(p.surface_variant);
    window.set_pal_search_bg(p.search_bg);
    window.set_pal_on_surface(p.on_surface);
    window.set_pal_on_surface_variant(p.on_surface_variant);
    window.set_pal_outline(p.outline);
    window.set_pal_outline_variant(p.outline_variant);
    window.set_pal_primary(p.primary);
    window.set_pal_on_primary(p.on_primary);
    window.set_pal_primary_container(p.primary_container);
    window.set_pal_on_primary_container(p.on_primary_container);
    window.set_pal_secondary(p.secondary);
    window.set_pal_secondary_container(p.secondary_container);
    window.set_pal_scrim(p.scrim);
    window.set_pal_dark(p.dark);

    window.invoke_apply_palette();
}

struct Palette {
    background: slint::Color,
    surface: slint::Color,
    surface_container: slint::Color,
    surface_container_high: slint::Color,
    surface_variant: slint::Color,
    search_bg: slint::Color,
    on_surface: slint::Color,
    on_surface_variant: slint::Color,
    outline: slint::Color,
    outline_variant: slint::Color,
    primary: slint::Color,
    on_primary: slint::Color,
    primary_container: slint::Color,
    on_primary_container: slint::Color,
    secondary: slint::Color,
    secondary_container: slint::Color,
    scrim: slint::Color,
    dark: bool,
}

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: slint::android::AndroidApp) {
    slint::android::init(app.clone()).unwrap();

    // 登记 JVM / Activity，用于系统手势排除区
    // SAFETY: 指针由 android-activity 提供，生命周期覆盖本次 android_main
    unsafe {
        android_window::init(app.vm_as_ptr(), app.activity_as_ptr());
        // 同时登记 AndroidApp 本身：创建 SurfaceView 必须在 Java 主线程，
        // 需要借它的 run_on_java_main_thread。
        android_window::set_android_app(app.clone());
    }

    let main_window = create_app().unwrap();

    // Monet 动态取色：从系统 tonal palette 拉取壁纸派生色并灌进 Theme
    apply_palette(&main_window);

    // 左边缘排除出系统返回手势区。
    //
    // 这个排除区是 App 自己的语义，**不随页面切换而解除**：
    //   - 首页：边缘右滑没接（抽屉走左上角头像），但保留排除区无害；
    //   - 详情页：边缘右滑由 appwindow 的 EdgeBack 处理。
    //
    // 曾经在进入详情页时调 set_exclusion(false)，把左边缘交还系统，
    // 于是边缘侧滑 = 系统返回 = finish 掉 NativeActivity = App 直接退出。
    // 这是「左边侧滑退出 App」的直接原因，别再改回去。
    android_window::set_exclusion(32, true);

    // 抽屉状态变化时不需要动排除区 —— 抽屉的关闭有显式按钮与遮罩点击。
    // 这里保留回调只是为了让 UI 侧的 drawer_changed 有落点（便于以后加动画/埋点）。
    {
        let weak = main_window.as_weak();
        main_window.on_drawer_changed(move || {
            let _ = weak.upgrade();
        });
    }

    // 视频层不在这里创建。
    // SurfaceView 一旦 VISIBLE 就会盖住 Slint 的 UI（它是后加的 View），
    // 而 INVISIBLE/GONE 又拿不到有效的 Surface。
    // 所以推迟到真正要播放时，通过 run_on_ui_thread 在主线程创建。

    // 点击卡片 → 播放
    {
        let weak = main_window.as_weak();
        main_window.on_play_video(move |bvid| {
            let bvid = bvid.to_string();
            if let Some(w) = weak.upgrade() {
                player::play(w.as_weak(), bvid);
            }
        });
    }

    // 长按卡片（为点赞三连/快捷操作预留）
    {
        main_window.on_video_long_pressed(move |bvid| {
            let bvid = bvid.to_string();
            log::info!("[手势状态机] 长按卡片触发: {}", bvid);
        });
    }

    // 退出二级详情页
    {
        main_window.on_exit_detail_view(move || {
            player::stop_play();
            // 不再动排除区：它必须全程生效，否则详情页的边缘返回会被系统抢走
        });
    }

    // 点赞
    {
        let weak = main_window.as_weak();
        main_window.on_toggle_like(move || {
            player::toggle_like(weak.clone());
        });
    }

    // 投币
    {
        let weak = main_window.as_weak();
        main_window.on_toggle_coin(move || {
            player::toggle_coin(weak.clone());
        });
    }

    // 收藏
    {
        let weak = main_window.as_weak();
        main_window.on_toggle_fav(move || {
            player::toggle_fav(weak.clone());
        });
    }

    // 首屏推荐流：真实数据。
    // fresh_idx=0 表示首次进入（语义见 bli_protocol::recommend）。
    feed::load_feed(main_window.as_weak(), 0);

    // 下拉刷新：重新拉第一页
    {
        let weak = main_window.as_weak();
        main_window.on_refresh_feed(move || {
            if let Some(w) = weak.upgrade() {
                feed::load_feed(w.as_weak(), 0);
            }
        });
    }

    // 滚动到底部：加载下一页
    {
        let weak = main_window.as_weak();
        main_window.on_load_more(move || {
            if let Some(w) = weak.upgrade() {
                feed::load_more(w.as_weak());
            }
        });
    }

    // 封面加载由 feed 模块在拿到数据后按批驱动，这里无需额外初始化

    main_window.run().unwrap();
}

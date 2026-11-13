// ==============================================================================
// 视频 Surface —— 通过 JNI 造一个 SurfaceView，拿到原生窗口
//
// 为什么要走 JNI：
//   MediaCodec 要零拷贝解码，必须有一个 Surface（ANativeWindow）作为输出目标。
//   纯 native 侧只有两条路能拿到 Surface：
//     a. ANativeWindow_fromSurface(JNIEnv, jobject surface) —— 需要一个 Java Surface
//     b. ASurfaceControl + setBuffer(AHardwareBuffer) —— 但 AMediaCodec 没有
//        getOutputImage，拿不到 HardwareBuffer
//   所以只能走 a：用 Java 的 SurfaceView 创建 Surface。
//
//   这不是妥协 —— ExoPlayer、mpv-android、ijkplayer 全都这么干。
//   Android 上「解码到 Surface」就是由 SurfaceView / SurfaceTexture 提供的。
//
// 视图层级：
//   activity.getWindow().getDecorView()  (FrameLayout 根)
//     └─ Slint 的 SurfaceView（backend 自己加的）
//     └─ 我们的 VideoSurfaceView（后加 → 盖在上面）
//
//   视频层天然覆盖在 Slint UI 之上。如果以后要让控制条浮在视频上，
//   需要把控制条也做成独立层，或者接受「控制条被视频遮住」并另想办法。
// ==============================================================================

// 顶层也要 Mutex：VIDEO_SIZE 定义在模块外（见文件末尾），
// 它不属于 imp 模块的 use 作用域。
use std::sync::Mutex;

#[cfg(target_os = "android")]
pub use imp::*;

#[cfg(target_os = "android")]
mod imp {
    use jni::objects::{GlobalRef, JObject, JValue};
    use jni::JavaVM;
    use std::ffi::c_void;
    use std::sync::{Mutex, OnceLock};

    use crate::android_window;

    /// 我们的 SurfaceView 的全局引用，避免被 Java GC 回收
    static SURFACE_VIEW: OnceLock<GlobalRef> = OnceLock::new();



    /// 创建 SurfaceView 并加进 Activity 的视图树。
    ///
    /// 必须在 UI 线程调用 —— Android 要求 View 的层级操作在主线程。
    /// 我们的 android_main 一开始就在主线程，所以直接在启动时调即可。
    pub fn create(parent_vm: *mut c_void, _activity: *mut c_void) -> bool {
        let Ok(vm) = (unsafe { JavaVM::from_raw(parent_vm.cast()) }) else {
            return false;
        };
        let Ok(mut env) = vm.attach_current_thread() else {
            return false;
        };

        // 用 android_window 里已经登记的 activity 引用
        let Some(activity) = android_window::activity_object() else {
            eprintln!("[vid] 拿不到 activity");
            return false;
        };

        // new SurfaceView(Context)
        let Ok(sv) = env.new_object(
            "android/view/SurfaceView",
            "(Landroid/content/Context;)V",
            &[JValue::Object(&activity)],
        ) else {
            eprintln!("[vid] 创建 SurfaceView 失败");
            return false;
        };

        // decorView.addView(sv, FrameLayout.LayoutParams(-1, default_h))
        let Ok(window) = env
            .call_method(&activity, "getWindow", "()Landroid/view/Window;", &[])
            .and_then(|v| v.l())
        else {
            return false;
        };
        let Ok(decor) = env
            .call_method(&window, "getDecorView", "()Landroid/view/View;", &[])
            .and_then(|v| v.l())
        else {
            return false;
        };

        // 播放区上沿：只让出状态栏。浮层 UI 现在盖在视频上（同一几何区域），
        // 不再需要额外的控制条留白。
        let status_top = crate::android_window::status_bar_height_px();
        let default_h = (|| {
            let res = env.call_method(&activity, "getResources", "()Landroid/content/res/Resources;", &[]).ok()?.l().ok()?;
            let dm = env.call_method(&res, "getDisplayMetrics", "()Landroid/util/DisplayMetrics;", &[]).ok()?.l().ok()?;
            let width_px = env.get_field(&dm, "widthPixels", "I").ok()?.i().unwrap_or(1080);
            Some((width_px as f32 * 9.0 / 16.0) as i32)
        })().unwrap_or(607);

        const MATCH_PARENT: i32 = -1;
        let Ok(lp) = env.new_object(
            "android/widget/FrameLayout$LayoutParams",
            "(II)V",
            &[JValue::Int(MATCH_PARENT), JValue::Int(default_h)],
        ) else {
            eprintln!("[vid] 创建 FrameLayout$LayoutParams 失败");
            return false;
        };
        let _ = env.set_field(&lp, "gravity", "I", JValue::Int(48)); // Gravity.TOP
        let _ = env.set_field(&lp, "topMargin", "I", JValue::Int(status_top)); // 留出顶部状态栏空间

        let added = env.call_method(
            &decor,
            "addView",
            "(Landroid/view/View;Landroid/view/ViewGroup$LayoutParams;)V",
            &[
                JValue::Object(&sv),
                JValue::Object(&lp),
            ],
        );

        if added.is_err() {
            eprintln!("[vid] addView 失败");
            return false;
        }

        // ====================================================================
        // 图层关系：视频留在**下层**（默认 APPLICATION_MEDIA_SUBLAYER, z=-2）
        // ====================================================================
        //
        // 这里刻意**不**调用 setZOrderOnTop(true)。实测两种极端都不能用：
        //
        //   z = -2（默认，当前选择）
        //     视频在窗口表面下方。只要 Slint 那一层是**不透明**的，视频就
        //     被整个盖住 —— 这就是"首帧已送显却全黑"的直接原因
        //     （dumpsys SurfaceFlinger 实测：SurfaceView z=-2，
        //       window surface 不透明且铺满全屏）。
        //     解法不是抬 z 序，而是让上层透明，见 android/res/values/strings.xml
        //     的 AppTheme.Translucent 与下面的 set_translucent_window()。
        //
        //   z = 0（setZOrderOnTop）
        //     视频翻到窗口表面**上方**，于是播放区里画不了任何东西：
        //     返回键、日志、控制条、亮度/音量指示、手势反馈全部被盖住。
        //     而这一层后面要承载整套播放 UI 与手势可视化，所以这条路走死。
        //
        // 最终架构（"玻璃盖在电视上"）：
        //
        //   ┌─ Slint window surface (TRANSLUCENT, 有 alpha) ─┐  ← UI / 手势 / 浮层
        //   ├─ SurfaceView  (z=-2, MediaCodec 零拷贝输出) ────┤  ← 视频画面
        //   └─ Activity window (透明背景) ───────────────────┘
        //
        //   上层像素透明处 → 直接看到下层视频，不需要 punchHole。
        //   手势事件照常走 View 树命中测试，SurfaceView 默认不消费触摸，
        //   所以 Slint 的 GestureArea 全程都在正常工作。
        //
        // 断言：如果将来有人为了"看得见视频"又把这行加回来，
        // 播放区里所有 UI 会立刻集体消失 —— 别加。

let _ = env.call_method(&sv, "setVisibility", "(I)V", &[JValue::Int(0)]);

        let Ok(global) = env.new_global_ref(&sv) else {
            return false;
        };
        let _ = SURFACE_VIEW.set(global);

        eprintln!("[vid] SurfaceView 已创建并加入视图树");
        true
    }

    /// 从 SurfaceView 拿到 Surface 的原始指针（ANativeWindow*）。
    ///
    /// 时序很重要：Surface 要等 SurfaceView 真正 attach 到窗口后才有值。
    /// 刚 addView 完立刻 getHolder().getSurface() 通常拿到的是 invalid surface。
    /// 所以要轮询等它 isValid()。
    pub fn acquire_native_window() -> Option<*mut c_void> {
        let vm = android_window::java_vm()?;
        let mut env = vm.attach_current_thread().ok()?;
        let sv = SURFACE_VIEW.get()?;

        // SurfaceView.getHolder() 返回的是接口 SurfaceHolder。
        // JNI 的 GetMethodID 会沿父类链 + 接口查找，所以这里能调通。
        // （曾误以为要换 getSurfaceHolder()，但 SurfaceView 并没有这个方法。）
        let vis = env
            .call_method(sv.as_obj(), "getVisibility", "()I", &[])
            .and_then(|v| v.i())
            .unwrap_or(-1);
        crate::mxprobe::log_public(&format!("getVisibility -> {vis}"));

        let holder = env
            .call_method(
                sv.as_obj(),
                "getHolder",
                "()Landroid/view/SurfaceHolder;",
                &[],
            )
            .and_then(|v| v.l());

        if env.exception_check().unwrap_or(false) {
            if let Ok(ex) = env.exception_occurred() {
                let desc = env
                    .call_method(&ex, "toString", "()Ljava/lang/String;", &[])
                    .and_then(|v| v.l())
                    .ok()
                    .and_then(|o| {
                        let js: &jni::objects::JString = (&o).into();
                        env.get_string(js).ok().map(String::from)
                    });
                crate::mxprobe::log_public(&format!(
                    "getHolder 抛异常: {}",
                    desc.unwrap_or_else(|| "?".into())
                ));
            }
            let _ = env.exception_clear();
            return None;
        }
        let holder = holder.ok()?;

        // 尝试从 SurfaceHolder 拿到 Surface：
        // 优先标准 JNI 接口方法直接调用（微秒级），反射作为兜底
        let surface = (|| -> Option<JObject> {
            // 路径 A: 标准 JNI 接口虚方法派发
            if let Ok(holder_cls) = env.find_class("android/view/SurfaceHolder") {
                if let Ok(mid) = env.get_method_id(&holder_cls, "getSurface", "()Landroid/view/Surface;") {
                    unsafe {
                        let res = env.call_method_unchecked(&holder, mid, jni::signature::ReturnType::Object, &[]);
                        if let Ok(val) = res {
                            if let Ok(obj) = val.l() {
                                if !obj.is_null() {
                                    return Some(obj);
                                }
                            }
                        }
                    }
                    let _ = env.exception_clear();
                }
            }

            // 路径 B: Java 反射兜底
            let holder_cls = env.find_class("android/view/SurfaceHolder").ok()?;
            let name = env.new_string("getSurface").ok()?;
            let method = env
                .call_method(
                    &holder_cls,
                    "getMethod",
                    "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
                    &[JValue::Object(&name), JValue::Object(&JObject::null())],
                )
                .and_then(|v| v.l())
                .ok()?;

            env.call_method(
                &method,
                "invoke",
                "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
                &[JValue::Object(&holder), JValue::Object(&JObject::null())],
            )
            .and_then(|v| v.l())
            .ok()
        })();

        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
            crate::mxprobe::log_public("getSurface 获取失败");
            return None;
        }
        let Some(surface) = surface else {
            crate::mxprobe::log_public("getSurface 返回 None");
            return None;
        };
        if surface.is_null() {
            crate::mxprobe::log_public("getSurface 返回 null");
            return None;
        }

        let valid = env
            .call_method(&surface, "isValid", "()Z", &[])
            .and_then(|v| v.z())
            .unwrap_or(false);

        if !valid {
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_clear();
                crate::mxprobe::log_public("isValid 调用抛异常");
            } else {
                crate::mxprobe::log_public("Surface 已拿到但 isValid=false");
            }
            return None;
        }

        // ANativeWindow_fromSurface(JNIEnv*, jobject)
        extern "C" {
            fn ANativeWindow_fromSurface(
                env: *mut jni::sys::JNIEnv,
                surface: jni::sys::jobject,
            ) -> *mut c_void;
        }

        let raw_env = env.get_raw();
        let win = unsafe { ANativeWindow_fromSurface(raw_env, surface.into_raw()) };
        if win.is_null() {
            None
        } else {
            Some(win)
        }
    }

    /// 强制 SurfaceView 重新走一遍 attach 流程，让 Surface 重新创建。
    ///
    /// 为什么需要这个：alpha=0 能保住"活着"的 Surface，但一旦它被系统
    /// 在某个时机销毁（切后台、Activity 重建、某些 ROM 的内存回收），
    /// 单纯 setVisibility(VISIBLE) **不会**重建它 —— Surface 的创建
    /// 只由 surfaceCreated 回调驱动，而回调只在 View attach 到 window
    /// 时触发。
    ///
    /// 做法：从父容器移除再重新加回去。这是唯一能可靠触发 attach 的手段，
    /// 效果等价于"把这块 View 重新装进窗口"。
    pub fn force_reattach() {
        let Some(vm) = android_window::java_vm() else {
            return;
        };
        let Ok(mut env) = vm.attach_current_thread() else {
            return;
        };
        let Some(sv) = SURFACE_VIEW.get() else {
            return;
        };
        let Some(activity) = android_window::activity_object() else {
            return;
        };

        // 拿 decorView（父容器）
        let Ok(window) = env
            .call_method(&activity, "getWindow", "()Landroid/view/Window;", &[])
            .and_then(|v| v.l())
        else {
            return;
        };
        let Ok(decor) = env
            .call_method(&window, "getDecorView", "()Landroid/view/View;", &[])
            .and_then(|v| v.l())
        else {
            return;
        };

        // removeView → addView，用原来那份 LayoutParams
        let Ok(lp) = env
            .call_method(sv.as_obj(), "getLayoutParams", "()Landroid/view/ViewGroup$LayoutParams;", &[])
            .and_then(|v| v.l())
        else {
            return;
        };

        let _ = env.call_method(
            &decor,
            "removeView",
            "(Landroid/view/View;)V",
            &[JValue::Object(sv.as_obj())],
        );
        let r = env.call_method(
            &decor,
            "addView",
            "(Landroid/view/View;Landroid/view/ViewGroup$LayoutParams;)V",
            &[JValue::Object(sv.as_obj()), JValue::Object(&lp)],
        );

        let _ = env.call_method(sv.as_obj(), "setVisibility", "(I)V", &[JValue::Int(0)]);
        let _ = env.call_method(sv.as_obj(), "setAlpha", "(F)V", &[JValue::Float(1.0)]);

        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
            crate::mxprobe::log_public("[Surface] 重新 attach 失败");
            return;
        }
        crate::mxprobe::log_public(&format!("[Surface] 已强制重新 attach -> {}", r.is_ok()));
    }

    /// 是否已创建过 SurfaceView
    pub fn surface_view_exists() -> bool {
        SURFACE_VIEW.get().is_some()
    }

    /// 显示 / 隐藏视频层
    pub fn set_visible(visible: bool) {
        let Some(vm) = android_window::java_vm() else {
            return;
        };
        let Ok(mut env) = vm.attach_current_thread() else {
            return;
        };
        let Some(sv) = SURFACE_VIEW.get() else {
            return;
        };
        // 隐藏策略：绝不落到 INVISIBLE/GONE —— 两者都会让系统
        // SurfaceHolder 收到 surfaceDestroyed 并销毁 Surface。
        // 实测：从详情页返回后再进第二个视频，Surface 再也 attach 不回来，
        // 日志刷满「Surface 已拿到但 isValid=false」直到
        // 「[播放失败] 等待渲染表面超时」，用户看到的就是黑屏。
        //
        // 改为 VISIBLE 但 alpha=0：Surface 保持存活，图层只是全透明。
        // 代价可以忽略（GPU 合成一个空 buffer），换来的是秒进秒出不再黑。
        let alpha = if visible { 1.0f32 } else { 0.0f32 };
        let _ = env.call_method(
            sv.as_obj(),
            "setAlpha",
            "(F)V",
            &[JValue::Float(alpha)],
        );
        // 同时确保 VISIBLE：alpha=0 只在 VISIBLE 时才有意义，
        // 若之前处于 INVISIBLE 则 alpha 不生效，Surface 依旧是死的。
        let _ = env.call_method(
            sv.as_obj(),
            "setVisibility",
            "(I)V",
            &[JValue::Int(0)],
        );
    }

    static LAST_LAYOUT: std::sync::Mutex<Option<(i32, i32)>> = std::sync::Mutex::new(None);

    /// 全屏切换：按屏幕物理尺寸重设 LayoutParams，并保持视频宽高比。
    ///
    /// `video_w` / `video_h` 是视频自身的分辨率。传 0 表示未知，
    /// 此时退化为铺满（会拉伸，但至少能看到画面）。
    pub fn apply_fullscreen(fullscreen: bool, video_w: i32, video_h: i32) {
        let Some(vm) = android_window::java_vm() else {
            return;
        };
        let Ok(mut env) = vm.attach_current_thread() else {
            return;
        };
        let Some(sv) = SURFACE_VIEW.get() else {
            return;
        };
        let Some(activity) = android_window::activity_object() else {
            return;
        };

        // 取屏幕真实物理尺寸
        let (width_px, height_px) = (|| -> Option<(i32, i32)> {
            let res = env
                .call_method(&activity, "getResources", "()Landroid/content/res/Resources;", &[])
                .ok()?
                .l()
                .ok()?;
            let dm = env
                .call_method(&res, "getDisplayMetrics", "()Landroid/util/DisplayMetrics;", &[])
                .ok()?
                .l()
                .ok()?;
            Some((
                env.get_field(&dm, "widthPixels", "I").ok()?.i().ok()?,
                env.get_field(&dm, "heightPixels", "I").ok()?.i().ok()?,
            ))
        })()
        .unwrap_or((1080, 2400));

        // 全屏时用 MATCH_PARENT 的宽高，让系统按实际方向布局。
        //
        // 为什么不用 height_px：调用此刻屏幕还没转过去，
        // getDisplayMetrics 给的还是竖屏的尺寸（宽<高）。若直接拿它设
        // 高度，旋转后视频高度会是错的（比如 2400 而屏幕现在是 1240 高）。
        // 交给 MATCH_PARENT(-1) 由 View 系统自己跟随窗口尺寸，
        // 旋转完成后自动铺满，不需要我们再算一次。
        // 全屏时按**视频自身宽高比**算居中矩形，而不是 MATCH_PARENT。
        //
        // 为什么不能直接用 MATCH_PARENT：
        //   MediaCodec 会把解码结果拉伸填满整个 Surface。16:9 的视频
        //   放进 1240x2772 的竖屏 Surface 会被拉成一竖条长脸；
        //   放进横屏窗口也会因窗口比例不同而变形。
        //   正确做法是让 Surface 的矩形本身就是视频比例，两侧留黑。
        //
        // letterbox 计算：
        //   取屏幕与视频两个比例，取"更受限"的那一维缩满，另一维留黑。
        //   例：视频 852x480 (1.775)，屏幕横屏 2772x1240 (2.235)
        //       → 高度受限 → 显示尺寸 = 1240*1.775 x 1240 = 2201x1240
        //       → 左右各留 (2772-2201)/2 = 285px 黑边
        //
        // gravity 用 CENTER(17)，让这块矩形在窗口里居中。
        const GRAVITY_TOP: i32 = 48;
        const GRAVITY_CENTER: i32 = 17;

        let video_ar = if video_w > 0 && video_h > 0 {
            video_w as f32 / video_h as f32
        } else {
            0.0
        };

        let (w, h, top, gravity) = if fullscreen {
            if video_ar > 0.0 {
                // 横屏全屏：宽高取屏幕的较长边/较短边
                let sw = width_px.max(height_px) as f32;
                let sh = width_px.min(height_px) as f32;
                let screen_ar = sw / sh;

                let (dw, dh) = if video_ar > screen_ar {
                    // 视频更宽 → 宽度受限，上下留黑
                    (sw, sw / video_ar)
                } else {
                    // 视频更高（或相等）→ 高度受限，左右留黑
                    (sh * video_ar, sh)
                };

                (
                    dw.round() as i32,
                    dh.round() as i32,
                    0,
                    GRAVITY_CENTER,
                )
            } else {
                // 不知道视频分辨率：退回铺满
                (-1, -1, 0, GRAVITY_CENTER)
            }
        } else {
            // 退出全屏：恢复成 16:9 + 状态栏下移
            (
                -1,
                (width_px as f32 * 9.0 / 16.0) as i32,
                crate::android_window::status_bar_height_px(),
                GRAVITY_TOP,
            )
        };

        // 退出全屏时清掉 setFixedSize，否则会一直钉在横屏尺寸上
        if !fullscreen {
            if let Ok(holder) = env
                .call_method(sv.as_obj(), "getHolder", "()Landroid/view/SurfaceHolder;", &[])
                .and_then(|v| v.l())
            {
                let _ = env.call_method(&holder, "setSizeFromLayout", "()V", &[]);
                if env.exception_check().unwrap_or(false) {
                    let _ = env.exception_clear();
                }
            }
        }

        // 清掉去重缓存，确保这次一定真的写下去
        if let Ok(mut lock) = LAST_LAYOUT.lock() {
            *lock = None;
        }

        let Ok(lp) = env.new_object(
            "android/widget/FrameLayout$LayoutParams",
            "(II)V",
            &[JValue::Int(w), JValue::Int(h)],
        ) else {
            return;
        };
        let _ = env.set_field(&lp, "gravity", "I", JValue::Int(gravity));
        let _ = env.set_field(&lp, "topMargin", "I", JValue::Int(top));

        let _ = env.call_method(
            sv.as_obj(),
            "setLayoutParams",
            "(Landroid/view/ViewGroup$LayoutParams;)V",
            &[JValue::Object(&lp)],
        );

        // 同时告诉 SurfaceHolder 这块 buffer 的准确尺寸。
        //
        // 为什么光有 LayoutParams 不够：
        //   SurfaceView 的 buffer 尺寸默认跟随 View 尺寸，但在窗口
        //   刚旋转、布局还没走完一轮时，Surface 可能仍沿用旧尺寸。
        //   setFixedSize 会显式钉死 buffer 大小，使 MediaCodec 拿到的
        //   ANativeWindow 尺寸与我们算的 letterbox 完全一致 ——
        //   否则会出现"View 是 2201 宽、实际 buffer 还是别的尺寸"
        //   导致画面在 View 内被二次缩放，看起来就是画面偏小、黑边偏窄。
        if w > 0 && h > 0 {
            if let Ok(holder) = env
                .call_method(sv.as_obj(), "getHolder", "()Landroid/view/SurfaceHolder;", &[])
                .and_then(|v| v.l())
            {
                let _ = env.call_method(
                    &holder,
                    "setFixedSize",
                    "(II)V",
                    &[JValue::Int(w), JValue::Int(h)],
                );
                if env.exception_check().unwrap_or(false) {
                    let _ = env.exception_clear();
                }
            }
        }

        crate::mxprobe::log_public(&format!(
            "[Surface] 全屏={fullscreen} -> {w}x{h}, top={top}, video={video_w}x{video_h}"
        ));

        // ---- 屏幕方向 ----
        //
        // 全屏必须是**横屏**。上一版只把播放区铺满，结果竖屏状态下
        // 视频被拉成一个大方块，既不是电影比例也不像全屏。
        //
        // setRequestedOrientation：
        //   SCREEN_ORIENTATION_SENSOR_LANDSCAPE = 6  （全屏：横屏，可左右翻转）
        //   SCREEN_ORIENTATION_UNSPECIFIED      = -1 （退出：交还系统）
        //
        // 注意：Manifest 里 configChanges 已含 orientation，
        // Activity 不会因为旋转被重建，所以播放状态不会丢。
        // SCREEN_ORIENTATION_PORTRAIT = 1
        //   —— 退出全屏时**显式**设成竖屏，不能只写 UNSPECIFIED(-1)。
        //   写 -1 只是"交还系统"，而系统当前仍记着上一次的横屏请求，
        //   结果退出全屏后还是横着，用户得手动转回来。
        //
        // SCREEN_ORIENTATION_SENSOR_LANDSCAPE = 6 —— 全屏横屏，可左右翻转。
        let orientation: i32 = if fullscreen { 6 } else { 1 };
        let _ = env.call_method(
            &activity,
            "setRequestedOrientation",
            "(I)V",
            &[JValue::Int(orientation)],
        );
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        } else {
            crate::mxprobe::log_public(&format!(
                "[Surface] 屏幕方向 -> {}",
                if fullscreen { "横屏(6)" } else { "竖屏(1)" }
            ));
        }

        // ---- 状态栏 / 导航栏：全屏时隐藏 ----
        //
        // 横屏看视频时状态栏会占掉顶部一条，还会压住画面（我们播放区
        // 是全屏的，状态栏直接叠在视频上）。用 WindowInsetsController
        // 把系统栏藏起来，退出全屏再显示回来。
        //
        // 用老 API（setSystemUiVisibility + FLAG_FULLSCREEN）的实现：
        //   Android 11+ 虽标记为 deprecated，但功能完全有效，且不需要
        //   去 find_class 拿 WindowInsetsController 这种内部类。
        //   对一个 minSdk 26 的项目来说这是最省事且兼容面最广的路子。
        let flags: i32 = if fullscreen {
            // FLAG_FULLSCREEN = 1024
            // SYSTEM_UI_FLAG_FULLSCREEN = 4
            // SYSTEM_UI_FLAG_HIDE_NAVIGATION = 2
            // SYSTEM_UI_FLAG_IMMERSIVE_STICKY = 4096
            1024
        } else {
            0
        };
        let decor = env
            .call_method(&window, "getDecorView", "()Landroid/view/View;", &[])
            .and_then(|v| v.l());
        if let Ok(decor) = decor {
            let sys_ui: i32 = if fullscreen { 4 | 2 | 4096 } else { 0 };
            let _ = env.call_method(
                &decor,
                "setSystemUiVisibility",
                "(I)V",
                &[JValue::Int(sys_ui)],
            );
        }
        let _ = env.call_method(
            &window,
            "setFlags",
            "(II)V",
            &[JValue::Int(flags), JValue::Int(1024)],
        );
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
        crate::mxprobe::log_public(&format!(
            "[Surface] 系统栏 -> {}",
            if fullscreen { "隐藏" } else { "显示" }
        ));
    }

    /// 更新 SurfaceView 的高宽与顶部边距（在主线程运行）
    pub fn update_layout(height_px: i32, top_px: i32) {
        if let Ok(mut lock) = LAST_LAYOUT.lock() {
            if *lock == Some((height_px, top_px)) {
                return; // 尺寸与位置未改变，绝不重复调用 setLayoutParams 导致 Surface 销毁重建
            }
            *lock = Some((height_px, top_px));
        }

        let Some(vm) = android_window::java_vm() else {
            return;
        };
        let Ok(mut env) = vm.attach_current_thread() else {
            return;
        };
        let Some(sv) = SURFACE_VIEW.get() else {
            return;
        };

        crate::mxprobe::log_public(&format!("[Surface] 更新 SurfaceView 布局: h={height_px}, top={top_px}"));

        // new FrameLayout.LayoutParams(-1, height_px)
        let Ok(lp) = env.new_object(
            "android/widget/FrameLayout$LayoutParams",
            "(II)V",
            &[JValue::Int(-1), JValue::Int(height_px)],
        ) else {
            return;
        };

        // gravity = Gravity.TOP (48)
        let _ = env.set_field(&lp, "gravity", "I", JValue::Int(48));
        // topMargin = top_px
        let _ = env.set_field(&lp, "topMargin", "I", JValue::Int(top_px));

        let _ = env.call_method(
            sv.as_obj(),
            "setLayoutParams",
            "(Landroid/view/ViewGroup$LayoutParams;)V",
            &[JValue::Object(&lp)],
        );
    }

    /// 收起视频层：0 高度 + 归零边距，但保持 VISIBLE + alpha=0。
    ///
    /// 只改 LayoutParams，不碰 SurfaceHolder —— Surface 必须活到下次播放。
    pub fn park() {
        let Some(vm) = android_window::java_vm() else {
            return;
        };
        let Ok(mut env) = vm.attach_current_thread() else {
            return;
        };
        let Some(sv) = SURFACE_VIEW.get() else {
            return;
        };

        // alpha=0 但不 INVISIBLE：View 保留，Surface 保留
        let _ = env.call_method(
            sv.as_obj(),
            "setAlpha",
            "(F)V",
            &[JValue::Float(0.0)],
        );

        let Ok(lp) = env.new_object(
            "android/widget/FrameLayout$LayoutParams",
            "(II)V",
            &[JValue::Int(-1), JValue::Int(0)],
        ) else {
            return;
        };
        let _ = env.set_field(&lp, "gravity", "I", JValue::Int(48));
        let _ = env.set_field(&lp, "topMargin", "I", JValue::Int(0));
        let _ = env.call_method(
            sv.as_obj(),
            "setLayoutParams",
            "(Landroid/view/ViewGroup$LayoutParams;)V",
            &[JValue::Object(&lp)],
        );

        // 下次 set_adaptive_layout 必须真的写回尺寸，因此清掉去重缓存
        if let Ok(mut lock) = LAST_LAYOUT.lock() {
            *lock = None;
        }
    }

    /// 根据设备真实屏幕尺寸与视频横竖屏，自适应调整 SurfaceView（在主线程运行）
    pub fn update_adaptive_layout(is_vertical: bool) {
        let Some(vm) = android_window::java_vm() else {
            return;
        };
        let Ok(mut env) = vm.attach_current_thread() else {
            return;
        };
        let Some(sv) = SURFACE_VIEW.get() else {
            return;
        };
        let Some(activity) = android_window::activity_object() else {
            return;
        };

        // 从 Activity 获取真实 DisplayMetrics 物理像素
        let Ok(resources) = env
            .call_method(&activity, "getResources", "()Landroid/content/res/Resources;", &[])
            .and_then(|v| v.l())
        else {
            return;
        };
        let Ok(dm) = env
            .call_method(&resources, "getDisplayMetrics", "()Landroid/util/DisplayMetrics;", &[])
            .and_then(|v| v.l())
        else {
            return;
        };

        let width_px = env.get_field(&dm, "widthPixels", "I").map(|v| v.i().unwrap_or(1080)).unwrap_or(1080);
        let height_px = env.get_field(&dm, "heightPixels", "I").map(|v| v.i().unwrap_or(2400)).unwrap_or(2400);
        let density = env.get_field(&dm, "density", "F").map(|v| v.f().unwrap_or(2.0)).unwrap_or(2.0);
        // 播放区上沿 = 状态栏。与 Slint 的 safe_top 对齐（浮层与视频同区域）。
        let top_px = crate::android_window::status_bar_height_px();

        // 竖屏与横屏尺寸与 Slint DSL 保持严格对齐，且留出顶部状态栏与控制条空间
        let target_h = if is_vertical {
            let avail_h = (height_px - top_px).max(0);
            let min_h = (220.0 * density) as i32;
            let max_h = (380.0 * density) as i32;
            ((avail_h as f32 * 0.45) as i32).clamp(min_h, max_h)
        } else {
            (width_px as f32 * 9.0 / 16.0) as i32
        };

        let Ok(lp) = env.new_object(
            "android/widget/FrameLayout$LayoutParams",
            "(II)V",
            &[JValue::Int(-1), JValue::Int(target_h)],
        ) else {
            return;
        };

        let _ = env.set_field(&lp, "gravity", "I", JValue::Int(48)); // Gravity.TOP
        let _ = env.set_field(&lp, "topMargin", "I", JValue::Int(top_px)); // 状态栏留白

        let _ = env.call_method(
            sv.as_obj(),
            "setLayoutParams",
            "(Landroid/view/ViewGroup$LayoutParams;)V",
            &[JValue::Object(&lp)],
        );
    }
}

#[cfg(not(target_os = "android"))]
pub fn create(_vm: *mut c_void, _activity: *mut c_void) -> bool {
    false
}

/// 把闭包排到 Android 的 Java 主线程执行。
///
/// View 的层级操作只能由创建视图树的线程做，否则 Android 会抛
/// CalledFromWrongThreadException **并直接杀进程**（不是异常可捕获的那种）。
///
/// android-activity 提供了 `run_on_java_main_thread`，正好用来做这件事。
/// 这也解释了为什么必须早点把它接出来：我们的播放流程跑在后台线程，
/// 但创建 SurfaceView 必须在主线程。
pub fn run_on_ui_thread<F: FnOnce() + Send + 'static>(f: F) {
    #[cfg(target_os = "android")]
    {
        if let Some(app) = crate::android_window::android_app() {
            app.run_on_java_main_thread(Box::new(f));
            return;
        }
        // 拿不到 AndroidApp（理论上不应该），直接执行会崩，
        // 记录后放弃更安全。
        crate::mxprobe::log_public("run_on_ui_thread: 没有 AndroidApp");
    }
    #[cfg(not(target_os = "android"))]
    {
        f();
    }
}

/// 创建视频层。**必须在主线程调用** ——
/// Android 的 View 层级操作只能由创建视图树的那个线程执行，
/// 否则抛 CalledFromWrongThreadException 并直接杀进程。
///
/// 返回后 Surface 不一定立刻可用（SurfaceView 需要等 attach 完成），
/// 调用方应稍后调 `acquire_native_window()` 轮询。
pub fn setup_on_main_thread() -> bool {
    #[cfg(target_os = "android")]
    {
        // 若已存在，恢复可见性并设置默认 16:9 尺寸，确保 Surface 能被窗口系统及时创建
        if imp::surface_view_exists() {
            crate::mxprobe::log_public("SurfaceView 已存在，重置可见与布局");
            imp::set_visible(true);
            imp::update_adaptive_layout(false);
            return true;
        }

        crate::mxprobe::log_public("=== 创建 SurfaceView（主线程）===");
        let Some(vm) = crate::android_window::java_vm() else {
            crate::mxprobe::log_public("拿不到 JavaVM");
            return false;
        };
        let raw = vm.get_java_vm_pointer();
        let ok = create(raw.cast(), std::ptr::null_mut());
        crate::mxprobe::log_public(&format!("create -> {ok}"));
        if ok {
            imp::set_visible(true);
        }
        ok
    }
    #[cfg(not(target_os = "android"))]
    {
        false
    }
}

/// 更新视频 Surface 尺寸和位置
pub fn update_surface_layout(height_px: i32, top_px: i32) {
    run_on_ui_thread(move || {
        #[cfg(target_os = "android")]
        imp::update_layout(height_px, top_px);
    });
}

/// 全屏：SurfaceView 铺满整屏（含状态栏区域）。
///
/// 为什么必须单独一个函数而不是复用 update_layout：
///   全屏时 topMargin 要归 0（连状态栏区域一起占），
///   高度用**屏幕物理高度**而不是播放区高度 —— 后者是按 16:9 或 45%
///   算出来的，全屏语义完全不同。
pub fn set_fullscreen_surface(fullscreen: bool, video_w: i32, video_h: i32) {
    run_on_ui_thread(move || {
        #[cfg(target_os = "android")]
        imp::apply_fullscreen(fullscreen, video_w, video_h);
    });
}

/// 记录当前视频分辨率，供全屏时算 letterbox
static VIDEO_SIZE: Mutex<Option<(i32, i32)>> = Mutex::new(None);

/// 由 player 在拿到播放地址后调用
pub fn set_video_size(w: u32, h: u32) {
    if let Ok(mut g) = VIDEO_SIZE.lock() {
        *g = Some((w as i32, h as i32));
    }
}

fn current_video_size() -> (i32, i32) {
    VIDEO_SIZE
        .lock()
        .ok()
        .and_then(|g| *g)
        .unwrap_or((0, 0))
}

/// 自适应调整视频 Surface 尺寸 (横屏 16:9 / 竖屏 48%)
pub fn set_adaptive_layout(is_vertical: bool) {
    run_on_ui_thread(move || {
        #[cfg(target_os = "android")]
        imp::update_adaptive_layout(is_vertical);
    });
}

/// 强制视频层重新 attach（Surface 被系统销毁后用来重建）
pub fn force_reattach_surface() {
    run_on_ui_thread(|| {
        #[cfg(target_os = "android")]
        imp::force_reattach();
    });
}

/// 隐藏视频 Surface（保持几何尺寸，杜绝 Surface 销毁重建）
pub fn hide_surface() {
    run_on_ui_thread(|| {
        #[cfg(target_os = "android")]
        {
            imp::set_visible(false);
        }
    });
}

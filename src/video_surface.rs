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

#[cfg(target_os = "android")]
pub use imp::*;

#[cfg(target_os = "android")]
mod imp {
    use jni::objects::{GlobalRef, JObject, JValue};
    use jni::JavaVM;
    use std::ffi::c_void;
    use std::sync::OnceLock;

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

        // 从 Activity 获取真实 DisplayMetrics 物理像素与状态栏高度，初始限定在 16:9 且让出顶部状态栏空间
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

        // 保持 VISIBLE(0)，但严格局限于顶部 16:9 播放区域，绝不遮盖下方 UI
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
        // View.VISIBLE = 0, View.INVISIBLE = 4
        // 同样不用 GONE，否则隐藏后 Surface 会被销毁，再显示要等重建。
        let _ = env.call_method(
            sv.as_obj(),
            "setVisibility",
            "(I)V",
            &[JValue::Int(if visible { 0 } else { 4 })],
        );
    }

    /// 更新 SurfaceView 的高宽与顶部边距（在主线程运行）
    pub fn update_layout(height_px: i32, top_px: i32) {
        let Some(vm) = android_window::java_vm() else {
            return;
        };
        let Ok(mut env) = vm.attach_current_thread() else {
            return;
        };
        let Some(sv) = SURFACE_VIEW.get() else {
            return;
        };

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
        let top_px = crate::android_window::status_bar_height_px();

        // 竖屏与横屏尺寸与 Slint DSL 保持严格对齐，且留出顶部状态栏空间 topMargin
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
        let _ = env.set_field(&lp, "topMargin", "I", JValue::Int(top_px)); // 状态栏留白，严禁覆盖状态栏与下方 UI

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

/// 自适应调整视频 Surface 尺寸 (横屏 16:9 / 竖屏 48%)
pub fn set_adaptive_layout(is_vertical: bool) {
    run_on_ui_thread(move || {
        #[cfg(target_os = "android")]
        imp::update_adaptive_layout(is_vertical);
    });
}

/// 隐藏视频 Surface 并把尺寸清零
pub fn hide_surface() {
    run_on_ui_thread(|| {
        #[cfg(target_os = "android")]
        {
            imp::set_visible(false);
            imp::update_layout(0, 0);
        }
    });
}

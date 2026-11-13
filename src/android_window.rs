// ==============================================================================
// Android 窗口级配置：系统手势排除区 (System Gesture Exclusion Rects)
//
// 问题：
//   Android 10+ 手势导航把屏幕左右各约 20dp 划给"系统返回"，应用层收不到
//   这块的触摸事件。实测从 x=6px 起手右滑，App 直接被切到后台 ——
//   抽屉的左边缘手势因此完全不可用。
//
// 解决：
//   Window.setSystemGestureExclusionRects(List<Rect>) 可以让应用声明
//   "这块区域不要被系统抢占"。单边配额 200dp，我们只需一条 32dp 窄条。
//
//   本项目 hasCode=false、没有 Java 类，所以直接走 JNI 反射调用框架 API：
//     activity.getWindow().getDecorView().setSystemGestureExclusionRects(list)
//   Rect / ArrayList 都在 android.jar 里，运行时必然存在。
//
//   用 jni crate 而非手写 vtable：手写 JNI 函数表有上百个字段，
//   顺序错一个就是静默崩溃，不值这个风险。
//
// 时序：
//   界面打开时登记排除区；抽屉展开期间解除排除 —— 抽屉已经在前台，
//   此时用户从边缘右滑的意图是"关闭抽屉"，应该交还给系统返回。
// ==============================================================================

#[cfg(target_os = "android")]
mod imp {
    use jni::objects::{JObject, JValue};
    use jni::JavaVM;
    use std::ffi::c_void;
    use std::sync::OnceLock;

    struct Ctx {
        vm: JavaVM,
        activity: *mut c_void,
    }

    /// AndroidApp 内部是 Arc<RwLock<..>>，可以安全 clone 并长期持有。
    /// 存一份用于调度到 Java 主线程。
    static APP: OnceLock<slint::android::AndroidApp> = OnceLock::new();

    unsafe impl Send for Ctx {}
    unsafe impl Sync for Ctx {}

    static CTX: OnceLock<Ctx> = OnceLock::new();

    /// 由 android_main 在启动时登记 JVM 与 Activity（均来自 AndroidApp）
    pub fn init(vm_ptr: *mut c_void, activity_ptr: *mut c_void) {
        // SAFETY: 指针由 android_activity 提供，在 AndroidApp 存活期间有效。
        let vm = unsafe { JavaVM::from_raw(vm_ptr.cast()) };
        let Ok(vm) = vm else { return };
        let _ = CTX.set(Ctx { vm, activity: activity_ptr });
    }

    /// 登记 AndroidApp，供后续调度到 Java 主线程
    pub fn set_android_app(app: slint::android::AndroidApp) {
        let _ = APP.set(app);
    }

    /// 取 AndroidApp 的副本（内部 Arc，clone 很便宜）
    pub fn android_app() -> Option<slint::android::AndroidApp> {
        APP.get().cloned()
    }

    /// 给其他模块用的 activity 对象（JNI 全局引用的临时视图）
    pub fn activity_object() -> Option<JObject<'static>> {
        let ctx = CTX.get()?;
        // SAFETY: activity 是 AndroidApp 持有的全局引用，生命周期覆盖整个进程
        Some(unsafe { JObject::from_raw(ctx.activity.cast()) })
    }

    /// 给其他模块用的 JavaVM 句柄
    pub fn java_vm() -> Option<JavaVM> {
        let vm = &CTX.get()?.vm;
        // JavaVM 内部是 Arc，clone 一份即可
        let raw = vm.get_java_vm_pointer();
        unsafe { JavaVM::from_raw(raw.cast()).ok() }
    }

    fn with_env<T>(f: impl FnOnce(&mut jni::JNIEnv, &JObject) -> T) -> Option<T> {
        let ctx = CTX.get()?;
        let mut env = ctx.vm.attach_current_thread().ok()?;
        // SAFETY: activity 是 JNI 全局引用，由 AndroidApp 持有
        let activity = unsafe { JObject::from_raw(ctx.activity.cast()) };
        Some(f(&mut env, &activity))
    }

    // =========================================================================
    // 主题取色：uiMode(明/暗) + Monet(壁纸派生)
    //
    // 明暗判定：
    //   resources.configuration.uiMode & Configuration.UI_MODE_NIGHT_MASK
    //   == UI_MODE_NIGHT_YES  → 暗色
    //   这是系统级的权威来源，跟随用户在设置里的选择。
    //
    // Monet（Material You，API 31+）：
    //   系统从壁纸派生出五组色调盘，暴露成 framework-res 的 PUBLIC 色资源：
    //     system_accent{1,2,3}_{0..1000}   主/次/三色阶（500 = 基准色相，数字越大越暗）
    //     system_neutral{1,2}_{...}        中性灰阶
    //   这些资源带 night 变体，所以直接读就是当前明暗模式下的正确值，
    //   不需要自己算 HCT 色彩空间。
    //
    // 读取方式（hasCode=false 下唯一可行）：
    //   Resources.getIdentifier(name, "color", "android") → id
    //   Resources.getColor(id, null) → ARGB
    // =========================================================================

    /// 当前是否暗色模式
    ///
    /// 读 resources.configuration.uiMode 的 UI_MODE_NIGHT_MASK(0x30) 位：
    ///   UI_MODE_NIGHT_NO(0x10) → 亮色；UI_MODE_NIGHT_YES(0x20) → 暗色
    pub fn is_dark() -> bool {
        with_env(|env, activity| {
            let res = env
                .call_method(activity, "getResources", "()Landroid/content/res/Resources;", &[])
                .ok()
                .and_then(|v| v.l().ok());
            let Some(res) = res else { return true };

            let cfg = env
                .call_method(&res, "getConfiguration", "()Landroid/content/res/Configuration;", &[])
                .ok()
                .and_then(|v| v.l().ok());
            let Some(cfg) = cfg else { return true };

            let mode = env.get_field(&cfg, "uiMode", "I").ok().and_then(|v| v.i().ok());
            match mode {
                Some(m) => (m & 0x30) == 0x20,
                None => true,
            }
        })
        .unwrap_or(true)
    }

    /// 从系统资源读一个 ARGB 颜色
    fn sys_color(env: &mut jni::JNIEnv, activity: &JObject, name: &str) -> Option<u32> {
        let res = env
            .call_method(activity, "getResources", "()Landroid/content/res/Resources;", &[])
            .ok()?
            .l()
            .ok()?;

        let jname = env.new_string(name).ok()?;
        let jtype = env.new_string("color").ok()?;
        let jpkg = env.new_string("android").ok()?;
        let id = env
            .call_method(
                &res,
                "getIdentifier",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I",
                &[JValue::Object(&jname), JValue::Object(&jtype), JValue::Object(&jpkg)],
            )
            .ok()?
            .i()
            .ok()?;
        if id == 0 {
            return None;
        }

        env.call_method(
            &res,
            "getColor",
            "(ILandroid/content/res/Resources$Theme;)I",
            &[JValue::Int(id), JValue::Object(&JObject::null())],
        )
        .ok()?
        .i()
        .ok()
        .map(|v| v as u32)
    }

    /// 抓取 Monet 调色盘（名称, ARGB）。取不到返回空 vec。
    pub fn fetch_monet() -> Vec<(&'static str, u32)> {
        const NAMES: &[&str] = &[
            "accent1_100", "accent1_200", "accent1_500", "accent1_700", "accent1_800", "accent1_900",
            "accent2_100", "accent2_200", "accent2_500", "accent2_700",
            "neutral1_10", "neutral1_50", "neutral1_100", "neutral1_900", "neutral1_1000",
            "neutral2_100", "neutral2_200", "neutral2_300", "neutral2_700", "neutral2_800",
        ];
        with_env(|env, activity| {
            let mut out = Vec::new();
            for n in NAMES {
                let full = format!("system_{n}");
                if let Some(c) = sys_color(env, activity, &full) {
                    out.push((*n, c));
                }
            }
            out
        })
        .unwrap_or_default()
    }

    /// 设置排除区：左侧 width_dp 宽、全高的一条竖带
    pub fn set_exclusion(width_dp: i32, enable: bool) -> bool {
        with_env(|env, activity| {
            let Ok(window) = env
                .call_method(activity, "getWindow", "()Landroid/view/Window;", &[])
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

            let Ok(list_cls) = env.find_class("java/util/ArrayList") else { return false };
            let Ok(list) = env.new_object(&list_cls, "()V", &[]) else { return false };

            if enable {
                let Ok(metrics) = env
                    .call_method(activity, "getResources", "()Landroid/content/res/Resources;", &[])
                    .and_then(|v| v.l())
                else {
                    return false;
                };
                let Ok(dm) = env
                    .call_method(&metrics, "getDisplayMetrics", "()Landroid/util/DisplayMetrics;", &[])
                    .and_then(|v| v.l())
                else {
                    return false;
                };
                let Ok(density) = env
                    .get_field(&dm, "density", "F")
                    .and_then(|v| v.f())
                else {
                    return false;
                };
                let Ok(height) = env.get_field(&dm, "heightPixels", "I").and_then(|v| v.i()) else {
                    return false;
                };

                let w = (width_dp as f32 * density) as i32;
                let Ok(rect_cls) = env.find_class("android/graphics/Rect") else { return false };
                let Ok(rect) = env.new_object(
                    &rect_cls,
                    "(IIII)V",
                    &[
                        JValue::Int(0),
                        JValue::Int(0),
                        JValue::Int(w),
                        JValue::Int(height),
                    ],
                ) else {
                    return false;
                };
                let _ = env.call_method(
                    &list,
                    "add",
                    "(Ljava/lang/Object;)Z",
                    &[JValue::Object(&rect)],
                );
            }

            let r = env.call_method(
                &decor,
                "setSystemGestureExclusionRects",
                "(Ljava/util/List;)V",
                &[JValue::Object(&list)],
            );
            if env.exception_check().unwrap_or(false) {
                env.exception_describe();
                env.exception_clear();
            }
            r.is_ok()
        })
        .unwrap_or(false)
    }
}

#[cfg(target_os = "android")]
pub use imp::{activity_object, android_app, fetch_monet, init, is_dark, java_vm, set_android_app, set_exclusion};

/// 非 Android 平台（桌面调试）为空实现
#[cfg(not(target_os = "android"))]
pub fn init(_vm: *mut std::ffi::c_void, _activity: *mut std::ffi::c_void) {}

#[cfg(not(target_os = "android"))]
pub fn set_exclusion(_width_dp: i32, _enable: bool) -> bool {
    false
}

#[cfg(not(target_os = "android"))]
pub fn fetch_monet() -> Vec<(&'static str, u32)> {
    Vec::new()
}

#[cfg(not(target_os = "android"))]
pub fn is_dark() -> bool {
    true
}

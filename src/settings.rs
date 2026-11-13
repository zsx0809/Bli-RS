// ==============================================================================
// 极简设置持久化
//
// 需求只有"自动播放开关"这一个布尔值，不值得引入 serde + 独立 json 文件，
// 更不值得上 SharedPreferences（那要走 JNI，还得让 hasCode=false 的
// NativeActivity 去 find_class 一个 Java 类）。
//
// 方案：应用私有目录下一个 key=value 文本文件。
//   优点：零依赖、纯 native、进程重启即生效、出问题能直接 cat 出来看。
//   缺点：不支持嵌套结构 —— 目前也不需要。
// ==============================================================================

use std::collections::HashMap;
use std::sync::Mutex;

static CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn file_path() -> std::path::PathBuf {
    // filesDir 比 cacheDir 合适：后者会被系统在空间不足时清掉，
    // 而用户设置被清掉是很糟的体验。
    let base = crate::android_window::files_dir()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/data/data/com.bli.rs/files"));
    let _ = std::fs::create_dir_all(&base);
    base.join("settings.conf")
}

fn load() -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(text) = std::fs::read_to_string(file_path()) {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    map
}

fn with_cache<T>(f: impl FnOnce(&mut HashMap<String, String>) -> T) -> T {
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(load());
    }
    f(guard.as_mut().unwrap())
}

fn persist(map: &HashMap<String, String>) {
    let mut out = String::new();
    for (k, v) in map {
        out.push_str(&format!("{k}={v}\n"));
    }
    let _ = std::fs::write(file_path(), out);
}

pub fn get_bool(key: &str, default: bool) -> bool {
    with_cache(|m| match m.get(key) {
        Some(v) => v == "true" || v == "1",
        None => default,
    })
}

pub fn set_bool(key: &str, value: bool) {
    with_cache(|m| {
        m.insert(key.to_string(), value.to_string());
        persist(m);
    });
}

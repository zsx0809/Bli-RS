// ==============================================================================
// Wbi 签名
//
// 移植自 PiliPlus `lib/utils/wbi_sign.dart`。
// B 站对部分 REST 接口要求 w_rid + wts 查询参数，算法：
//   1. 从 /x/web-interface/nav 拿到 img_url / sub_url；
//   2. 取两个 URL 的文件名（不含扩展名）拼接，得到 64 字符原始串；
//   3. 按 MIXIN_KEY_ENC_TAB 重排取前 32 位，得到 mixin_key；
//   4. 请求参数按 key 字典序排列，值过滤掉 !'()* 四个字符；
//   5. w_rid = md5(query + mixin_key)
//
// mixin_key 按天变化，PiliPlus 用「同一天则复用缓存」的策略，这里照搬。
// ==============================================================================

use std::collections::BTreeMap;
use std::sync::Mutex;

use md5::{Digest, Md5};

/// img_key 与 sub_key 的字符重排表。这不是「加密」，是 B 站的混淆手段。
const MIXIN_KEY_ENC_TAB: [usize; 32] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13,
];

/// 进程内的 mixin_key 缓存：(mixin_key, 获取时的 UTC 天数)
static MIXIN_CACHE: Mutex<Option<(String, i64)>> = Mutex::new(None);

pub fn get_mixin_key(orig: &str) -> String {
    // orig 是 64 字符的 img_key + sub_key；重排表最大索引 58，不会越界。
    // 但外部输入不可信，用 chars().nth 而不是直接索引字节，
    // 避免未来 B 站换 URL 格式时 panic。
    let chars: Vec<char> = orig.chars().collect();
    MIXIN_KEY_ENC_TAB
        .iter()
        .filter_map(|&i| chars.get(i))
        .collect()
}

/// 当前 UTC 天数（用于判断 mixin_key 是否过期）
fn utc_day() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86400) as i64)
        .unwrap_or(0)
}

/// 从 URL 里取文件名（去扩展名）
/// 例：https://i0.hdslb.com/bfs/wbi/7cd0849413....png -> 7cd0849413...
fn file_stem(url: &str) -> &str {
    let last = url.rsplit('/').next().unwrap_or(url);
    match last.rfind('.') {
        Some(pos) => &last[..pos],
        None => last,
    }
}

/// 用已知的 img_key/sub_key 计算签名，填充 wts 与 w_rid。
///
/// 参数用 BTreeMap 而非 HashMap：签名要求按 key 字典序拼接，
/// BTreeMap 天然有序，省掉一次显式排序，也避免排序键不一致的坑。
pub fn sign_with_key(params: &mut BTreeMap<String, String>, mixin_key: &str) {
    let wts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    params.insert("wts".into(), wts.to_string());

    // 拼接：key=value&key=value...，值需过滤 !'()* 四个字符
    // （B 站服务端也是这么算的，少过滤一个字符就会签名不匹配）
    let query = params
        .iter()
        .map(|(k, v)| {
            let cleaned: String = v.chars().filter(|c| !matches!(c, '!' | '\'' | '(' | ')' | '*')).collect();
            format!("{}={}", urlencode(k), urlencode(&cleaned))
        })
        .collect::<Vec<_>>()
        .join("&");

    let mut hasher = Md5::new();
    hasher.update(query.as_bytes());
    hasher.update(mixin_key.as_bytes());
    let w_rid = format!("{:x}", hasher.finalize());

    params.insert("w_rid".into(), w_rid);
}

/// 最小 percent-encoding。只编码会破坏 query 结构的字符，
/// 与 Dart 的 Uri.encodeComponent 行为对齐（它保留 -_.!~*'() 等）。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// 用缓存（若当天有效）或新拉取的方式拿到 mixin_key。
pub async fn mixin_key(client: &reqwest::Client, base: &str) -> Result<String, String> {
    // 1. 查缓存
    if let Ok(guard) = MIXIN_CACHE.lock() {
        if let Some((key, day)) = guard.as_ref() {
            if *day == utc_day() {
                return Ok(key.clone());
            }
        }
    }

    // 2. 拉 nav 接口
    let url = format!("{}/x/web-interface/nav", base);
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("nav 请求失败: {e}"))?;
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("nav 解析失败: {e}"))?;

    let wbi = &json["data"]["wbi_img"];
    let img = wbi["img_url"].as_str().ok_or("nav 响应缺少 wbi_img.img_url")?;
    let sub = wbi["sub_url"].as_str().ok_or("nav 响应缺少 wbi_img.sub_url")?;

    let raw = format!("{}{}", file_stem(img), file_stem(sub));
    let key = get_mixin_key(&raw);

    // 3. 写缓存
    if let Ok(mut guard) = MIXIN_CACHE.lock() {
        *guard = Some((key.clone(), utc_day()));
    }

    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixin_key_matches_known_vector() {
        // PiliPlus 实测值：img_key/sub_key 拼成的原始串经重排后得到
        // ea1db124af3c7062474693fa704f4ff8
        // 这里用同一对 key 验证算法一致性。
        let img = "7cd084941338484aae1ad9425b84077c";
        let sub = "4932caff0ff746eab6f01bf08b70ac45";
        let key = get_mixin_key(&format!("{img}{sub}"));
        assert_eq!(key.len(), 32, "mixin_key 长度必须是 32");
        assert_eq!(key, "ea1db124af3c7062474693fa704f4ff8");
    }

    #[test]
    fn file_stem_strips_dir_and_ext() {
        assert_eq!(
            file_stem("https://i0.hdslb.com/bfs/wbi/7cd084941338484aae1ad9425b84077c.png"),
            "7cd084941338484aae1ad9425b84077c"
        );
        assert_eq!(file_stem("noext"), "noext");
    }

    #[test]
    fn sign_is_deterministic_for_same_params() {
        let mut a = BTreeMap::new();
        a.insert("foo".to_string(), "bar".to_string());
        let mut b = a.clone();
        sign_with_key(&mut a, "testkey");
        // wts 是秒级时间戳，同一秒内两次调用应一致
        sign_with_key(&mut b, "testkey");
        assert!(a.contains_key("w_rid"));
        assert_eq!(a.get("w_rid"), b.get("w_rid"));
    }

    #[test]
    fn encodes_special_chars() {
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("中"), "%E4%B8%AD");
    }
}

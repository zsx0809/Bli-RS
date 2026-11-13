// ==============================================================================
// 播放地址 (playurl)
//
// 移植自 PiliPlus:
//   - lib/http/api.dart       Api.ugcUrl
//   - lib/http/video.dart     VideoHttp.videoUrl
//   - lib/models/video/play/url.dart
//
// 关键概念 —— DASH 是音视频分离的：
//   fnval=4048 时 B 站返回 `dash` 字段，里面 video[] 和 audio[] 是两组
//   独立的轨道，各有自己的 URL。播放器必须同时打开两路并自己对齐时间轴。
//
//   这一点跟 MP4 单文件完全不同，是纯 native 实现里最容易低估的地方。
//
// 为什么不用 fnval=1（MP4 合并流）：
//   合并流最高只到 360P/480P，且 B 站早在 2019 年后就不再对 Web 端提供
//   高清晰度的合并流了。想要 1080P 必须走 DASH。
// ==============================================================================

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::api;
use crate::client::BiliClient;
use crate::{Error, Result};

/// 一条可用的媒体轨道。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    /// 清晰度 id。视频：127=8K, 120=4K, 116=1080P60, 80=1080P, 64=720P, 32=480P, 16=360P
    /// 音频：30280=192K, 30232=132K, 30216=64K
    pub id: u64,
    /// mime，例如 "video/mp4" / "audio/mp4"
    pub mime: String,
    /// 编码，例如 "avc1.640032" / "hev1.1.6.L150.90" / "mp4a.40.2"
    pub codecs: String,
    /// 码率（bps）
    pub bandwidth: u64,
    /// 视频宽/高（音频为 None）
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// 帧率描述，例如 "60" / "30"
    pub frame_rate: Option<String>,
    /// 备用地址列表，第一个为主
    pub urls: Vec<String>,
}

impl Track {
    /// 主地址
    pub fn primary_url(&self) -> Option<&str> {
        self.urls.first().map(|s| s.as_str())
    }

    /// 是否是 AVC（H.264）。
    ///
    /// 优先选 AVC 而不是 HEVC：AVC 的硬解支持面广得多，
    /// 老设备上 HEVC 常常只能软解，直接违背这个项目的低功耗目标。
    pub fn is_avc(&self) -> bool {
        self.codecs.starts_with("avc")
    }
}

/// DASH 播放信息
#[derive(Debug, Clone, Default)]
pub struct DashPlayInfo {
    /// 时长（秒）
    pub duration: u64,
    pub video: Vec<Track>,
    pub audio: Vec<Track>,
}

impl DashPlayInfo {
    /// 选一条视频轨。
    ///
    /// 策略（按优先级）：
    ///   1. 编码优先 AVC —— 硬解支持面最广
    ///   2. 在该编码内取 `max_height` 以内的最高清晰度
    ///   3. 同等清晰度取码率最高的
    ///
    /// `max_height = None` 表示不限制（取最高）。
    pub fn pick_video(&self, max_height: Option<u32>) -> Option<&Track> {
        let cap = max_height.unwrap_or(u32::MAX);

        let usable = || {
            self.video
                .iter()
                .filter(|t| t.height.unwrap_or(0) <= cap && t.primary_url().is_some())
        };

        // 先试 AVC
        if let Some(best) = usable()
            .filter(|t| t.is_avc())
            .max_by_key(|t| (t.height.unwrap_or(0), t.bandwidth))
        {
            return Some(best);
        }

        // 没有 AVC 就退回任意编码里最高的
        usable().max_by_key(|t| (t.height.unwrap_or(0), t.bandwidth))
    }

    /// 选一条音频轨：取码率最高的。
    ///
    /// 不区分编码是因为 B 站音频基本都是 AAC（mp4a.40.2），
    /// 偶尔有 Dolby（ec-3），但那种设备支持面窄，且码率排序天然会选中它。
    pub fn pick_audio(&self) -> Option<&Track> {
        self.audio
            .iter()
            .filter(|t| t.primary_url().is_some())
            .max_by_key(|t| t.bandwidth)
    }
}

impl BiliClient {
    /// 拉取播放地址。
    ///
    /// `cid` 来自视频详情接口；`bvid` 与 `aid` 二选一即可（这里用 bvid）。
    pub async fn play_url(&self, bvid: &str, cid: u64) -> Result<DashPlayInfo> {
        let mut params = BTreeMap::new();
        params.insert("bvid".into(), bvid.to_string());
        params.insert("cid".into(), cid.to_string());
        // qn=127 请求最高清晰度；实际返回哪条由 fnval + 账号权限决定
        params.insert("qn".into(), "127".into());
        params.insert("fnval".into(), api::FNVAL_DASH_ALL.to_string());
        params.insert("fnver".into(), "0".into());
        params.insert("fourk".into(), "1".into());
        // 未登录也想拿到 1080P —— B 站对 try_look 有宽松处理
        params.insert("try_look".into(), "1".into());
        // 这两个是 Web 播放器的埋点参数，缺了可能被风控
        params.insert("gaia_source".into(), "pre-load".into());
        params.insert("isGaiaAvoided".into(), "true".into());
        params.insert("web_location".into(), "1315873".into());

        let json = self.get_signed(api::PLAY_URL, params).await?;

        let dash = &json["data"]["dash"];
        if dash.is_null() {
            // 没有 dash 说明只给了 durl（合并流），或接口报错
            let msg = json["message"].as_str().unwrap_or("响应里没有 dash 字段");
            return Err(Error::Api {
                code: json["code"].as_i64().unwrap_or(-1),
                message: msg.to_string(),
            });
        }

        let duration = json["data"]["timelength"].as_u64().unwrap_or(0) / 1000;

        Ok(DashPlayInfo {
            duration,
            video: parse_tracks(&dash["video"]),
            audio: parse_tracks(&dash["audio"]),
        })
    }

    /// 取视频详情里的 cid。
    pub async fn video_cid(&self, bvid: &str) -> Result<u64> {
        let mut params = BTreeMap::new();
        params.insert("bvid".into(), bvid.to_string());

        let json = self.get_signed(api::VIDEO_VIEW, params).await?;
        json["data"]["cid"]
            .as_u64()
            .ok_or_else(|| Error::Decode("view 响应里没有 cid".into()))
    }
}

fn parse_tracks(arr: &serde_json::Value) -> Vec<Track> {
    let Some(list) = arr.as_array() else {
        return Vec::new();
    };

    list.iter()
        .filter_map(|v| {
            // 主地址在 baseUrl，备用在 backupUrl 数组
            let mut urls = Vec::new();
            if let Some(u) = v["baseUrl"].as_str() {
                if !u.is_empty() {
                    urls.push(u.to_string());
                }
            }
            if let Some(backups) = v["backupUrl"].as_array() {
                for b in backups {
                    if let Some(u) = b.as_str() {
                        if !u.is_empty() {
                            urls.push(u.to_string());
                        }
                    }
                }
            }
            if urls.is_empty() {
                return None;
            }

            Some(Track {
                id: v["id"].as_u64().unwrap_or(0),
                mime: v["mimeType"].as_str().unwrap_or("").to_string(),
                codecs: v["codecs"].as_str().unwrap_or("").to_string(),
                bandwidth: v["bandwidth"].as_u64().unwrap_or(0),
                width: v["width"].as_u64().map(|x| x as u32),
                height: v["height"].as_u64().map(|x| x as u32),
                frame_rate: v["frameRate"].as_str().map(|s| s.to_string()),
                urls,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(id: u64, h: u32, bw: u64, codec: &str) -> Track {
        Track {
            id,
            mime: "video/mp4".into(),
            codecs: codec.into(),
            bandwidth: bw,
            width: Some(h * 16 / 9),
            height: Some(h),
            frame_rate: None,
            urls: vec!["https://x/y".into()],
        }
    }

    #[test]
    fn prefers_avc_over_hevc_at_same_height() {
        let info = DashPlayInfo {
            video: vec![
                mk(80, 1080, 3_000_000, "avc1.640032"),
                mk(80, 1080, 1_500_000, "hev1.1.6.L150.90"),
            ],
            ..Default::default()
        };
        let picked = info.pick_video(None).unwrap();
        assert!(picked.is_avc());
    }

    #[test]
    fn respects_height_cap() {
        let info = DashPlayInfo {
            video: vec![
                mk(120, 2160, 9_000_000, "avc1.640033"),
                mk(80, 1080, 3_000_000, "avc1.640032"),
                mk(64, 720, 1_500_000, "avc1.64001F"),
            ],
            ..Default::default()
        };
        let picked = info.pick_video(Some(1080)).unwrap();
        assert_eq!(picked.height, Some(1080));
    }

    #[test]
    fn falls_back_to_hevc_when_no_avc() {
        let info = DashPlayInfo {
            video: vec![mk(80, 1080, 1_500_000, "hev1.1.6.L150.90")],
            ..Default::default()
        };
        assert!(!info.pick_video(None).unwrap().is_avc());
    }

    #[test]
    fn picks_highest_audio_bitrate() {
        let info = DashPlayInfo {
            audio: vec![
                Track {
                    id: 30216,
                    mime: "audio/mp4".into(),
                    codecs: "mp4a.40.2".into(),
                    bandwidth: 64_000,
                    width: None,
                    height: None,
                    frame_rate: None,
                    urls: vec!["https://a".into()],
                },
                Track {
                    id: 30280,
                    mime: "audio/mp4".into(),
                    codecs: "mp4a.40.2".into(),
                    bandwidth: 192_000,
                    width: None,
                    height: None,
                    frame_rate: None,
                    urls: vec!["https://b".into()],
                },
            ],
            ..Default::default()
        };
        assert_eq!(info.pick_audio().unwrap().id, 30280);
    }

    #[test]
    fn skips_tracks_without_url() {
        let mut t = mk(80, 1080, 3_000_000, "avc1.640032");
        t.urls.clear();
        let info = DashPlayInfo {
            video: vec![t],
            ..Default::default()
        };
        assert!(info.pick_video(None).is_none());
    }
}

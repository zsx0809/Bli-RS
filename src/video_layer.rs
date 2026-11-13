// ==============================================================================
// ASurfaceControl —— 独立视频层
//
// 为什么要用这个：
//   Slint 的 Image 只接受 CPU 像素缓冲（from_rgba8），没有硬件缓冲/外部
//   纹理入口。若把解码结果回读到 CPU 再上传，1080p30 约 250MB/s 的拷贝，
//   手机上直接发热 —— 正好撞在这个项目要根除的问题上。
//
//   ASurfaceControl 让我们在 Activity 窗口下挂一个**独立的合成层**：
//   MediaCodec 把画面直接解码进这个层的 buffer，SurfaceFlinger 负责合成，
//   全程 GPU、零回读。Slint 完全不知道视频的存在，照常渲染自己的 UI
//   （控制条、弹幕都可以叠在视频上面）。
//
// 为什么视频不做 Slint 的圆角/变换：
//   不需要。视频本来就是一块矩形区域 —— 竖屏在顶部，横屏铺满。
//   圆角和动画是 UI 元素的事，视频层不需要参与。
//
// API 限制：
//   ASurfaceControl_* 全部是 API 29+。min_sdk 是 26，所以需要运行时降级。
//   API < 29 时这块模块整体不可用，播放器要退回"不支持内嵌视频"。
// ==============================================================================

use std::os::raw::{c_char, c_int, c_void};

#[repr(C)]
pub struct ANativeWindow {
    _p: [u8; 0],
}
#[repr(C)]
pub struct ASurfaceControl {
    _p: [u8; 0],
}
#[repr(C)]
pub struct ASurfaceTransaction {
    _p: [u8; 0],
}

/// 与 android/rect.h 的 ARect 对应
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ARect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl ARect {
    pub fn new(l: i32, t: i32, r: i32, b: i32) -> Self {
        Self {
            left: l,
            top: t,
            right: r,
            bottom: b,
        }
    }

    pub fn size(w: i32, h: i32) -> Self {
        Self::new(0, 0, w, h)
    }
}

#[link(name = "android")]
extern "C" {
    fn ASurfaceControl_createFromWindow(
        parent: *mut ANativeWindow,
        debug_name: *const c_char,
    ) -> *mut ASurfaceControl;
    fn ASurfaceControl_release(sc: *mut ASurfaceControl);

    fn ASurfaceTransaction_create() -> *mut ASurfaceTransaction;
    fn ASurfaceTransaction_delete(t: *mut ASurfaceTransaction);
    fn ASurfaceTransaction_apply(t: *mut ASurfaceTransaction);

    fn ASurfaceTransaction_setVisibility(
        t: *mut ASurfaceTransaction,
        sc: *mut ASurfaceControl,
        visible: i8,
    );
    fn ASurfaceTransaction_setZOrder(t: *mut ASurfaceTransaction, sc: *mut ASurfaceControl, z: i32);
    fn ASurfaceTransaction_setGeometry(
        t: *mut ASurfaceTransaction,
        sc: *mut ASurfaceControl,
        source: *const ARect,
        destination: *const ARect,
        transform: i32,
    );
}

/// 一个挂在窗口上的视频层。
///
/// 生命周期：创建后一直存活，通过 `set_geometry` 改位置/大小。
/// drop 时释放。
pub struct VideoLayer {
    control: *mut ASurfaceControl,
    // 父窗口的引用计数由 Activity 持有，这里只是借用，不释放
    _parent: *mut ANativeWindow,
}

// SAFETY: ASurfaceControl 的操作本身是线程安全的（SurfaceFlinger 侧加锁），
// 我们只要求在同一时刻只有一处调用 transaction。
unsafe impl Send for VideoLayer {}

impl VideoLayer {
    /// 在给定的父窗口下创建一个视频层。
    ///
    /// `parent` 是 Activity 的 ANativeWindow（`ANativeWindow_fromSurface`
    /// 或 android-activity 直接给的 window 指针）。
    ///
    /// 返回 None 表示创建失败（多半是 API < 29）。
    pub fn new(parent: *mut ANativeWindow, name: &str) -> Option<Self> {
        let Ok(cname) = std::ffi::CString::new(name) else {
            return None;
        };

        let control = unsafe { ASurfaceControl_createFromWindow(parent, cname.as_ptr()) };
        if control.is_null() {
            return None;
        }

        Some(Self {
            control,
            _parent: parent,
        })
    }

    /// 设置这一层的显示区域。
    ///
    /// `src` 是视频自身的像素尺寸（决定缩放基准），
    /// `dst` 是它在父窗口坐标系里的位置和大小。
    ///
    /// 竖屏时 dst 可以是顶部一块矩形，横屏时可以是全屏 —— 调用方决定。
    pub fn set_geometry(&self, src: ARect, dst: ARect) {
        unsafe {
            let t = ASurfaceTransaction_create();
            if t.is_null() {
                return;
            }
            ASurfaceTransaction_setGeometry(t, self.control, &src, &dst, 0);
            ASurfaceTransaction_setVisibility(t, self.control, 1);
            ASurfaceTransaction_apply(t);
            ASurfaceTransaction_delete(t);
        }
    }

    /// 显示 / 隐藏（不销毁，便于切后台时省电）
    pub fn set_visible(&self, visible: bool) {
        unsafe {
            let t = ASurfaceTransaction_create();
            if t.is_null() {
                return;
            }
            ASurfaceTransaction_setVisibility(t, self.control, if visible { 1 } else { 0 });
            ASurfaceTransaction_apply(t);
            ASurfaceTransaction_delete(t);
        }
    }

    /// 把这一层置于父窗口内所有层的最上面。
    ///
    /// 视频层默认就压在 Slint 的 UI 之上（z 越大越靠上）。
    /// 如果将来要让 UI 控制条浮在视频上，需要给控制条也开独立层，
    /// 或者把视频层 z 调负 —— 但当前设计是控制条由 Slint 画在它自己
    /// 的 buffer 里，视觉上会被视频层遮住。这一点在接 UI 时需要重新考虑。
    pub fn bring_to_front(&self) {
        unsafe {
            let t = ASurfaceTransaction_create();
            if t.is_null() {
                return;
            }
            ASurfaceTransaction_setZOrder(t, self.control, 0x7fff_ffff);
            ASurfaceTransaction_apply(t);
            ASurfaceTransaction_delete(t);
        }
    }

    /// 拿到底层指针，供 `ANativeWindow_fromSurface` 之类的 API 使用。
    ///
    /// MediaCodec 需要的是一个 ANativeWindow 来输出画面。
    /// ASurfaceControl 本身不是 ANativeWindow，需要通过
    /// `ANativeWindow_fromSurface` 从 Java Surface 拿 —— 这是纯 native
    /// 场景下的一个空缺，见下方说明。
    pub fn raw(&self) -> *mut c_void {
        self.control as *mut c_void
    }
}

impl Drop for VideoLayer {
    fn drop(&mut self) {
        unsafe {
            ASurfaceControl_release(self.control);
        }
    }
}

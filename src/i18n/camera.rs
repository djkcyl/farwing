//! Remote camera texts (`docs/protocol/camera-design.md`).

pub(crate) struct Camera {
    pub in_use: &'static str,
    pub privacy_denied: &'static str,
    pub not_found: &'static str,
    pub removed: &'static str,
    pub no_mode: &'static str,
    pub open_failed: &'static str,
    pub read_failed: &'static str,
    pub synthetic_name: &'static str,
}

pub(crate) const ZH_CN: Camera = Camera {
    in_use: "摄像头正被其他程序使用",
    privacy_denied: "Windows 不允许桌面程序使用摄像头（设置 → 隐私和安全性 → 摄像头）",
    not_found: "找不到这个摄像头",
    removed: "摄像头已断开",
    no_mode: "摄像头没有可用的画面格式",
    open_failed: "无法打开摄像头",
    read_failed: "读取摄像头画面失败",
    synthetic_name: "测试画面",
};

pub(crate) const EN: Camera = Camera {
    in_use: "The camera is in use by another app",
    privacy_denied: "Windows doesn't let desktop apps use the camera (Settings → Privacy & security → Camera)",
    not_found: "Camera not found",
    removed: "The camera was disconnected",
    no_mode: "The camera offers no usable picture format",
    open_failed: "Couldn't open the camera",
    read_failed: "Couldn't read from the camera",
    synthetic_name: "Test picture",
};

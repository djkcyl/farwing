//! VirtualAudioDriverPolicyResponse errors shared by the host and viewer.
//! The wire values are mapped independently of assistance identity.
pub(crate) const COMPONENT_UNAVAILABLE: i32 = 0x0e00_3001;
pub(crate) const NOT_ALLOWED: i32 = 0x0e00_3006;
pub(crate) const INITIALIZATION_FAILED: i32 = 0x0e00_3012;

pub(crate) fn failure_message(code: i32) -> String {
    let reason = match code {
        COMPONENT_UNAVAILABLE => {
            "被控端虚拟音频组件不可用，请在被控端连接设置中检查虚拟声卡是否已安装、是否被 Windows 阻止加载；具体原因见被控端诊断信息"
        }
        NOT_ALLOWED => "被控端未允许麦克风输入，或本次会话许可已失效",
        INITIALIZATION_FAILED => "被控端虚拟音频初始化失败，请查看被控端诊断信息",
        _ => "远端麦克风请求失败",
    };
    format!("{reason}（{code:#x}）")
}

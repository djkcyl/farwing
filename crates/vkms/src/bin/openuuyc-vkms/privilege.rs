//! 打开设备以后放掉全部能力。
//!
//! 单元只给了 `CAP_SYS_MODULE`（外加放权用的 `CAP_SETPCAP`），而它只在
//! [`crate::configfs::Instance::open`] 里加载 vkms 的那一下用得到——设备是启动时定一次的，
//! 之后再也不会 modprobe。可这个进程接下来要一直处理 0666 套接字上的不可信输入，
//! 而沙箱里 `/tmp` 是可写的：万一它被人拿到代码执行，手里还有 `CAP_SYS_MODULE` 的话，
//! 往 `/tmp` 写一个 `.ko` 再 `finit_module` 就进了内核（没有强制模块签名的发行版，比如
//! Arch 默认就是这样）。
//!
//! 放掉以后它还能做的只剩 uid 0 的文件权限：configfs 里 vkms 的目录和属性都归 root、
//! 权限是属主可写，写它们走的是普通的属主权限检查，不需要任何能力；读 `/sys/class/drm`、
//! 连系统总线问 logind 也不需要。
use crate::note;

/// `_LINUX_CAPABILITY_VERSION_3`：64 位能力集，分两段 32 位。
const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

#[repr(C)]
struct Header {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Data {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// 清空 bounding、effective、permitted、inheritable 四个集合。
///
/// bounding 先清：那一步要 `CAP_SETPCAP` 还在 effective 里。没有它（比如手动以 root
/// 起来调试）时清不掉 bounding，但只要 permitted 清空了，单元里的 `NoNewPrivileges`
/// 保证 exec 也拿不回来，所以那种情况只记一行。
pub fn drop_all() {
    let mut kept = Vec::new();
    for capability in 0..64 {
        // SAFETY: PR_CAPBSET_DROP 只读这个整数参数。
        let result = unsafe { libc::prctl(libc::PR_CAPBSET_DROP, capability, 0, 0, 0) };
        if result != 0 {
            match std::io::Error::last_os_error().raw_os_error() {
                // 超出本内核认识的能力编号，后面的都不存在。
                Some(libc::EINVAL) => break,
                _ => kept.push(capability),
            }
        }
    }
    let mut header = Header {
        version: CAPABILITY_VERSION_3,
        pid: 0,
    };
    let data = [Data::default(); 2];
    // SAFETY: 头和两段数据都按内核的 v3 布局排好，pid 0 表示本线程。capset 只作用于调用
    // 它的线程，所以要在开任何线程之前做（main 里打开设备之后、accept 之前）。
    let result = unsafe {
        libc::syscall(
            libc::SYS_capset,
            std::ptr::from_mut(&mut header),
            data.as_ptr(),
        )
    };
    if result != 0 {
        note(&format!(
            "无法放掉能力：{}",
            std::io::Error::last_os_error()
        ));
    }
    if !kept.is_empty() && bounding_set().is_some_and(|set| set != 0) {
        note(&format!(
            "bounding 集合里仍留着 {} 项能力（没有 CAP_SETPCAP），effective/permitted 已清空",
            kept.len()
        ));
    }
}

/// `/proc/self/status` 里的 CapBnd。
fn bounding_set() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("CapBnd:"))
        .and_then(|value| u64::from_str_radix(value.trim(), 16).ok())
}

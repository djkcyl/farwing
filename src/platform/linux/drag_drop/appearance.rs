//! Drag-image metadata and the local identity of a drag.
//!
//! 拖起来跟着指针走的那张图，Linux 这一侧是**读不到**的：Wayland 里它是合成器按源程序
//! 给的图标自己画的一块面（画在指针旁边，谁都不给读），X11 那边它是源程序自己的一个窗口。
//! 但也不必读——Windows 端本来就是**自己拼**一张（走 Shell 的文件图标），这里照拼：
//! 是图片就用文件自己的缩略图（拖一张照片出去，看着就该是那张照片），其余按类型从图标
//! 主题里取一枚，选了好几个就错开叠两张。拼不出来就报错，调用方会发一张空图，那边照旧
//! 显示它自己的图标——总比把一张 16×16 放大糊上去强。
//!
//! 身份（`identify`）只服务于「拖回来」，Linux 不提供（`super::RETURN_CAPABLE`）；
//! `decorate` 是 Windows 的 OLE 拖放助手要的那一步，Linux 上那个「本地拖动」是 XDND
//! 交接窗口假装的，图由源程序自己画，没这一步。
pub(crate) use crate::protocol::drag_drop::DragImage;
use anyhow::{Context as _, Result, bail, ensure};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// 单张图的边长，跟 Windows 端 Shell 给的 64 对齐。
const SIDE: u32 = 64;
/// 多选时叠出来的画布，比单张大些，好让错开的那两张露出来。
const STACK: u32 = 80;
/// 源图多大都只是浪费内存：反正要缩到 `SIDE`。
const MAX_SOURCE: u32 = 4096;
/// 位图图标至少要有这么大才值得用，再小放大出来是一团糊。
const MIN_BITMAP: u32 = 48;
/// 图标工具的超时。这两张图只是「拖起来好看一点」，而图标主题和文件都可能待在网络
/// 盘或 FUSE 上：`gio` 一卡住，整次拖出就跟着卡在「正在准备文件」上（图是在发布
/// 文件清单之前生成的）。超时就杀掉，当没有这张图。
const TOOL_LIMIT: Duration = Duration::from_secs(2);

/// 跑一个只看输出的子进程，把它的 stdout 拿回来；超过 `TOOL_LIMIT` 就杀掉它。
///
/// 「工具」可能是个包装脚本（发行版的 `gio`、用户自己垫的同名脚本），真正干活的是
/// 它起的孙进程，而孙进程继承着同一根 stdout：只杀直接子进程，管道的写端还开着，
/// 读线程就要等到孙进程自己退出。所以让它自成一个进程组、超时整组杀掉，读线程也
/// 只等到时限为止，等不到就不要了（线程自己会在写端全关时退出）。
fn capture(command: &mut std::process::Command, tool: &str) -> Result<Vec<u8>> {
    use std::io::Read as _;
    use std::os::unix::process::CommandExt as _;
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .with_context(|| format!("调用 {tool} 失败"))?;
    // 边跑边读：等它退出了再读，输出填满管道时两边一起卡住。
    let mut pipe = child.stdout.take().context("子进程没有标准输出")?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut data = Vec::new();
        let _ = pipe.read_to_end(&mut data);
        let _ = sender.send(data);
    });
    let deadline = Instant::now() + TOOL_LIMIT;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("等待 {tool} 失败"))?
        {
            break Some(status);
        }
        if Instant::now() >= deadline {
            // 进程组号就是它自己的 pid（`process_group(0)`）。
            // SAFETY: 只是发信号。子进程还没被回收（`wait` 在这之后），它的 pid
            // 连同这个组号都不会被别人复用。
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            child.wait().with_context(|| format!("回收 {tool} 失败"))?;
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let status = status.with_context(|| format!("{tool} 没有在时限内给出结果"))?;
    // 子进程退了，可它留下的孙进程也许还攥着 stdout：同样只等到时限，等不到就
    // 不要了。这时子进程已经回收，组号随时可能被复用，不再去杀那个组。
    // 刚好卡着时限退出的，也给读线程留一点收尾的工夫。
    let wait = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(100));
    let data = receiver
        .recv_timeout(wait)
        .ok()
        .with_context(|| format!("{tool} 没有在时限内给出结果"))?;
    ensure!(status.success(), "{tool} 没能给出结果");
    Ok(data)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub token: u64,
    pub drag: u64,
}

pub fn identify(_data: &super::Files, _identity: Identity) -> Result<()> {
    bail!("Linux 版暂不支持拖回原拖动")
}

pub fn decorate(_data: &super::Files, _image: &DragImage) -> Result<()> {
    bail!("X11 拖放没有拖动预览图")
}

/// 拖动预览图。跑在阻塞线程上（会去翻图标主题、可能还要起 rsvg 画一张）。
pub fn selection(paths: &[PathBuf]) -> Result<DragImage> {
    ensure!(!paths.is_empty(), "文件选择为空");
    if paths.len() == 1 {
        return icon(&paths[0]);
    }
    // 叠的是前三项里拼得出来的那几张：哪一项拼不出来就跳过它，全都拼不出来才算没图。
    let mut icons = Vec::new();
    let mut failure = None;
    for path in paths.iter().take(3) {
        match icon(path) {
            Ok(icon) => icons.push(icon),
            Err(error) => {
                failure.get_or_insert(error);
            }
        }
    }
    if icons.is_empty() {
        return Err(failure.unwrap_or_else(|| anyhow::anyhow!("文件选择为空")));
    }
    Ok(stacked(&icons))
}

/// 一个文件拖起来该是什么样：是图片就用它自己，其余去图标主题里找它这一类的图标。
fn icon(path: &Path) -> Result<DragImage> {
    if let Some(preview) = preview(path) {
        return Ok(preview);
    }
    let names = type_icons(path)?;
    // 候选是有先后的，但装着的主题不一定每一枚都有（`application-toml` 这种就常常缺），
    // 所以按顺序找，第一个真画得出来的算数：矢量图画不出来（没装 `rsvg-convert`）
    // 就用同名的位图，同名的都不行再看下一个名字。
    let mut failure = None;
    for name in &names {
        for file in lookup(name).unwrap_or_default() {
            match render(&file) {
                Ok(image) => return Ok(image),
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
    }
    match failure {
        Some(error) => Err(error),
        None => bail!("图标主题里没有这个类型的图（{}）", names.join("、")),
    }
}

/// 图片文件自己的缩略图。读不动（不是图片、太大、这种格式没编进来）就当没有。
fn preview(path: &Path) -> Option<DragImage> {
    let mut reader = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SOURCE);
    limits.max_image_height = Some(MAX_SOURCE);
    // 解码要按原始像素分配，放开这道闸就能被一张几亿像素的图拖死。
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?.into_rgba8();
    Some(fit(&image))
}

/// 这个文件算哪几类图标，交给 GIO 定（它带着 MIME 库和主题）：`standard::icon` 给的
/// 是有先后的候选，越靠前越是这一类的准确名字；`-symbolic` 那几枚是单色版本，不要。
fn type_icons(path: &Path) -> Result<Vec<String>> {
    let mut command = std::process::Command::new("gio");
    command
        .args(["info", "-a", "standard::icon", "--"])
        .arg(path);
    let stdout = capture(&mut command, "gio")?;
    let text = String::from_utf8_lossy(&stdout);
    let line = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("standard::icon:"))
        .context("gio 没有给出图标名")?;
    let names: Vec<String> = line
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.ends_with("-symbolic"))
        .map(str::to_owned)
        .collect();
    ensure!(!names.is_empty(), "gio 给出的图标名是空的");
    Ok(names)
}

/// 在装着的图标主题里找这个名字的文件，按该试的先后排好。
///
/// 矢量图在前：多大都画得清楚。可它要靠 `rsvg-convert` 才画得出来，没装就画不了，
/// 所以后面再跟一张同名的位图垫底。位图要挑够大的——主题里往往只有 16×16，放大到
/// 64 只会是一团糊，那就干脆不要，让主控显示它自己的图标。两样都没有时报错。
fn lookup(name: &str) -> Result<Vec<PathBuf>> {
    // `scalable` 目录里的矢量图优先于尺寸目录里的（`true` 排在 `false` 后面）。
    let mut vector: Option<(bool, PathBuf)> = None;
    let mut bitmap: Option<(u32, PathBuf)> = None;
    for root in roots() {
        let Ok(themes) = std::fs::read_dir(&root) else {
            continue;
        };
        for theme in themes.flatten() {
            let Ok(entries) = std::fs::read_dir(theme.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let directory = entry.file_name().to_string_lossy().into_owned();
                // 尺寸目录名就是尺寸（`64x64`、`128x128@2`），`scalable` 是矢量那一份。
                let scalable = directory == "scalable";
                let size = parse_size(&directory);
                if !scalable && size.is_none() {
                    continue;
                }
                if vector.as_ref().is_none_or(|(known, _)| scalable && !known)
                    && let Some(file) = find_in(&entry.path(), name, "svg")
                {
                    vector = Some((scalable, file));
                }
                if let Some(size) = size
                    && bitmap.as_ref().is_none_or(|(known, _)| *known < size)
                    && let Some(file) = find_in(&entry.path(), name, "png")
                {
                    bitmap = Some((size, file));
                }
            }
        }
    }
    let mut files: Vec<PathBuf> = vector.into_iter().map(|(_, file)| file).collect();
    files.extend(
        bitmap
            .filter(|(size, _)| *size >= MIN_BITMAP)
            .map(|(_, file)| file),
    );
    ensure!(!files.is_empty(), "图标主题里没有够大的 {name}");
    Ok(files)
}

/// 图标主题可能待着的几个地方。
fn roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    if let Some(home) = data_home {
        roots.push(home.join("icons"));
    }
    roots.push(PathBuf::from("/usr/share/icons"));
    for directory in std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into())
        .split(':')
    {
        if !directory.is_empty() {
            roots.push(Path::new(directory).join("icons"));
        }
    }
    roots
}

/// 一个尺寸目录（或者 `scalable`）里有没有这个名字、这种格式的图。
fn find_in(directory: &Path, name: &str, extension: &str) -> Option<PathBuf> {
    for context in std::fs::read_dir(directory).ok()?.flatten() {
        let file = context.path().join(format!("{name}.{extension}"));
        if file.is_file() {
            return Some(file);
        }
    }
    None
}

/// `64x64` 是 64；`scalable` 之类认不出来。
fn parse_size(directory: &str) -> Option<u32> {
    let side = directory.split('x').next()?;
    side.parse().ok()
}

/// 把找到的图标文件变成我们这条线上要的 BGRA。
fn render(file: &Path) -> Result<DragImage> {
    let vector = file
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"));
    let image = if vector {
        rasterize(file)?
    } else {
        image::open(file).context("解码图标失败")?.into_rgba8()
    };
    Ok(fit(&image))
}

/// 矢量图标转成位图。自己拖进一整棵 SVG 实现不值当：本机装着的 `rsvg-convert` 就够了，
/// 没有它这条就报错，`icon` 会改用同名的位图。
fn rasterize(file: &Path) -> Result<image::RgbaImage> {
    let mut command = std::process::Command::new("rsvg-convert");
    command.args(["-w", "64", "-h", "64"]).arg(file);
    let stdout = capture(&mut command, "rsvg-convert")?;
    Ok(
        image::load_from_memory_with_format(&stdout, image::ImageFormat::Png)
            .context("解析画出来的图标失败")?
            .into_rgba8(),
    )
}

/// 等比缩到长边 `SIDE`，再转成线路上约定的 BGRA（`image` 给的是 RGBA）。
fn fit(image: &image::RgbaImage) -> DragImage {
    let (width, height) = (image.width(), image.height());
    let scale = f32::from(SIDE as u16) / width.max(height).max(1) as f32;
    let size = |side: u32| ((side as f32 * scale).round().max(1.0) as u32).min(SIDE);
    let (width, height) = (size(width), size(height));
    let resized = if (width, height) == (image.width(), image.height()) {
        image.clone()
    } else {
        image::imageops::resize(image, width, height, image::imageops::FilterType::Triangle)
    };
    let mut pixels = resized.into_raw();
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    DragImage {
        width,
        height,
        pixels,
    }
}

/// 多选：最多三张错开叠在一起，排法跟 Windows 端一样（它还在右下角画个数量角标，
/// 这边没有字体可用，不画）。
fn stacked(icons: &[DragImage]) -> DragImage {
    let mut pixels = vec![0u8; (STACK * STACK * 4) as usize];
    // 从后往前贴：先来的那张在上面，跟 Windows 端的层序一致。
    for (layer, icon) in icons.iter().enumerate().rev() {
        for y in 0..icon.height as usize {
            for x in 0..icon.width as usize {
                let (dx, dy) = (x + layer * 7, y + layer * 5);
                if dx >= STACK as usize || dy >= STACK as usize {
                    continue;
                }
                let from = (y * icon.width as usize + x) * 4;
                let at = (dy * STACK as usize + dx) * 4;
                let source = &icon.pixels[from..from + 4];
                let alpha = u32::from(source[3]);
                let behind = u32::from(pixels[at + 3]);
                let inverse = 255 - alpha;
                // 线路上是**直通** alpha（不是预乘），所以这里有两步：先把上面那层
                // 按自己的 alpha 加权、和下面那层（也按它的 alpha 加权）叠成预乘
                // 结果，再除以合成出来的 alpha 换回直通。少乘那个 alpha，半透明的
                // 边缘会整体发亮。
                let over = alpha + behind * inverse / 255;
                for channel in 0..3 {
                    let mut mixed = u32::from(source[channel]) * alpha
                        + u32::from(pixels[at + channel]) * behind / 255 * inverse;
                    mixed /= over.max(1);
                    pixels[at + channel] = mixed.min(255) as u8;
                }
                pixels[at + 3] = over.min(255) as u8;
            }
        }
    }
    DragImage {
        width: STACK,
        height: STACK,
        pixels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 包装脚本卡住时，真正卡着的是它起的孙进程，而孙进程攥着同一根 stdout：
    /// 超时要连它一起收拾掉，按时返回，而不是陪它等满 10 秒。
    #[test]
    fn tool_timeout_covers_grandchildren() {
        let started = Instant::now();
        let mut command = std::process::Command::new("sh");
        // 后面那个 `:` 让 sh 不能把 `sleep` exec 掉，`sleep` 只能是孙进程。
        command.args(["-c", "sleep 10; :"]);
        assert!(capture(&mut command, "sh").is_err());
        let elapsed = started.elapsed();
        assert!(elapsed < TOOL_LIMIT + Duration::from_secs(1), "{elapsed:?}");
    }
}

#[cfg(test)]
mod probes {
    use super::*;

    /// 真机上想看一眼拖起来会是什么样就跑它，出来的图放在 `/tmp/openuuyc-drag-*.png`：
    /// `cargo test --lib -- --ignored --nocapture writes_previews`。
    #[test]
    #[ignore]
    fn writes_previews() {
        let scratch = std::env::temp_dir().join("openuuyc-drag-preview");
        std::fs::create_dir_all(&scratch).unwrap();
        let sheet = scratch.join("probe.xlsx");
        std::fs::write(&sheet, b"x").unwrap();
        let folder = scratch.join("probe-folder");
        std::fs::create_dir_all(&folder).unwrap();
        let cases = [
            vec![PathBuf::from("assets/icon-256.png")],
            vec![PathBuf::from("Cargo.toml")],
            vec![
                sheet,
                PathBuf::from("Cargo.toml"),
                PathBuf::from("assets/icon-256.png"),
            ],
            vec![folder],
        ];
        for (index, paths) in cases.iter().enumerate() {
            let paths: Vec<PathBuf> = paths
                .iter()
                .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
                .collect();
            match selection(&paths) {
                Ok(image) => {
                    let mut rgba = image.pixels.clone();
                    for pixel in rgba.chunks_exact_mut(4) {
                        pixel.swap(0, 2);
                    }
                    let file = format!("/tmp/openuuyc-drag-{index}.png");
                    image::RgbaImage::from_raw(image.width, image.height, rgba)
                        .unwrap()
                        .save(&file)
                        .unwrap();
                    println!(
                        "{index}: {paths:?} -> {file} {}x{}",
                        image.width, image.height
                    );
                }
                Err(error) => println!("{index}: {paths:?} -> 没有预览图：{error:#}"),
            }
        }
    }
}

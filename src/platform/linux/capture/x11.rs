//! X11 capture: MIT-SHM `GetImage` from the root window, one monitor's
//! rectangle at a time, with the XFixes cursor drawn in on request. Without
//! MIT-SHM (a remote X server) the plain request returns the same pixels,
//! only slower. This is Sunshine's `x11grab` path.
//!
//! X11 has no notification that a frame changed; the desktop compares each
//! grab with the previous one.
use super::Screen;
use anyhow::{Context, Result, bail, ensure};
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, Window};
use x11rb::rust_connection::RustConnection;

/// A shared-memory segment the X server writes captured pixels into.
struct Segment {
    id: u32,
    address: *mut u8,
    size: usize,
}

// The segment is only touched from the capture thread that owns the grab.
unsafe impl Send for Segment {}

impl Segment {
    fn new(connection: &RustConnection, size: usize) -> Result<Self> {
        // SAFETY: plain SysV shared memory calls; every failure is checked and
        // the segment is marked for removal as soon as the server holds it, so
        // it cannot outlive this process.
        let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600) };
        ensure!(shmid >= 0, "分配共享内存失败");
        let address = unsafe { libc::shmat(shmid, std::ptr::null(), 0) };
        if address as isize == -1 {
            unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
            bail!("映射共享内存失败");
        }
        let id = connection.generate_id()?;
        let attached = connection
            .shm_attach(id, shmid as u32, false)
            .map_err(anyhow::Error::from)
            .and_then(|cookie| cookie.check().map_err(anyhow::Error::from));
        unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
        if let Err(error) = attached {
            unsafe { libc::shmdt(address) };
            return Err(error.context("X 服务器无法附加共享内存"));
        }
        Ok(Self {
            id,
            address: address.cast(),
            size,
        })
    }

    fn pixels(&self) -> &[u8] {
        // SAFETY: the mapping stays valid until drop, and it is only read after
        // the server has answered the request that filled it.
        unsafe { std::slice::from_raw_parts(self.address, self.size) }
    }

    fn pixels_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above; the server writes only while a request is pending,
        // and the cursor is drawn after the reply.
        unsafe { std::slice::from_raw_parts_mut(self.address, self.size) }
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        unsafe { libc::shmdt(self.address.cast()) };
    }
}

enum Grabber {
    Shm(Segment),
    Plain,
}

pub(super) struct Grab {
    connection: RustConnection,
    root: Window,
    grabber: Grabber,
    cursor_available: bool,
    /// Without MIT-SHM, the latest picture as the server sent it.
    plain: Vec<u8>,
    /// The byte length of the latest picture.
    length: usize,
}

impl Grab {
    pub fn open(screen: &Screen) -> Result<Self> {
        // Under Wayland the X server is XWayland, whose root window holds only
        // the X clients' pixels: a capture would stream a black desktop.
        ensure!(
            !super::wayland_session(),
            "Wayland 会话中 X11 只能看到 X 客户端的窗口"
        );
        let (connection, screen_index) = x11rb::connect(None).context("连接 X11 显示失败")?;
        let setup = connection.setup();
        let root_screen = &setup.roots[screen_index];
        let root = root_screen.root;
        // Only the common 24/32-bit TrueColor layout is read as BGRA; anything
        // else would need a conversion this capture does not have.
        let format = setup
            .pixmap_formats
            .iter()
            .find(|format| format.depth == root_screen.root_depth)
            .context("X 服务器没有根窗口深度对应的像素格式")?;
        ensure!(
            format.bits_per_pixel == 32
                && matches!(root_screen.root_depth, 24 | 32)
                && setup.image_byte_order == x11rb::protocol::xproto::ImageOrder::LSB_FIRST,
            "只支持 32 位小端 TrueColor 桌面"
        );
        let size = screen.width as usize * screen.height as usize * 4;
        let grabber = match connection
            .extension_information(x11rb::protocol::shm::X11_EXTENSION_NAME)
            .ok()
            .flatten()
        {
            Some(_) => match Segment::new(&connection, size) {
                Ok(segment) => Grabber::Shm(segment),
                Err(error) => {
                    tracing::debug!(error = %format!("{error:#}"), "MIT-SHM 不可用，改用普通 GetImage");
                    Grabber::Plain
                }
            },
            None => Grabber::Plain,
        };
        let cursor_available = connection
            .xfixes_query_version(4, 0)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some();
        Ok(Self {
            connection,
            root,
            grabber,
            cursor_available,
            plain: Vec::new(),
            length: 0,
        })
    }

    pub fn name(&self) -> &'static str {
        match self.grabber {
            Grabber::Shm(_) => "X11 MIT-SHM",
            Grabber::Plain => "X11 GetImage",
        }
    }

    /// Read the monitor's rectangle, cursor included when asked for; the
    /// picture is then [`Grab::pixels`]. It is read where the server left it
    /// (the shared segment) rather than copied out, and the padding byte X
    /// leaves undefined is the reader's to ignore: the scaler writes alpha.
    pub fn grab(&mut self, screen: &Screen, cursor: bool) -> Result<()> {
        let (x, y) = (
            i16::try_from(screen.left).context("显示器坐标超出 X11 范围")?,
            i16::try_from(screen.top).context("显示器坐标超出 X11 范围")?,
        );
        let (width, height) = (
            u16::try_from(screen.width).context("显示器宽度超出 X11 范围")?,
            u16::try_from(screen.height).context("显示器高度超出 X11 范围")?,
        );
        let length = usize::from(width) * usize::from(height) * 4;
        self.length = 0;
        match &self.grabber {
            Grabber::Shm(segment) => {
                ensure!(segment.size >= length, "共享内存小于所选显示器");
                self.connection
                    .shm_get_image(
                        self.root,
                        x,
                        y,
                        width,
                        height,
                        !0,
                        ImageFormat::Z_PIXMAP.into(),
                        segment.id,
                        0,
                    )?
                    .reply()
                    .context("读取屏幕画面失败")?;
            }
            Grabber::Plain => {
                let reply = self
                    .connection
                    .get_image(ImageFormat::Z_PIXMAP, self.root, x, y, width, height, !0)?
                    .reply()
                    .context("读取屏幕画面失败")?;
                ensure!(reply.data.len() >= length, "屏幕画面数据不完整");
                self.plain = reply.data;
            }
        }
        self.length = length;
        if cursor && self.cursor_available {
            // A cursor that cannot be read is simply left out of this frame.
            let _ = self.draw_cursor(screen);
        }
        Ok(())
    }

    /// The latest picture, BGRA at the monitor's full size; the alpha byte is
    /// whatever X left in the padding.
    pub fn pixels(&self) -> &[u8] {
        match &self.grabber {
            Grabber::Shm(segment) => &segment.pixels()[..self.length],
            Grabber::Plain => &self.plain[..self.length],
        }
    }

    fn pixels_mut(&mut self) -> &mut [u8] {
        match &mut self.grabber {
            Grabber::Shm(segment) => &mut segment.pixels_mut()[..self.length],
            Grabber::Plain => &mut self.plain[..self.length],
        }
    }

    fn draw_cursor(&mut self, screen: &Screen) -> Result<()> {
        let image = self.connection.xfixes_get_cursor_image()?.reply()?;
        let (width, height) = (i32::from(image.width), i32::from(image.height));
        ensure!(
            image.cursor_image.len() >= (width * height) as usize,
            "光标图像不完整"
        );
        let origin_x = i32::from(image.x) - i32::from(image.xhot) - screen.left;
        let origin_y = i32::from(image.y) - i32::from(image.yhot) - screen.top;
        let (screen_width, screen_height) = (screen.width as i32, screen.height as i32);
        let pixels = self.pixels_mut();
        for row in 0..height {
            let y = origin_y + row;
            if !(0..screen_height).contains(&y) {
                continue;
            }
            for column in 0..width {
                let x = origin_x + column;
                if !(0..screen_width).contains(&x) {
                    continue;
                }
                // Premultiplied ARGB, one u32 per pixel.
                let argb = image.cursor_image[(row * width + column) as usize];
                let alpha = argb >> 24;
                if alpha == 0 {
                    continue;
                }
                let offset = ((y * screen_width + x) * 4) as usize;
                let pixel = &mut pixels[offset..offset + 4];
                let inverse = 255 - alpha;
                for (channel, shift) in [(0usize, 0u32), (1, 8), (2, 16)] {
                    let over = (argb >> shift) & 0xff;
                    let under = u32::from(pixel[channel]);
                    pixel[channel] = (over + (under * inverse + 127) / 255).min(255) as u8;
                }
            }
        }
        Ok(())
    }
}

//! The remote's copied files, as a filesystem.
//!
//! A local paste hands the pasting application a path and expects it to
//! resolve, which is the one thing a clipboard on X11 or Wayland cannot
//! promise: `text/uri-list` and `x-special/gnome-copied-files` carry URIs, not
//! content, so the file manager opens them itself, long after the clipboard
//! exchange is over. Downloading every copy up front would work and would also
//! pull gigabytes nobody asked for.
//!
//! So the URIs point into a filesystem of our own, and a read of one of those
//! files turns into the `FileContentsRequest` the protocol already has. The
//! two match closely: that request takes a file index, a byte offset and a
//! length, which is what `read` is handed.

use anyhow::{Context as _, Result, bail};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::protocol::ClipboardFileDescriptor;

/// How long the kernel may trust an attribute or lookup. The tree never changes
/// once mounted -- a new copy gets a new directory -- so this only bounds how
/// long a stale generation stays visible after it is retired.
const TTL: Duration = Duration::from_secs(60);
/// Seconds between the Windows epoch (1601-01-01) and the Unix one.
const FILETIME_EPOCH_OFFSET: u64 = 11_644_473_600;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
/// The kernel asks for at most this much in one `read`; the protocol caps a
/// single `FileContentsRequest` lower, so a read may take several of them.
const MAX_READ: u32 = 1 << 20;

/// Reads `length` bytes of the file at `index` in the offer, starting at
/// `offset`. Returning fewer bytes than asked for means end of file.
pub(super) type Reader = Arc<dyn Fn(u32, u64, usize) -> Result<Vec<u8>> + Send + Sync + 'static>;

struct Node {
    name: String,
    parent: u64,
    /// Index into the offer's descriptor list, for a file.
    index: Option<u32>,
    size: u64,
    mtime: SystemTime,
    children: Vec<u64>,
}

impl Node {
    const fn is_dir(&self) -> bool {
        self.index.is_none()
    }
}

/// A Windows FILETIME as the local clock reads it.
fn mtime(filetime: u64) -> SystemTime {
    let seconds = filetime / 10_000_000;
    let nanos = (filetime % 10_000_000) as u32 * 100;
    seconds
        .checked_sub(FILETIME_EPOCH_OFFSET)
        .map_or(UNIX_EPOCH, |unix| UNIX_EPOCH + Duration::new(unix, nanos))
}

/// One remote copy, laid out as a tree. Names arrive flat, each relative to the
/// item it was copied from and separated the way Windows writes them, so the
/// directories between them have to be recovered.
pub(super) struct Tree {
    nodes: HashMap<u64, Node>,
    next: u64,
}

impl Tree {
    const ROOT: u64 = 1;

    fn new() -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(
            Self::ROOT,
            Node {
                name: String::new(),
                parent: Self::ROOT,
                index: None,
                size: 0,
                mtime: UNIX_EPOCH,
                children: Vec::new(),
            },
        );
        Self { nodes, next: 2 }
    }

    pub(super) fn build(descriptors: &[ClipboardFileDescriptor]) -> Result<Self> {
        let mut tree = Self::new();
        for (index, descriptor) in descriptors.iter().enumerate() {
            let index = u32::try_from(index).context("剪贴板文件过多")?;
            tree.insert(descriptor, index)?;
        }
        Ok(tree)
    }

    fn insert(&mut self, descriptor: &ClipboardFileDescriptor, index: u32) -> Result<()> {
        // The peer names files the way Windows does; anything that would climb
        // out of the offer was already refused before this point, but the walk
        // below must not depend on that.
        let parts: Vec<&str> = descriptor
            .file_name
            .split(['\\', '/'])
            .filter(|part| !part.is_empty() && *part != "." && *part != "..")
            .collect();
        let Some((name, directories)) = parts.split_last() else {
            bail!("剪贴板文件名为空");
        };
        let mut parent = Self::ROOT;
        for directory in directories {
            parent = self.directory(parent, directory);
        }
        let is_dir = descriptor.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        if is_dir {
            self.directory(parent, name);
            return Ok(());
        }
        let node = self.attach(
            parent,
            Node {
                name: (*name).to_owned(),
                parent,
                index: Some(index),
                size: descriptor.file_size,
                mtime: mtime(descriptor.last_write_time),
                children: Vec::new(),
            },
        );
        debug_assert!(self.nodes.contains_key(&node));
        Ok(())
    }

    /// The directory called `name` under `parent`, created if the descriptor
    /// list named a file inside it before naming the directory itself.
    fn directory(&mut self, parent: u64, name: &str) -> u64 {
        if let Some(existing) = self.child(parent, name)
            && self.nodes[&existing].is_dir()
        {
            return existing;
        }
        self.attach(
            parent,
            Node {
                name: name.to_owned(),
                parent,
                index: None,
                size: 0,
                mtime: UNIX_EPOCH,
                children: Vec::new(),
            },
        )
    }

    fn attach(&mut self, parent: u64, node: Node) -> u64 {
        let ino = self.next;
        self.next += 1;
        self.nodes.insert(ino, node);
        if let Some(parent) = self.nodes.get_mut(&parent) {
            parent.children.push(ino);
        }
        ino
    }

    fn child(&self, parent: u64, name: &str) -> Option<u64> {
        self.nodes
            .get(&parent)?
            .children
            .iter()
            .copied()
            .find(|ino| self.nodes[ino].name == name)
    }

    /// The names directly under the root, which is what the clipboard points at.
    pub(super) fn roots(&self) -> Vec<String> {
        self.nodes[&Self::ROOT]
            .children
            .iter()
            .map(|ino| self.nodes[ino].name.clone())
            .collect()
    }
}

/// The filesystem itself: a tree that never changes and a way to fetch bytes.
pub(super) struct ClipboardFs {
    tree: Tree,
    reader: Reader,
    uid: u32,
    gid: u32,
}

impl ClipboardFs {
    fn attr(&self, ino: u64) -> Option<fuser::FileAttr> {
        let node = self.tree.nodes.get(&ino)?;
        Some(fuser::FileAttr {
            ino,
            size: node.size,
            blocks: node.size.div_ceil(512),
            atime: node.mtime,
            mtime: node.mtime,
            ctime: node.mtime,
            crtime: node.mtime,
            kind: if node.is_dir() {
                fuser::FileType::Directory
            } else {
                fuser::FileType::RegularFile
            },
            // 这几个位保护不了任何东西：挂载本身是只读的，我们也没实现 `write`，
            // 谁也写不回远端。但文件管理器粘贴时会照抄来源的权限，报 0444 粘出来
            // 的文件就是只读的，用户还得自己 chmod 一遍。
            perm: if node.is_dir() { 0o755 } else { 0o644 },
            nlink: if node.is_dir() { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 512,
            flags: 0,
        })
    }
}

impl fuser::Filesystem for ClipboardFs {
    fn lookup(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &OsStr,
        reply: fuser::ReplyEntry,
    ) {
        let Some(name) = name.to_str() else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.tree.child(parent, name).and_then(|ino| self.attr(ino)) {
            Some(attr) => reply.entry(&TTL, &attr, 0),
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _fh: Option<u64>,
        reply: fuser::ReplyAttr,
    ) {
        match self.attr(ino) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(libc::ENOENT),
        }
    }

    fn readdir(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: fuser::ReplyDirectory,
    ) {
        let Some(node) = self.tree.nodes.get(&ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        if !node.is_dir() {
            reply.error(libc::ENOTDIR);
            return;
        }
        let mut entries = vec![
            (ino, fuser::FileType::Directory, ".".to_owned()),
            (node.parent, fuser::FileType::Directory, "..".to_owned()),
        ];
        for child in &node.children {
            let child_node = &self.tree.nodes[child];
            entries.push((
                *child,
                if child_node.is_dir() {
                    fuser::FileType::Directory
                } else {
                    fuser::FileType::RegularFile
                },
                child_node.name.clone(),
            ));
        }
        for (position, (ino, kind, name)) in
            entries.into_iter().enumerate().skip(offset.max(0) as usize)
        {
            // A full buffer is not an error: the kernel asks again from here.
            if reply.add(ino, position as i64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &fuser::Request<'_>, ino: u64, flags: i32, reply: fuser::ReplyOpen) {
        let Some(node) = self.tree.nodes.get(&ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        if node.is_dir() {
            reply.error(libc::EISDIR);
            return;
        }
        if flags & (libc::O_WRONLY | libc::O_RDWR) != 0 {
            reply.error(libc::EACCES);
            return;
        }
        reply.opened(0, 0);
    }

    fn read(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: fuser::ReplyData,
    ) {
        let Some(node) = self.tree.nodes.get(&ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        let (Some(index), false) = (node.index, node.is_dir()) else {
            reply.error(libc::EISDIR);
            return;
        };
        let offset = offset.max(0) as u64;
        if offset >= node.size {
            reply.data(&[]);
            return;
        }
        let wanted = u64::from(size.min(MAX_READ)).min(node.size - offset) as usize;
        // One `read` can outrun what a single request carries, so it is filled
        // from as many as it takes; a short answer means the file ended early.
        let mut data = Vec::with_capacity(wanted);
        while data.len() < wanted {
            let offset = offset + data.len() as u64;
            match (self.reader)(index, offset, wanted - data.len()) {
                Ok(chunk) if chunk.is_empty() => break,
                Ok(chunk) => data.extend_from_slice(&chunk),
                Err(error) => {
                    tracing::debug!(%error, index, offset, "读取远端剪贴板文件失败");
                    reply.error(libc::EIO);
                    return;
                }
            }
        }
        reply.data(&data);
    }
}

/// A mounted offer. Unmounts when dropped, which is what retires the paths the
/// clipboard still points at.
pub(super) struct Mount {
    /// Kept for its `Drop`, which unmounts.
    #[allow(dead_code, reason = "Owned for its Drop.")]
    session: fuser::BackgroundSession,
    root: PathBuf,
    names: Vec<String>,
}

impl Mount {
    /// Mount `tree` in its own directory under `parent`.
    pub(super) fn new(parent: &Path, generation: u64, tree: Tree, reader: Reader) -> Result<Self> {
        let names = tree.roots();
        if names.is_empty() {
            bail!("剪贴板文件列表为空");
        }
        let root = parent.join(generation.to_string());
        std::fs::create_dir_all(&root)
            .with_context(|| format!("创建挂载点 {} 失败", root.display()))?;
        let filesystem = ClipboardFs {
            tree,
            reader,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        };
        let session = fuser::spawn_mount2(
            filesystem,
            &root,
            &[
                fuser::MountOption::FSName("openuuyc-clipboard".to_owned()),
                fuser::MountOption::Subtype("openuuyc".to_owned()),
                fuser::MountOption::RO,
                fuser::MountOption::NoExec,
                fuser::MountOption::NoSuid,
                fuser::MountOption::NoDev,
                fuser::MountOption::NoAtime,
                // Not `AutoUnmount`: it needs `AllowOther` or `AllowRoot`, and
                // either would hand the remote's files to every local user. A
                // mount left behind by a crash is cleared on the next run
                // instead.
            ],
        );
        let session = match session {
            Ok(session) => session,
            Err(error) => {
                let _ = std::fs::remove_dir(&root);
                return Err(anyhow::Error::new(error))
                    .with_context(|| format!("挂载 {} 失败", root.display()));
            }
        };
        Ok(Self {
            session,
            root,
            names,
        })
    }

    /// The paths the local clipboard should point at.
    pub(super) fn paths(&self) -> Vec<PathBuf> {
        self.names.iter().map(|name| self.root.join(name)).collect()
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        // The session unmounts as it drops; the directory it stood on is ours.
        let root = self.root.clone();
        std::thread::spawn(move || {
            // Give the unmount a moment to complete before the directory goes.
            std::thread::sleep(Duration::from_millis(200));
            let _ = std::fs::remove_dir(&root);
        });
    }
}

/// A fresh name for a mount's directory, so a new offer never reuses a path a
/// file manager may still have open.
pub(super) fn generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The directory mounts go under, without creating it.
///
/// Resolved once, through any symlink in `XDG_RUNTIME_DIR`: the paths a remote
/// copy publishes come back from the clipboard canonicalized (arboard resolves
/// them when it writes `text/uri-list`), and `own_copy` compares them against
/// this. With the raw path, a symlinked runtime directory would make our own
/// mount look like a local copy and send it back to the remote.
pub(super) fn mount_root() -> Option<PathBuf> {
    static ROOT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .map(|base| root_under(&base))
    })
    .clone()
}

/// The mount root under `base`, with `base` resolved through symlinks. The
/// directories below it are ours and may not exist yet, so only `base` is
/// resolved; if that fails the raw path is the best answer there is.
pub(super) fn root_under(base: &Path) -> PathBuf {
    std::fs::canonicalize(base)
        .unwrap_or_else(|_| base.to_path_buf())
        .join("openuuyc")
        .join("clipboard")
}

/// Where this process mounts clipboard offers. Under the runtime directory, so
/// it is per-user, on tmpfs, and removed when the session ends.
pub(super) fn mount_parent() -> Result<PathBuf> {
    let parent = mount_root().context("XDG_RUNTIME_DIR 未设置，无法挂载剪贴板文件")?;
    std::fs::create_dir_all(&parent).with_context(|| format!("创建 {} 失败", parent.display()))?;
    static CLEANED: std::sync::Once = std::sync::Once::new();
    CLEANED.call_once(|| retire_stale(&parent));
    Ok(parent)
}

/// Clear out generations from an earlier run of this process. Nothing unmounts
/// them when it dies, so they are still mounted, and the directory each stands
/// on cannot be removed until it is not.
fn retire_stale(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Not `is_dir`: asking the kernel about a mount whose server died
        // fails with `ENOTCONN`, so the entries most in need of clearing are
        // exactly the ones that would answer no. Unmounting something that was
        // never mounted is harmless.
        //
        // Lazily, because a file manager may still hold the old mount open;
        // the kernel detaches it once the last user lets go.
        let _ = std::process::Command::new("fusermount3")
            .args(["-u", "-q", "-z"])
            .arg(&path)
            .status();
        if std::fs::remove_dir(&path).is_err() {
            tracing::debug!(path = %path.display(), "残留的剪贴板挂载点暂时无法移除");
        }
    }
}

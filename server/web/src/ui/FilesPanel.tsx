// The viewer's file panel (`docs/web-console.md` §7.7): browse the host's
// disks and folders, upload files and folders into the shown folder (or the
// host user's Downloads), download files and folders, follow transfers.
import { useCallback, useEffect, useRef, useState } from 'react';
import {
  childPath,
  DEFAULT_DIR,
  EntryKind,
  fileTarget,
  folderSelection,
  saveFolderTarget,
  saveTarget,
  type FileEntry,
  type FileTransfer,
  type Listing,
  type TransferInfo,
} from '../lib/files';
import { bytes } from '../lib/format';
import { Icon, type IconName } from './icons';
import { Button, Spinner } from './kit';

const FOLDER_ICONS: Record<string, string> = {
  document: '文档',
  desktop: '桌面',
  picture: '图片',
  video: '视频',
  music: '音乐',
  download: '下载',
};

function iconOf(e: FileEntry): IconName {
  if (e.kind === EntryKind.DISK) return 'disk';
  if (e.kind === EntryKind.DIR) return 'folder';
  if (e.kind === EntryKind.LINK) return 'link';
  return 'file';
}

/// The folder above `path`, or the root list ('') above a disk.
function parentOf(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, '');
  const at = Math.max(trimmed.lastIndexOf('\\'), trimmed.lastIndexOf('/'));
  if (at < 0 || /^[A-Za-z]:$/.test(trimmed)) return '';
  const parent = trimmed.slice(0, at);
  return /^[A-Za-z]:$/.test(parent) ? `${parent}\\` : parent;
}

export function FilesPanel({
  files,
  transfers,
  toast,
}: {
  files: FileTransfer | undefined;
  transfers: TransferInfo[];
  toast(text: string, tone?: 'green' | 'red' | 'blue'): void;
}) {
  const [listing, setListing] = useState<Listing>();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string>();
  const [showHidden, setShowHidden] = useState(false);
  const picker = useRef<HTMLInputElement>(null);
  const folderPicker = useRef<HTMLInputElement>(null);

  const load = useCallback(
    async (path: string) => {
      if (!files) return;
      setLoading(true);
      setError(undefined);
      try {
        setListing(await files.list(path));
      } catch (e) {
        setError((e as Error).message);
      } finally {
        setLoading(false);
      }
    },
    [files],
  );

  // The root list (disks and known folders) when the panel opens.
  useEffect(() => {
    void Promise.resolve().then(() => load(''));
  }, [load]);

  const atRoot = !listing || listing.path === '';
  const target = atRoot ? DEFAULT_DIR : listing.path;

  async function upload(list: FileList | null, folders: boolean) {
    if (!files || !list?.length) return;
    const chosen = [...list];
    try {
      if (folders) {
        for (const group of folderSelection(chosen)) await files.upload(group.entries, target, group.folder);
      } else {
        await files.uploadFiles(chosen, target);
      }
      toast(atRoot ? '已上传到对方的“下载”文件夹' : '上传完成');
      if (!atRoot) void load(listing.path);
    } catch (e) {
      toast((e as Error).message, 'red');
    }
  }

  async function download(entry: FileEntry) {
    if (!files || !listing) return;
    const folder = entry.kind === EntryKind.DIR;
    try {
      // The pickers need this click: ask before any other await.
      const target = folder
        ? await saveFolderTarget(entry.name, 0)
        : await saveTarget(entry.name, Number(entry.size)).then((sink) => sink && fileTarget(sink));
      if (!target) return;
      await files.download(childPath(listing.path, entry), entry.name, target);
      toast(`已下载 ${entry.name}`);
    } catch (e) {
      toast((e as Error).message, 'red');
    }
  }

  const entries = (listing?.entries ?? []).filter((e) => showHidden || !e.hidden);

  return (
    <div className="files">
      <div className="files-bar row gap-sm">
        <button
          type="button"
          className="tool"
          title="上一级"
          disabled={atRoot || loading}
          onClick={() => listing && void load(parentOf(listing.path))}
        >
          <Icon name="up" size={16} />
        </button>
        <div className="grow ellipsis small mono" title={listing?.path}>
          {atRoot ? '此电脑' : listing.path}
        </div>
        <button type="button" className="tool" title="刷新" onClick={() => void load(listing?.path ?? '')}>
          <Icon name="refresh" size={16} />
        </button>
      </div>
      <div className="row gap-sm files-actions">
        <Button icon="upload" onClick={() => picker.current?.click()}>
          {atRoot ? '上传到“下载”' : '上传文件'}
        </Button>
        <Button icon="folder" onClick={() => folderPicker.current?.click()}>
          上传文件夹
        </Button>
        <label className="small muted row gap-sm">
          <input type="checkbox" checked={showHidden} onChange={(e) => setShowHidden(e.target.checked)} />
          隐藏文件
        </label>
        <input
          ref={picker}
          type="file"
          multiple
          hidden
          onChange={(e) => {
            void upload(e.target.files, false);
            e.target.value = '';
          }}
        />
        <input
          ref={folderPicker}
          type="file"
          hidden
          {...{ webkitdirectory: '' }}
          onChange={(e) => {
            void upload(e.target.files, true);
            e.target.value = '';
          }}
        />
      </div>
      {error && <div className="files-error small red">{error}</div>}
      <div className="files-list">
        {loading && !listing ? (
          <div className="row gap-sm muted small" style={{ padding: 14 }}>
            <Spinner size={14} /> 正在读取
          </div>
        ) : (
          entries.map((e) => {
            const opens = e.kind === EntryKind.DIR || e.kind === EntryKind.DISK;
            const label = atRoot && e.kind === EntryKind.DIR ? (FOLDER_ICONS[e.icon] ?? e.name) : e.name;
            return (
              <div key={`${e.kind}:${e.name}:${e.path}`} className={`files-row ${e.hidden ? 'faint' : ''}`}>
                <button
                  type="button"
                  className="files-entry"
                  disabled={e.kind === EntryKind.LINK}
                  title={opens ? '打开' : e.kind === EntryKind.FILE ? '下载到本机' : '快捷方式不能打开'}
                  onClick={() => (opens ? void load(childPath(listing!.path, e)) : void download(e))}
                >
                  <Icon name={iconOf(e)} size={16} />
                  <span className="grow ellipsis">{label}</span>
                  {e.kind === EntryKind.FILE && <span className="small muted">{bytes(Number(e.size))}</span>}
                </button>
                {(e.kind === EntryKind.FILE || e.kind === EntryKind.DIR) && (
                  <button
                    type="button"
                    className="tool"
                    title={e.kind === EntryKind.DIR ? '下载整个文件夹' : '下载到本机'}
                    onClick={() => void download(e)}
                  >
                    <Icon name="download" size={14} />
                  </button>
                )}
              </div>
            );
          })
        )}
        {listing && entries.length === 0 && !loading && <div className="muted small files-empty">空文件夹</div>}
      </div>
      {transfers.length > 0 && <TransferList transfers={transfers} files={files} />}
    </div>
  );
}

export function TransferList({ transfers, files }: { transfers: TransferInfo[]; files: FileTransfer | undefined }) {
  return (
    <div className="transfers">
      {transfers.map((t) => (
        <div key={`${t.direction}${t.id}`} className="transfer">
          <div className="row gap-sm">
            <Icon name={t.direction === 'up' ? 'upload' : 'download'} size={14} />
            <span className="grow ellipsis small">{t.name}</span>
            <span className="small muted">
              {t.state === 'running' ? `${bytes(t.done)} / ${bytes(t.total)}` : t.state === 'done' ? '完成' : t.error}
            </span>
            {t.state === 'running' && (
              <button type="button" className="tool" title="取消" onClick={() => void files?.cancel(t.id)}>
                <Icon name="close" size={14} />
              </button>
            )}
          </div>
          <div className="progress">
            <div style={{ width: `${t.total ? Math.min(100, (t.done / t.total) * 100) : 0}%` }} />
          </div>
        </div>
      ))}
    </div>
  );
}

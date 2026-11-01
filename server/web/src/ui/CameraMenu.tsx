// A camera's menu in the viewer (`docs/protocol/camera-design.md` §6.2):
// picture in picture, the parameters (the owner's or this browser's own) and
// the picture adjustments. Changes apply at once.
import type { CSSProperties } from 'react';
import { useT } from '../i18n/react';
import type { CameraInfo, CameraMode, CameraParams } from '../lib/fpp';
import type { Viewer } from '../lib/viewer';
import { Menu, MenuItem } from './ViewerMenu';

const FORMATS = ['NV12', 'YUY2', 'MJPEG', 'H.264'];
const BITRATES = [500, 1000, 1500, 2000, 3000, 4000, 5000, 8000, 12000, 20000];

function fps(mode: CameraMode): number {
  return mode.fpsDenominator ? Math.round(mode.fpsNumerator / mode.fpsDenominator) : 0;
}

function sameMode(a?: CameraMode, b?: CameraMode): boolean {
  return (
    !!a &&
    !!b &&
    a.width === b.width &&
    a.height === b.height &&
    a.fpsNumerator === b.fpsNumerator &&
    a.fpsDenominator === b.fpsDenominator &&
    a.format === b.format
  );
}

function kbpsText(kbps: number): string {
  return `${kbps % 1000 ? (kbps / 1000).toFixed(1) : kbps / 1000} Mbps`;
}

/// The bitrate ceiling by resolution and rate (`camera.rs`).
function defaultKbps(mode: CameraMode): number {
  const lines = Math.min(mode.width, mode.height);
  const high = fps(mode) > 30;
  if (lines <= 480) return high ? 2500 : 1500;
  if (lines <= 720) return high ? 4500 : 3000;
  if (lines <= 1080) return high ? 8000 : 5000;
  return high ? 12000 : 8000;
}

/// How a camera's picture is turned: mirrored, then rotated clockwise. A
/// quarter turn swaps the element's sides within its container (which is a
/// size container: `cqw`, `cqh`).
export function orientationStyle(mirror: boolean, quarterTurns: number): CSSProperties | undefined {
  if (!mirror && !quarterTurns) return undefined;
  const turn = `rotate(${quarterTurns * 90}deg)${mirror ? ' scaleX(-1)' : ''}`;
  if (quarterTurns % 2 === 0) return { transform: turn };
  return {
    position: 'absolute',
    left: '50%',
    top: '50%',
    width: '100cqh',
    height: '100cqw',
    transform: `translate(-50%, -50%) ${turn}`,
  };
}

export function CameraMenu({
  viewer,
  cameras,
  shown,
  pip,
  onPip,
}: {
  viewer: Viewer | undefined;
  cameras: CameraInfo[];
  /// The camera on the main picture, if it shows one.
  shown?: number;
  /// The picture-in-picture camera.
  pip?: number;
  onPip(camera: number | undefined): void;
}) {
  const t = useT();
  const m = t.viewer.camera;
  const target = cameras.find((c) => c.id === (shown ?? pip));
  const current = target ? (target.running ?? target.custom ?? target.owner) : undefined;
  const base: CameraParams = {
    matrix: 0,
    range: 0,
    bitrateKbps: 0,
    mirror: false,
    quarterTurns: 0,
    ...(target?.custom ?? target?.owner),
  };
  if (!base.mode) base.mode = current?.mode;
  const apply = (change: Partial<CameraParams>) => {
    if (target) void viewer?.setCameraParams(target.id, { ...base, ...change });
  };
  const status = target?.unavailable || (target?.chosenBy ? m.chosenBy(target.chosenBy) : '');
  return (
    <Menu label={m.title} icon="webcam" active={pip !== undefined}>
      {shown === undefined && (
        <>
          <div className="menu-label">{m.pictureInPicture}</div>
          {cameras.map((c) => (
            <MenuItem
              key={c.id}
              selected={pip === c.id}
              disabled={!!c.unavailable}
              title={c.unavailable || undefined}
              onClick={() => onPip(pip === c.id ? undefined : c.id)}
            >
              {c.name}
            </MenuItem>
          ))}
        </>
      )}
      {target && current && (
        <>
          <div className="menu-label">{target.name}</div>
          {status && <div className="menu-note small muted">{status}</div>}
          <div className="menu-label">{m.mode}</div>
          {target.modes.map((mode) => (
            <MenuItem
              key={`${mode.width}x${mode.height}@${mode.fpsNumerator}/${mode.fpsDenominator}:${mode.format}`}
              selected={sameMode(mode, current.mode)}
              hint={FORMATS[mode.format]}
              onClick={() => apply({ mode })}
            >
              {m.modeValue(mode.width, mode.height, fps(mode))}
            </MenuItem>
          ))}
          <div className="menu-label">{m.color}</div>
          {[
            [0, 0],
            [1, 1],
            [1, 2],
            [2, 1],
            [2, 2],
          ].map(([matrix, range]) => (
            <MenuItem
              key={`${matrix}${range}`}
              selected={current.matrix === matrix && current.range === range}
              onClick={() => apply({ matrix, range })}
            >
              {matrix === 0
                ? m.automatic
                : `${matrix === 1 ? 'BT.601' : 'BT.709'} · ${range === 2 ? m.full : m.limited}`}
            </MenuItem>
          ))}
          <div className="menu-label">{m.orientation}</div>
          <MenuItem selected={current.mirror} onClick={() => apply({ mirror: !current.mirror })}>
            {m.mirror}
          </MenuItem>
          {[0, 1, 2, 3].map((turns) => (
            <MenuItem
              key={turns}
              selected={current.quarterTurns === turns}
              onClick={() => apply({ quarterTurns: turns })}
            >
              {`${turns * 90}°`}
            </MenuItem>
          ))}
          <div className="menu-label">{m.bitrate}</div>
          <MenuItem
            selected={!current.bitrateKbps || !(target.custom ?? target.owner)?.bitrateKbps}
            hint={current.mode ? kbpsText(defaultKbps(current.mode)) : undefined}
            onClick={() => apply({ bitrateKbps: 0 })}
          >
            {m.automatic}
          </MenuItem>
          {BITRATES.map((kbps) => (
            <MenuItem
              key={kbps}
              selected={(target.custom ?? target.owner)?.bitrateKbps === kbps}
              onClick={() => apply({ bitrateKbps: kbps })}
            >
              {kbpsText(kbps)}
            </MenuItem>
          ))}
          {target.controls.length > 0 && <div className="menu-label">{m.adjustments}</div>}
          {target.controls.map((control) => (
            <div key={control.control} className="menu-slider" onClick={(e) => e.stopPropagation()}>
              <div className="row gap-sm">
                <span className="grow">{m.controls[control.control] ?? ''}</span>
                {control.canAuto && (
                  <button
                    type="button"
                    className={`menu-chip ${control.auto ? 'selected' : ''}`}
                    onClick={() => void viewer?.adjustCamera(target.id, control.control, control.value, !control.auto)}
                  >
                    {m.automatic}
                  </button>
                )}
                <span className="small faint">{control.value}</span>
              </div>
              <input
                type="range"
                min={control.minimum}
                max={control.maximum}
                step={control.step}
                defaultValue={control.value}
                disabled={control.auto}
                aria-label={m.controls[control.control] ?? ''}
                onPointerUp={(e) =>
                  void viewer?.adjustCamera(target.id, control.control, Number(e.currentTarget.value), false)
                }
                onKeyUp={(e) =>
                  void viewer?.adjustCamera(target.id, control.control, Number(e.currentTarget.value), false)
                }
              />
            </div>
          ))}
          <MenuItem disabled={!target.custom} onClick={() => void viewer?.setCameraParams(target.id, undefined)}>
            {m.ownerSettings}
          </MenuItem>
        </>
      )}
    </Menu>
  );
}

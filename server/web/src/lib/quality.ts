// The bitrate ceiling of each quality tier, as the host computes it
// (`src/features/host/parameters.rs` `tier_rate`): the menu shows the figure
// for the viewed screen at the chosen frame rate.

/// Output size limit of tiers 1–4 (`src/media/geometry.rs` `dimensions`).
const BOXES: Record<number, [number, number]> = {
  1: [1280, 720],
  2: [1920, 1080],
  3: [2560, 1440],
  4: [3840, 2160],
};

/// Mbps at 60 FPS for 720p, 1080p, 1440p and 4K.
const RATES: Record<number, [number, number, number, number]> = {
  1: [1.5, 1.5, 1.5, 1.5],
  2: [5.5, 8, 8, 8],
  3: [8, 14, 24, 24],
  4: [12, 20, 36, 60],
};

const ANCHORS = [1280 * 720, 1920 * 1080, 2560 * 1440, 3840 * 2160];

/// Fit a source into a tier's box in either orientation (`fit_size`).
function fit(width: number, height: number, box: [number, number]): [number, number] {
  const [bw, bh] = width < height !== box[0] < box[1] ? [box[1], box[0]] : box;
  if (width <= bw && height <= bh) return [Math.max(width & ~1, 2), Math.max(height & ~1, 2)];
  const scale = Math.min(bw / width, bh / height, 1);
  return [Math.max(Math.round(width * scale * 0.5) * 2, 2), Math.max(Math.round(height * scale * 0.5) * 2, 2)];
}

/// The ceiling (bps) of tier `quality` (1–4) for a `width`×`height` screen
/// at `fps`; 4:4:4 (`trueColor`) doubles it.
export function tierRate(quality: number, width: number, height: number, fps: number, trueColor = false): number {
  const rates = RATES[quality] ?? RATES[2];
  const [w, h] = fit(width, height, BOXES[quality] ?? BOXES[2]);
  const pixels = w * h;
  let base: number;
  if (pixels <= ANCHORS[0]) base = rates[0] * Math.max(pixels / ANCHORS[0], 0.25);
  else {
    const i = ANCHORS.findIndex((anchor) => pixels <= anchor);
    base =
      i < 0
        ? rates[3]
        : rates[i - 1] + ((rates[i] - rates[i - 1]) * (pixels - ANCHORS[i - 1])) / (ANCHORS[i] - ANCHORS[i - 1]);
  }
  const ratio = Math.min(Math.max(fps, 1), 144) / 60;
  const rate = base * 1e6 * (trueColor ? 2 : 1) * (fps <= 60 ? ratio : Math.sqrt(ratio));
  return Math.min(Math.max(Math.round(rate), 500_000), 500_000_000);
}

/// A bitrate in the quality menu: `1.5M`, `8M`, `22M`.
export function mbps(bps: number): string {
  const value = bps / 1e6;
  if (value < 9.95) return `${Math.round(value * 10) / 10}M`;
  return `${Math.round(value)}M`;
}

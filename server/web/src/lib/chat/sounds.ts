// Voice sounds (`docs/protocol/chat-design.md` §7.5): the native client's
// short tones (`src/features/voice/sounds.rs`), synthesized in the page.

export type Sound = 'ring' | 'ringback' | 'joined' | 'left' | 'came' | 'went' | 'muted' | 'unmuted';

/// (frequency Hz, milliseconds); 0 Hz is a pause.
const NOTES: Record<Sound, [number, number][]> = {
  ring: [
    [784, 180],
    [0, 60],
    [659, 180],
    [0, 60],
    [784, 180],
    [0, 60],
    [659, 180],
  ],
  ringback: [[440, 900]],
  joined: [
    [523, 90],
    [784, 140],
  ],
  left: [
    [784, 90],
    [523, 140],
  ],
  came: [[880, 90]],
  went: [[587, 90]],
  muted: [[392, 60]],
  unmuted: [[659, 60]],
};

let context: AudioContext | undefined;

/// Plays `sound` at `volume` (0–2) on output `output` (the default if unset).
export function play(sound: Sound, volume: number, output?: string) {
  if (volume <= 0) return;
  context ??= new AudioContext();
  const ctx = context as AudioContext & { setSinkId?(id: string): Promise<void>; sinkId?: string };
  if (ctx.setSinkId && (ctx.sinkId ?? '') !== (output ?? '')) void ctx.setSinkId(output ?? '').catch(() => undefined);
  void ctx.resume().catch(() => undefined);
  let at = ctx.currentTime + 0.01;
  for (const [hz, ms] of NOTES[sound]) {
    const d = ms / 1000;
    if (hz > 0) {
      const osc = ctx.createOscillator();
      const gain = ctx.createGain();
      const level = 0.2 * volume;
      osc.frequency.value = hz;
      gain.gain.setValueAtTime(0, at);
      gain.gain.linearRampToValueAtTime(level, at + 0.005);
      gain.gain.setValueAtTime(level, at + d - 0.005);
      gain.gain.linearRampToValueAtTime(0, at + d);
      osc.connect(gain).connect(ctx.destination);
      osc.start(at);
      osc.stop(at + d);
    }
    at += d;
  }
}

import { describe, expect, it } from 'vitest';
import { mbps, tierRate } from '../lib/quality';

describe('tier ceilings', () => {
  it('match the host table', () => {
    // Same figures as parameters.rs `tier_rates_follow_size_frame_rate_and_colour`.
    expect(tierRate(3, 2560, 1600, 60)).toBe(21_714_286);
    expect(tierRate(3, 2560, 1600, 30)).toBe(10_857_143);
    expect(tierRate(2, 1920, 1080, 144)).toBe(12_393_547);
    expect(tierRate(3, 1920, 1080, 60)).toBe(14_000_000);
  });
  it('read as menu figures', () => {
    expect(mbps(1_500_000)).toBe('1.5M');
    expect(mbps(8_000_000)).toBe('8M');
    expect(mbps(21_714_286)).toBe('22M');
    expect(mbps(750_000)).toBe('0.8M');
  });
});

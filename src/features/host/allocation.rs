//! Streams share one connection estimate. Reservations are preferences, never
//! permission to exceed the available rate; excess is shared up to each ceiling.
use super::parameters::Bounds;
use std::collections::BTreeMap;

#[derive(Clone, Copy)]
pub(super) enum View {
    Media,
    Probe,
}

pub(super) fn total(streams: &BTreeMap<usize, Bounds>) -> Bounds {
    if streams.len() == 1 {
        return *streams.values().next().unwrap();
    }
    let mut sum = Bounds {
        reservation: 0,
        maximum: 0,
        initial: 0,
        probe: 0,
    };
    for b in streams.values() {
        sum.reservation = sum.reservation.saturating_add(b.reservation);
        sum.maximum = sum.maximum.saturating_add(b.maximum);
        sum.initial = sum.initial.saturating_add(b.initial);
        sum.probe = sum.probe.saturating_add(b.network_maximum());
    }
    // One link startup allowance, even when many screens join at once.
    sum.initial = sum.initial.min(super::parameters::STARTUP_CEILING);
    sum
}

pub(super) fn share(
    streams: &BTreeMap<usize, Bounds>,
    index: usize,
    target: u32,
    view: View,
) -> u32 {
    if target == 0 || !streams.contains_key(&index) {
        return 0;
    }
    let mut rates: Vec<_> = streams
        .iter()
        .map(|(&id, b)| {
            let maximum = if matches!(view, View::Probe) {
                b.network_maximum()
            } else {
                b.maximum
            };
            (id, b.reservation.min(maximum), maximum)
        })
        .collect();
    let minimum: u64 = rates.iter().map(|(_, min, _)| u64::from(*min)).sum();
    if minimum > u64::from(target) {
        let mut remaining = target;
        for (_, rate, _) in &mut rates {
            *rate = (u64::from(*rate) * u64::from(target) / minimum) as u32;
            remaining -= *rate;
        }
        // Distribute rounding remainders deterministically across callers.
        for (_, rate, maximum) in &mut rates {
            if remaining == 0 {
                break;
            }
            if *rate < *maximum {
                *rate += 1;
                remaining -= 1;
            }
        }
        return rates
            .into_iter()
            .find(|(id, _, _)| *id == index)
            .map_or(0, |(_, rate, _)| rate);
    }
    let mut remaining = u64::from(target).saturating_sub(minimum);
    // Saturated small streams return their unused share to the remaining ones.
    while remaining > 0 {
        let count = rates.iter().filter(|(_, rate, max)| rate < max).count() as u64;
        if count == 0 {
            break;
        }
        let step = remaining / count;
        if step == 0 {
            for (_, rate, maximum) in &mut rates {
                if remaining == 0 {
                    break;
                }
                if *rate < *maximum {
                    *rate += 1;
                    remaining -= 1;
                }
            }
            break;
        }
        for (_, rate, max) in &mut rates {
            let added = step.min(u64::from(max.saturating_sub(*rate)));
            *rate += added as u32;
            remaining -= added;
        }
    }
    rates
        .into_iter()
        .find(|(id, _, _)| *id == index)
        .map_or(0, |(_, rate, _)| rate)
}

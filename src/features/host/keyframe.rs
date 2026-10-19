//! Authenticated RTCP requests: target filtering, FIR deduplication, then the
//! sender's request interval of max(100 ms, 1.5 x RTT): long enough for an
//! IDR to arrive before a repeat counts, short enough that a loss after it
//! is repaired promptly. Local frame controls bypass this network gate.
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use webrtc::rtcp::{
    goodbye::Goodbye,
    packet::Packet,
    payload_feedbacks::{
        full_intra_request::FullIntraRequest, picture_loss_indication::PictureLossIndication,
    },
};

#[derive(Default)]
pub(super) struct Feedback {
    primary: Option<u32>,
    fir: HashMap<u32, (u8, Instant)>,
    requested: Option<Instant>,
}
impl Feedback {
    pub fn accept(&mut self, packet: &dyn Packet, primary: u32, now: Instant, rtt: Duration) -> bool {
        if self.primary != Some(primary) {
            self.primary = Some(primary);
            self.fir.clear();
            self.requested = None;
        }
        if let Some(bye) = packet.as_any().downcast_ref::<Goodbye>() {
            // Remove the departing feedback sender's FIR sequence.
            for sender in &bye.sources {
                self.fir.remove(sender);
            }
            return false;
        }
        let valid = if let Some(pli) = packet.as_any().downcast_ref::<PictureLossIndication>() {
            pli.media_ssrc == primary
        } else if let Some(fir) = packet.as_any().downcast_ref::<FullIntraRequest>() {
            let mut valid = false;
            // Update FIR history before the request gate, even
            // when that later gate rejects this request as too early.
            for entry in &fir.fir {
                if entry.ssrc != primary {
                    continue;
                }
                if self
                    .fir
                    .get(&fir.sender_ssrc)
                    .is_some_and(|(sequence, at)| {
                        *sequence == entry.sequence_number
                            || now.saturating_duration_since(*at) < Duration::from_millis(17)
                    })
                {
                    continue;
                }
                self.fir
                    .insert(fir.sender_ssrc, (entry.sequence_number, now));
                valid = true;
            }
            valid
        } else {
            false
        };
        let spacing = rtt.mul_f64(1.5).max(Duration::from_millis(100));
        if !valid
            || self
                .requested
                .is_some_and(|at| now.saturating_duration_since(at) < spacing)
        {
            return false;
        }
        self.requested = Some(now);
        true
    }
}

//! RFC 7798 single NAL and FU packets, without DONL (not negotiated).
use anyhow::{Result, ensure};
use bytes::Bytes;
pub(crate) fn payloads(mtu: usize, data: &[u8]) -> Result<Vec<Bytes>> {
    ensure!(mtu > 3, "HEVC RTP负载预算不足");
    let mut packets = Vec::new();
    for nal in crate::media::video_format::annex_b_units(data) {
        ensure!(
            nal.len() >= 2 && nal[0] & 0x80 == 0 && nal[1] & 7 != 0,
            "HEVC NAL头无效"
        );
        let kind = (nal[0] >> 1) & 63;
        ensure!(kind < 48, "输入HEVC码流包含RTP封包类型");
        if nal.len() <= mtu {
            packets.push(Bytes::copy_from_slice(nal));
            continue;
        }
        let count = (nal.len() - 2).div_ceil(mtu - 3);
        for (index, fragment) in nal[2..].chunks(mtu - 3).enumerate() {
            let mut packet = Vec::with_capacity(fragment.len() + 3);
            packet.extend_from_slice(&[
                (nal[0] & 0x81) | (49 << 1),
                nal[1],
                kind | if index == 0 { 0x80 } else { 0 }
                    | if index + 1 == count { 0x40 } else { 0 },
            ]);
            packet.extend_from_slice(fragment);
            packets.push(Bytes::from(packet));
        }
    }
    ensure!(!packets.is_empty(), "HEVC码流缺少Annex B NAL");
    Ok(packets)
}

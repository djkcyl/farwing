//! Video MediaChannel socket policy.
//! This is kernel datagram capacity, not playout delay.
use socket2::SockRef;

const VIDEO_SOCKET_BUFFER_BYTES: usize = 1_048_576;

pub(crate) fn configure_media_buffers(socket: &SockRef<'_>) {
    // SetOption logs failure but does not fail the media connection.
    // OS limits/accounting can differ (Linux may report double the request).
    if let Err(error) = socket.set_recv_buffer_size(VIDEO_SOCKET_BUFFER_BYTES) {
        log::warn!("media socket receive-buffer configuration failed: {error}");
    }
    if let Err(error) = socket.set_send_buffer_size(VIDEO_SOCKET_BUFFER_BYTES) {
        log::warn!("media socket send-buffer configuration failed: {error}");
    }
    log::debug!(
        "media socket buffers: requested={} receive={:?} send={:?}",
        VIDEO_SOCKET_BUFFER_BYTES,
        socket.recv_buffer_size(),
        socket.send_buffer_size()
    );
}

pub(crate) fn configure_udp_media_buffers(socket: &(dyn util::Conn + Send + Sync)) {
    if let Some(udp) = socket.as_any().downcast_ref::<tokio::net::UdpSocket>() {
        configure_media_buffers(&SockRef::from(udp));
    }
    // In-memory VNet transports have no kernel socket to configure.
}

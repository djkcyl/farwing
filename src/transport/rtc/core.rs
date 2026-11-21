//! Shared connection ownership and transport contracts for both UU roles.
//! Media registration and business authorization belong to the role adapters.
use super::negotiation::{MAX_DATA_CHANNEL_MESSAGE_SIZE, apply_uu_application_attributes_for_role};
use crate::transport::uu_kcp::{ControlReceiver, UuKcpControl};
use anyhow::{Context, Result};
use std::sync::Arc;
use webrtc::{
    api::{
        APIBuilder,
        media_engine::MediaEngine,
        setting_engine::{SctpMaxMessageSize, SettingEngine},
    },
    ice_transport::ice_server::RTCIceServer,
    interceptor::registry::Registry,
    peer_connection::{
        RTCPeerConnection, configuration::RTCConfiguration, offer_answer_options::RTCOfferOptions,
        peer_connection_state::RTCPeerConnectionState,
        policy::ice_transport_policy::RTCIceTransportPolicy,
    },
};

pub(crate) struct ConnectionCore {
    pub connection: Arc<RTCPeerConnection>,
    pub control: UuKcpControl,
    runtime: tokio::runtime::Handle,
    cleanup_started: bool,
}

impl ConnectionCore {
    pub fn settings(self_hosted: bool) -> SettingEngine {
        let mut settings = SettingEngine::default();
        settings.set_sctp_max_message_size_can_send(SctpMaxMessageSize::Bounded(
            MAX_DATA_CHANNEL_MESSAGE_SIZE,
        ));
        settings.set_data_channel_receive_limit(MAX_DATA_CHANNEL_MESSAGE_SIZE as usize);
        settings.set_srtp_replay_protection_window(1024);
        settings.set_srtcp_replay_protection_window(128);
        settings.set_continual_gathering(true);
        if self_hosted {
            settings.set_vnet(Some(Arc::new(
                webrtc::util::vnet::net::Net::with_virtual_adapters(),
            )));
        }
        settings
    }

    pub async fn new(
        media: MediaEngine,
        registry: Registry,
        settings: SettingEngine,
        configuration: RTCConfiguration,
    ) -> Result<Self> {
        let connection = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(settings)
            .build()
            .new_peer_connection(configuration)
            .await
            .context("create native peer connection")?;
        Ok(Self {
            connection: Arc::new(connection),
            control: UuKcpControl::default(),
            runtime: tokio::runtime::Handle::current(),
            cleanup_started: false,
        })
    }

    pub async fn offer(&self, restart: bool, streams: &str) -> Result<String> {
        let options = restart.then_some(RTCOfferOptions {
            ice_restart: true,
            ..Default::default()
        });
        let offer = self
            .connection
            .create_offer(options)
            .await
            .context("create SDP offer")?;
        let mut wire = offer.sdp.clone();
        // Install the exact library-generated description. UU additions belong
        // to its signaling copy, not the library's cached local-description check.
        self.connection
            .set_local_description(offer)
            .await
            .context("install SDP offer")?;
        apply_uu_application_attributes_for_role(&mut wire, streams, Some(2))?;
        Ok(wire)
    }

    pub async fn answer(&self, streams: &str, mixed_kcp: Option<u8>) -> Result<String> {
        let answer = self
            .connection
            .create_answer(None)
            .await
            .context("create SDP answer")?;
        self.connection
            .set_local_description(answer)
            .await
            .context("install SDP answer")?;
        let mut wire = self
            .connection
            .local_description()
            .await
            .context("missing local SDP answer")?
            .sdp;
        apply_uu_application_attributes_for_role(&mut wire, streams, mixed_kcp)?;
        Ok(wire)
    }

    pub fn activate_control(&self, version: Option<u8>, receiver: ControlReceiver) -> Result<()> {
        if let Some(version) = version {
            self.control
                .start_receiver(self.connection.sctp(), version, receiver)?;
        }
        // T35CB5E: omission on an ICE restart does not retire the existing KCP.
        Ok(())
    }

    pub async fn close(&self) -> Result<()> {
        self.control.close().await;
        self.connection
            .close()
            .await
            .context("close native peer connection")
    }

    pub async fn configure_ice(
        &self,
        servers: Vec<RTCIceServer>,
        policy: RTCIceTransportPolicy,
    ) -> Result<()> {
        let mut configuration = self.connection.get_configuration().await;
        configuration.ice_servers = servers;
        configuration.ice_transport_policy = policy;
        self.connection
            .set_configuration(configuration)
            .await
            .context("apply ICE configuration")
    }

    /// Construction failure or exceptional owner drop must also release the
    /// transport. A role may await this task before joining its native workers.
    pub fn close_detached(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        if self.cleanup_started
            || (self.connection.connection_state() == RTCPeerConnectionState::Closed
                && !self.control.is_negotiated())
        {
            return None;
        }
        self.cleanup_started = true;
        self.control.stop();
        let control = self.control.clone();
        let connection = self.connection.clone();
        Some(self.runtime.spawn(async move {
            control.close().await;
            let _ = connection.close().await;
        }))
    }
}

impl Drop for ConnectionCore {
    fn drop(&mut self) {
        self.close_detached();
    }
}

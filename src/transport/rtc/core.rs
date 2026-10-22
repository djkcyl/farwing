//! Shared connection ownership and transport contracts for both roles.
//! Media registration and business authorization belong to the role adapters.
use super::negotiation::{MAX_DATA_CHANNEL_MESSAGE_SIZE, apply_application_attributes_for_role};
use crate::transport::fct::{ControlReceiver, FctControl};
use crate::transport::nat;
use anyhow::{Context, Result};
use std::sync::Arc;
use webrtc::{
    api::{
        APIBuilder,
        media_engine::MediaEngine,
        setting_engine::{SctpMaxMessageSize, SettingEngine},
    },
    ice_transport::{ice_candidate::RTCIceCandidateInit, ice_server::RTCIceServer},
    interceptor::registry::Registry,
    peer_connection::{
        RTCPeerConnection, configuration::RTCConfiguration, offer_answer_options::RTCOfferOptions,
        peer_connection_state::RTCPeerConnectionState,
        policy::ice_transport_policy::RTCIceTransportPolicy,
    },
};

/// Read buffer for each received datagram: any datagram of a 1500-byte MTU,
/// with room for relay framing.
const RECEIVE_MTU: usize = 2048;

pub(crate) struct ConnectionCore {
    pub connection: Arc<RTCPeerConnection>,
    pub control: FctControl,
    /// NAT info exchange, port prediction and gateway mappings (§11).
    pub nat: Arc<nat::Traversal>,
    runtime: tokio::runtime::Handle,
    cleanup_started: bool,
}

impl ConnectionCore {
    pub fn settings() -> SettingEngine {
        let mut settings = SettingEngine::default();
        settings.set_sctp_max_message_size_can_send(SctpMaxMessageSize::Bounded(
            MAX_DATA_CHANNEL_MESSAGE_SIZE,
        ));
        settings.set_data_channel_receive_limit(MAX_DATA_CHANNEL_MESSAGE_SIZE as usize);
        // webrtc-rs reads each datagram into a 1460-byte buffer by default
        // and silently drops longer ones; video packets fill a 1500-byte MTU
        // (up to 1472 bytes of UDP payload). `FARWING_RECEIVE_MTU` sets a
        // smaller one for measurements.
        settings.set_receive_mtu(
            std::env::var("FARWING_RECEIVE_MTU")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(RECEIVE_MTU),
        );
        settings.set_srtp_replay_protection_window(1024);
        settings.set_srtcp_replay_protection_window(128);
        settings.set_continual_gathering(true);
        // Probe punching is the other half of "aggressive punching" (§11).
        settings.set_probe_punching(crate::transport::nat::settings::load().aggressive_punching);
        settings
    }

    pub async fn new(
        media: MediaEngine,
        registry: Registry,
        settings: SettingEngine,
        configuration: RTCConfiguration,
    ) -> Result<Self> {
        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(settings)
            .build();
        // The NAT snapshot waits at most nat::LOCAL_INFO_BUDGET, alongside
        // the peer construction.
        let (connection, nat) = tokio::join!(
            api.new_peer_connection(configuration),
            nat::Traversal::new()
        );
        let connection = connection.context("create native peer connection")?;
        Ok(Self {
            connection: Arc::new(connection),
            control: FctControl::default(),
            nat,
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
        // Install the exact library-generated description. Farwing additions belong
        // to its signaling copy, not the library's cached local-description check.
        self.connection
            .set_local_description(offer)
            .await
            .context("install SDP offer")?;
        apply_application_attributes_for_role(&mut wire, streams, Some(crate::transport::fct::FCT_VERSION))?;
        nat::insert_attribute(&mut wire, &self.nat.attribute());
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
        apply_application_attributes_for_role(&mut wire, streams, mixed_kcp)?;
        nat::insert_attribute(&mut wire, &self.nat.attribute());
        Ok(wire)
    }

    /// After installing a remote offer or answer: record the peer's NAT
    /// attribute and add predicted candidates for srflx candidates the SDP
    /// already carries.
    pub async fn remote_description_installed(&self, sdp: &str) {
        self.nat.remote_description(sdp);
        for predicted in self.nat.sdp_predictions(sdp) {
            if let Err(error) = self.connection.add_ice_candidate(predicted).await {
                tracing::debug!(%error, "discarded predicted remote ICE candidate");
            }
        }
    }

    /// Install a remote candidate, then any predicted ports for it.
    pub async fn add_remote_candidate(&self, candidate: RTCIceCandidateInit) -> Result<()> {
        let predicted = self.nat.predictions(&candidate);
        self.connection
            .add_ice_candidate(candidate)
            .await
            .context("install remote ICE candidate")?;
        if !predicted.is_empty() {
            tracing::debug!(count = predicted.len(), "adding predicted remote srflx candidates");
        }
        for predicted in predicted {
            if let Err(error) = self.connection.add_ice_candidate(predicted).await {
                tracing::debug!(%error, "discarded predicted remote ICE candidate");
            }
        }
        Ok(())
    }

    pub fn activate_control(&self, version: Option<u8>, receiver: ControlReceiver) -> Result<()> {
        if let Some(version) = version {
            self.control
                .start_receiver(self.connection.sctp(), version, receiver)?;
        }
        // Omission on an ICE restart does not retire the existing KCP.
        Ok(())
    }

    pub async fn close(&self) -> Result<()> {
        self.summarize().await;
        self.control.close().await;
        // Gateway mappings are deleted while the transport closes.
        let (closed, ()) = tokio::join!(self.connection.close(), self.nat.close());
        closed.context("close native peer connection")
    }

    /// The connection summary, with the path it is on now.
    pub async fn summarize(&self) {
        let pair = self
            .connection
            .sctp()
            .transport()
            .ice_transport()
            .get_selected_candidate_pair()
            .await;
        match pair {
            Some(pair) => {
                let route = super::connection_route(
                    pair.local.typ,
                    &pair.local.address,
                    pair.remote.typ,
                    &pair.remote.address,
                );
                let relay = [&pair.local.relay_protocol, &pair.remote.relay_protocol]
                    .into_iter()
                    .find(|p| !p.is_empty())
                    .map(|p| format!(" via {p}"))
                    .unwrap_or_default();
                self.nat.summarize(
                    route,
                    &format!(
                        "{}/{} {}{relay}",
                        pair.local.typ, pair.remote.typ, pair.local.protocol
                    ),
                );
            }
            None => self.nat.summarize("", "never connected"),
        }
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
        // Mapping tasks delete their gateway mappings on their own.
        self.nat.summarize("", "");
        self.nat.close_detached();
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

//! Farwing Peer Protocol payloads and capability negotiation.

pub(crate) mod capability;
pub(crate) mod input;
pub(crate) mod pointer;
pub(crate) mod peer;

pub(crate) mod audio_control;

pub(crate) mod annotation;
pub(crate) mod drag_drop;
pub(crate) mod host_event;
pub(crate) mod participants;
pub(crate) mod microphone;

#[cfg(test)]
mod dispatch_tests {
    //! Every peer frame type has exactly one consumer. Dispatch is by the
    //! FPP header type, never by sniffing payload contents.
    use super::peer::{self, FrameType};

    const ALL: [FrameType; 22] = [
        FrameType::Hello,
        FrameType::Error,
        FrameType::Input,
        FrameType::PointerPolicy,
        FrameType::Cursor,
        FrameType::Touch,
        FrameType::Pen,
        FrameType::Pointer,
        FrameType::Session,
        FrameType::Audio,
        FrameType::HostEvent,
        FrameType::Clipboard,
        FrameType::File,
        FrameType::Transfer,
        FrameType::Drag,
        FrameType::Annotation,
        FrameType::Port,
        FrameType::PortDatagram,
        FrameType::Diagnostics,
        FrameType::Permissions,
        FrameType::Participants,
        FrameType::Flow,
    ];

    #[test]
    fn each_frame_type_is_claimed_only_by_its_consumer() {
        for kind in ALL {
            // An empty protobuf body is a valid default message.
            let bytes = peer::frame(kind, &[]);
            assert_eq!(
                super::input::decode(&bytes).unwrap().is_some(),
                kind == FrameType::Input,
                "input claimed {kind:?}"
            );
            assert_eq!(
                super::pointer::decode(&bytes).unwrap().is_some(),
                matches!(kind, FrameType::Touch | FrameType::Pen),
                "pointer input claimed {kind:?}"
            );
            assert_eq!(
                super::annotation::is_packet(&bytes),
                kind == FrameType::Annotation,
                "annotation claimed {kind:?}"
            );
            assert_eq!(
                super::drag_drop::is_packet(&bytes),
                kind == FrameType::Drag,
                "drag-drop claimed {kind:?}"
            );
            if kind != FrameType::Audio {
                assert!(super::audio_control::decode(&bytes).unwrap().is_none(), "audio claimed {kind:?}");
            }
            if kind != FrameType::Hello {
                assert!(peer::decode_hello(&bytes).unwrap().is_none(), "hello claimed {kind:?}");
            }
            assert_eq!(
                crate::features::stream_control::wire::decode(&bytes)
                    .map(|m| m.is_some())
                    .unwrap_or(true),
                kind == FrameType::Session,
                "session control claimed {kind:?}"
            );
        }
    }

    #[test]
    fn typed_messages_round_trip() {
        let audio = super::audio_control::encode(super::audio_control::Message::Set {
            request: 7,
            kbps: 192,
        });
        assert!(matches!(
            super::audio_control::decode(&audio).unwrap(),
            Some(super::audio_control::Message::Set { request: 7, kbps: 192 })
        ));
        let hello = peer::hello_request(peer::Role::Controller);
        let received = peer::decode_hello(&hello).unwrap().expect("hello frame");
        assert!(!received.response);
        let response = peer::hello_response(peer::Role::Host, received.request_id);
        let answered = peer::decode_hello(&response).unwrap().expect("hello response");
        assert!(answered.response);
        assert_eq!(answered.request_id, received.request_id);
    }

    #[test]
    fn fct_records_never_look_like_peer_frames_or_sctp() {
        // FCT records start with "FCT" + version; FPP frames with "FW".
        let record = [b'F', b'C', b'T', 1, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(!farwing_protocol::peer::is_frame(&record));
        assert!(&peer::frame(FrameType::Input, &[])[..2] == b"FW");
    }
}

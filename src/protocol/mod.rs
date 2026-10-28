//! Farwing Peer Protocol payloads and capability negotiation.

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

    const ALL: [FrameType; 20] = [
        FrameType::Hello,
        FrameType::Input,
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
                super::drag_drop::is_packet(&bytes),
                kind == FrameType::Drag,
                "drag-drop claimed {kind:?}"
            );
            assert_eq!(
                super::audio_control::decode(&bytes).unwrap().is_some(),
                kind == FrameType::Audio,
                "audio claimed {kind:?}"
            );
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
        let audio = super::audio_control::request(7, 192);
        let received = super::audio_control::decode(&audio).unwrap().expect("an AUDIO frame");
        assert_eq!(received.request_id, 7);
        assert!(matches!(
            received.body,
            Ok(super::audio_control::AudioMessage {
                body: Some(super::audio_control::Body::Request(super::audio_control::AudioRequest { kbps: 192 }))
            })
        ));
        let refused = super::audio_control::response(7, Err(super::audio_control::reason::NOT_ALLOWED));
        let received = super::audio_control::decode(&refused).unwrap().unwrap();
        assert_eq!(received.body.unwrap_err().reason, "not_allowed");
        use super::annotation::{self as ann, Op};
        let erase = ann::request(9, Op::Erase(ann::StrokeId { id: 3, screen: 0 })).unwrap();
        let received = ann::decode(&erase).unwrap().unwrap();
        assert_eq!((received.request_id, received.response), (9, false));
        assert_eq!(
            received.body.unwrap().op,
            Some(Op::Erase(ann::StrokeId { id: 3, screen: 0 }))
        );
        let answer = ann::decode(&ann::response(9, Ok(()))).unwrap().unwrap();
        assert!(answer.response && answer.body.unwrap().op.is_none());
        let refused = ann::decode(&ann::response(9, Err(ann::reason::LOCKED))).unwrap().unwrap();
        assert_eq!(refused.body.unwrap_err().reason, "locked");
        let reset = ann::decode(&ann::reset()).unwrap().unwrap();
        assert!(matches!(reset.body.unwrap().op, Some(Op::Reset(_))));
        let hello = peer::hello_request(peer::Role::Controller);
        let received = peer::decode_hello(&hello).unwrap().expect("hello frame");
        assert!(!received.response);
        assert_ne!(received.request_id, 0, "a HELLO is a request");
        let response = peer::hello_response(peer::Role::Host, received.request_id);
        let answered = peer::decode_hello(&response).unwrap().expect("hello response");
        assert!(answered.response);
        assert_eq!(answered.request_id, received.request_id);
    }

    #[test]
    fn unknown_requests_are_answered_unsupported() {
        let request = peer::framed(FrameType::Drag, 0, 9, &farwing_protocol::peer::Profile { color: 1 });
        let reply = peer::unsupported(&request).expect("a request is answered");
        let (header, body) = peer::decode(&reply).unwrap();
        assert!(header.is_response() && header.is_error());
        assert_eq!((header.frame_type(), header.request_id), (Some(FrameType::Drag), 9));
        let error = <farwing_protocol::peer::PeerError as prost::Message>::decode(body).unwrap();
        assert_eq!(error.code, farwing_protocol::peer::error_code::UNSUPPORTED);
        assert!(peer::unsupported(&peer::frame(FrameType::Drag, &[])).is_none(), "pushes are not");
    }

    #[test]
    fn fct_records_never_look_like_peer_frames_or_sctp() {
        // FCT records start with "FCT" + version; FPP frames with "FW".
        let record = [b'F', b'C', b'T', 1, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(!farwing_protocol::peer::is_frame(&record));
        assert!(&peer::frame(FrameType::Input, &[])[..2] == b"FW");
    }
}

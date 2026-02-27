use serde::{Deserialize, Serialize};

use crate::state::SessionMetadata;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(default)]
    pub metadata: Option<SessionMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WireMessage {
    PeerSend {
        from: String,
        to: String,
        message: String,
    },
    PeerSendAck {
        from: String,
        to: String,
        delivered: bool,
        daemon_id: String,
    },
    SessionAnnounce {
        id: String,
        daemon_id: String,
        #[serde(default)]
        daemon_name: String,
        #[serde(default)]
        metadata: Option<SessionMetadata>,
    },
    SessionList {
        sessions: Vec<SessionInfo>,
        daemon_id: String,
        daemon_name: String,
    },
    SessionRemove {
        id: String,
        daemon_id: String,
        #[serde(default)]
        daemon_name: String,
    },
    ConnectRequest {
        secret: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(msg: &WireMessage) -> WireMessage {
        let json = serde_json::to_string(msg).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn peer_send_round_trip() {
        let msg = WireMessage::PeerSend {
            from: "a".into(),
            to: "b".into(),
            message: "hello".into(),
        };
        let decoded = round_trip(&msg);
        assert!(matches!(decoded, WireMessage::PeerSend { from, to, message }
            if from == "a" && to == "b" && message == "hello"));
    }

    #[test]
    fn peer_send_ack_round_trip() {
        let msg = WireMessage::PeerSendAck {
            from: "a".into(),
            to: "b".into(),
            delivered: true,
            daemon_id: "d1".into(),
        };
        let decoded = round_trip(&msg);
        assert!(matches!(decoded, WireMessage::PeerSendAck { delivered, .. } if delivered));
    }

    #[test]
    fn session_announce_round_trip() {
        let msg = WireMessage::SessionAnnounce {
            id: "s1".into(),
            daemon_id: "npub1test".into(),
            daemon_name: "host1".into(),
            metadata: Some(SessionMetadata {
                vim_mode: true,
                project_dir: Some("/tmp".into()),
                role: None,
            }),
        };
        let decoded = round_trip(&msg);
        assert!(matches!(decoded, WireMessage::SessionAnnounce { id, daemon_name, metadata, .. }
            if id == "s1" && daemon_name == "host1" && metadata.clone().unwrap().vim_mode));
    }

    #[test]
    fn session_announce_no_metadata() {
        let msg = WireMessage::SessionAnnounce {
            id: "s1".into(),
            daemon_id: "d1".into(),
            daemon_name: String::new(),
            metadata: None,
        };
        let decoded = round_trip(&msg);
        assert!(matches!(decoded, WireMessage::SessionAnnounce { metadata, .. } if metadata.is_none()));
    }

    #[test]
    fn session_list_round_trip() {
        let msg = WireMessage::SessionList {
            sessions: vec![
                SessionInfo {
                    id: "s1".into(),
                    metadata: None,
                },
                SessionInfo {
                    id: "s2".into(),
                    metadata: Some(SessionMetadata::default()),
                },
            ],
            daemon_id: "d1".into(),
            daemon_name: "host1".into(),
        };
        let decoded = round_trip(&msg);
        assert!(matches!(decoded, WireMessage::SessionList { sessions, .. } if sessions.len() == 2));
    }

    #[test]
    fn session_remove_round_trip() {
        let msg = WireMessage::SessionRemove {
            id: "s1".into(),
            daemon_id: "d1".into(),
            daemon_name: "host1".into(),
        };
        let decoded = round_trip(&msg);
        assert!(matches!(decoded, WireMessage::SessionRemove { id, .. } if id == "s1"));
    }

    #[test]
    fn connect_request_round_trip() {
        let msg = WireMessage::ConnectRequest {
            secret: "a1b2c3d4e5f6".into(),
        };
        let decoded = round_trip(&msg);
        assert!(
            matches!(decoded, WireMessage::ConnectRequest { secret } if secret == "a1b2c3d4e5f6")
        );
    }

    #[test]
    fn wire_message_uses_type_tag() {
        let msg = WireMessage::PeerSend {
            from: "a".into(),
            to: "b".into(),
            message: "hi".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"PeerSend\""));
    }
}

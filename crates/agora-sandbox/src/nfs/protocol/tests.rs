//! Network filesystem protocol tests.

use super::{
    PROTOCOL_VERSION, RemotePath, Request, RequestEnvelope, RequestId, Response, ResponseEnvelope,
};

#[test]
fn protocol_round_trip_preserves_remote_operations() {
    let request = RequestEnvelope {
        version: PROTOCOL_VERSION,
        token: "run-token".to_string(),
        request_id: RequestId::new("0123456789abcdef0123456789abcdef").unwrap(),
        request: Request::Rename {
            from: RemotePath::new(2, "old/name").unwrap(),
            to: RemotePath::new(2, "new/name").unwrap(),
        },
    };
    let encoded = serde_json::to_vec(&request).unwrap();
    let decoded: RequestEnvelope = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, request);

    let response = ResponseEnvelope {
        version: PROTOCOL_VERSION,
        request_id: request.request_id.clone(),
        response: Response::List {
            anchor: "anchor-opaque".to_string(),
        },
    };
    let encoded = serde_json::to_vec(&response).unwrap();
    assert_eq!(
        serde_json::from_slice::<ResponseEnvelope>(&encoded).unwrap(),
        response
    );
}

#[test]
fn request_ids_reject_non_canonical_values() {
    assert!(RequestId::new("0123456789abcdef0123456789abcdef").is_ok());
    assert!(RequestId::new("short").is_err());
    assert!(RequestId::new("0123456789ABCDEF0123456789ABCDEF").is_err());
}

#[test]
fn remote_paths_reject_absolute_parent_and_non_utf8_equivalents() {
    assert!(RemotePath::new(0, "relative/path").is_ok());
    assert!(RemotePath::new(0, "").is_ok());
    assert!(RemotePath::new(0, "/absolute").is_err());
    assert!(RemotePath::new(0, "../escape").is_err());
    assert!(RemotePath::new(0, "a/../escape").is_err());
    assert!(RemotePath::new(0, "a//b").is_err());
    assert!(RemotePath::new(0, "a/./b").is_err());
    assert!(RemotePath::new(0, "a\\b").is_err());
    assert!(RemotePath::new(0, "a\0b").is_err());
}

#[test]
fn remote_path_deserialization_enforces_the_same_confinement_rules() {
    for path in ["/absolute", "../escape", "a/../escape", "a//b", "a/./b"] {
        let encoded = serde_json::json!({ "root": 0, "path": path });
        assert!(serde_json::from_value::<RemotePath>(encoded).is_err());
    }

    let root: RemotePath = serde_json::from_value(serde_json::json!({
        "root": 0,
        "path": ""
    }))
    .unwrap();
    assert_eq!(root.path(), "");
}

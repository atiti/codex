use super::*;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;

#[test]
fn matching_launcher_socket_uses_local_folder_consent() {
    let directory = tempfile::tempdir().expect("socket directory");
    let socket_path = AbsolutePathBuf::from_absolute_path(directory.path().join("owner.sock"))
        .expect("absolute socket path");
    let target = AppServerTarget::Remote {
        endpoint: RemoteAppServerEndpoint::UnixSocket {
            socket_path: socket_path.clone(),
        },
    };
    assert_eq!(
        project_trust_host_for_socket(&target, Some(socket_path.as_os_str())),
        ProjectTrustHost::Local,
    );
}

#[test]
fn missing_or_mismatched_launcher_socket_keeps_remote_folder_consent() {
    let directory = tempfile::tempdir().expect("socket directory");
    let socket_path = AbsolutePathBuf::from_absolute_path(directory.path().join("remote.sock"))
        .expect("absolute socket path");
    let other_socket = directory.path().join("owner.sock");
    let target = AppServerTarget::Remote {
        endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
    };
    for local_socket in [None, Some(other_socket.as_os_str())] {
        assert_eq!(
            project_trust_host_for_socket(&target, local_socket),
            ProjectTrustHost::Remote,
        );
    }
}

#[test]
fn websocket_endpoint_keeps_remote_folder_consent() {
    let target = AppServerTarget::Remote {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: "ws://127.0.0.1:1234".to_string(),
            auth_token: None,
        },
    };
    assert_eq!(
        project_trust_host_for_socket(&target, Some(OsStr::new("ws://127.0.0.1:1234"))),
        ProjectTrustHost::Remote,
    );
}

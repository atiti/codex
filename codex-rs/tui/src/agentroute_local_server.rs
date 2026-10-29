//! Uses local folder discovery for the shared owner explicitly identified by AgentRoute.

use crate::AppServerTarget;
use crate::RemoteAppServerEndpoint;
use crate::config_update::ProjectTrustHost;
use std::ffi::OsStr;
use std::path::Path;

const LOCAL_SERVER_SOCKET_ENV: &str = "AGENTROUTE_LOCAL_SERVER_SOCKET";

pub(crate) fn project_trust_host(target: &AppServerTarget) -> ProjectTrustHost {
    project_trust_host_for_socket(target, std::env::var_os(LOCAL_SERVER_SOCKET_ENV).as_deref())
}

fn project_trust_host_for_socket(
    target: &AppServerTarget,
    local_socket: Option<&OsStr>,
) -> ProjectTrustHost {
    match target {
        AppServerTarget::Remote {
            endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
        } if local_socket.is_some_and(|path| socket_path.as_path() == Path::new(path)) => {
            // The launcher owns this socket on the same filesystem. Only folder
            // consent uses local semantics; session discovery and updates stay remote.
            ProjectTrustHost::Local
        }
        AppServerTarget::Remote { .. } => ProjectTrustHost::Remote,
        AppServerTarget::Embedded | AppServerTarget::LocalDaemon { .. } => ProjectTrustHost::Local,
    }
}

#[cfg(test)]
#[path = "agentroute_local_server_tests.rs"]
mod tests;

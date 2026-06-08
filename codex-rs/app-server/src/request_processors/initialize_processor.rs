use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use axum::http::HeaderValue;
use codex_analytics::AppServerRpcTransport;
use codex_login::default_client::SetOriginatorError;
use codex_login::default_client::USER_AGENT_SUFFIX;
use codex_login::default_client::get_codex_user_agent;
use codex_login::default_client::set_default_client_residency_requirement;
use codex_login::default_client::set_default_originator;

use super::*;
use crate::message_processor::ConnectionSessionState;
use crate::message_processor::InitializedConnectionSessionState;

const NON_ORIGINATING_CLIENT_NAMES: &[&str] = &["codex_app_server_daemon", "codex-backend"];

#[derive(Clone)]
pub(crate) struct InitializeRequestProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    analytics_events_client: AnalyticsEventsClient,
    config: Arc<Config>,
    config_warnings: Arc<Vec<ConfigWarningNotification>>,
    rpc_transport: AppServerRpcTransport,
}

impl InitializeRequestProcessor {
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        analytics_events_client: AnalyticsEventsClient,
        config: Arc<Config>,
        config_warnings: Vec<ConfigWarningNotification>,
        rpc_transport: AppServerRpcTransport,
    ) -> Self {
        Self {
            outgoing,
            analytics_events_client,
            config,
            config_warnings: Arc::new(config_warnings),
            rpc_transport,
        }
    }

    pub(crate) async fn initialize(
        &self,
        connection_id: ConnectionId,
        request_id: RequestId,
        params: InitializeParams,
        session: &ConnectionSessionState,
        // `Some(...)` means the caller wants initialize to immediately mark the
        // connection outbound-ready. Websocket JSON-RPC calls pass `None` so
        // lib.rs can deliver connection-scoped initialize notifications first.
        outbound_initialized: Option<&AtomicBool>,
    ) -> Result<bool, JSONRPCErrorError> {
        let connection_request_id = ConnectionRequestId {
            connection_id,
            request_id,
        };
        if session.initialized() {
            return Err(invalid_request("Already initialized"));
        }

        // TODO(maxj): Revisit capability scoping for `experimental_api_enabled`.
        // Current behavior is per-connection. Reviewer feedback notes this can
        // create odd cross-client behavior (for example dynamic tool calls on a
        // shared thread when another connected client did not opt into
        // experimental API). Proposed direction is instance-global first-write-wins
        // with initialize-time mismatch rejection.
        let analytics_initialize_params = params.clone();
        let (experimental_api_enabled, request_attestation, opt_out_notification_methods) =
            match params.capabilities {
                Some(capabilities) => (
                    capabilities.experimental_api,
                    capabilities.request_attestation,
                    capabilities
                        .opt_out_notification_methods
                        .unwrap_or_default(),
                ),
                None => (false, false, Vec::new()),
            };
        let ClientInfo {
            name,
            title: _title,
            version,
        } = params.client_info;
        // Validate before committing; set_default_originator validates while
        // mutating process-global metadata.
        if HeaderValue::from_str(&name).is_err() {
            return Err(invalid_request(format!(
                "Invalid clientInfo.name: '{name}'. Must be a valid HTTP header value."
            )));
        }
        let originator = name.clone();
        let user_agent_suffix = format!("{name}; {version}");
        let mutates_global_identity = !NON_ORIGINATING_CLIENT_NAMES.contains(&name.as_str());
        let codex_home = self.config.codex_home.clone();
        if session
            .initialize(InitializedConnectionSessionState {
                experimental_api_enabled,
                opted_out_notification_methods: opt_out_notification_methods.into_iter().collect(),
                app_server_client_name: name.clone(),
                client_version: version,
                request_attestation,
            })
            .is_err()
        {
            return Err(invalid_request("Already initialized"));
        }

        if mutates_global_identity {
            // Only real client initialization may mutate process-global client metadata.
            if let Err(error) = set_default_originator(originator.clone()) {
                match error {
                    SetOriginatorError::InvalidHeaderValue => {
                        tracing::warn!(
                            client_info_name = %name,
                            "validated clientInfo.name was rejected while setting originator"
                        );
                    }
                    SetOriginatorError::AlreadyInitialized => {
                        // No-op. This is expected to happen if the originator is already set via env var.
                        // TODO(owen): Once we remove support for CODEX_INTERNAL_ORIGINATOR_OVERRIDE,
                        // this will be an unexpected state and we can return a JSON-RPC error indicating
                        // internal server error.
                    }
                }
            }
        }
        self.analytics_events_client.track_initialize(
            connection_id.0,
            analytics_initialize_params,
            originator,
            self.rpc_transport,
        );
        set_default_client_residency_requirement(self.config.enforce_residency.value());
        if mutates_global_identity && let Ok(mut suffix) = USER_AGENT_SUFFIX.lock() {
            *suffix = Some(user_agent_suffix);
        }

        let user_agent = get_codex_user_agent();
        let desktop_wsl_warning =
            desktop_wsl_windows_codex_home_warning(&name, codex_home.as_path(), is_wsl_runtime());
        let response = InitializeResponse {
            user_agent,
            codex_home,
            platform_family: std::env::consts::FAMILY.to_string(),
            platform_os: std::env::consts::OS.to_string(),
        };

        self.outgoing
            .send_response(connection_request_id, response)
            .await;

        if let Some(notification) = desktop_wsl_warning {
            self.outgoing
                .send_server_notification_to_connections(
                    &[connection_id],
                    ServerNotification::ConfigWarning(notification),
                )
                .await;
        }

        if let Some(outbound_initialized) = outbound_initialized {
            outbound_initialized.store(true, Ordering::Release);
            return Ok(true);
        }

        Ok(false)
    }

    pub(crate) async fn send_initialize_notifications_to_connection(
        &self,
        connection_id: ConnectionId,
    ) {
        for notification in self.config_warnings.iter().cloned() {
            self.outgoing
                .send_server_notification_to_connections(
                    &[connection_id],
                    ServerNotification::ConfigWarning(notification),
                )
                .await;
        }
    }

    pub(crate) async fn send_initialize_notifications(&self) {
        for notification in self.config_warnings.iter().cloned() {
            self.outgoing
                .send_server_notification(ServerNotification::ConfigWarning(notification))
                .await;
        }
    }

    pub(crate) fn track_initialized_request(
        &self,
        connection_id: ConnectionId,
        request_id: RequestId,
        request: &ClientRequest,
    ) {
        self.analytics_events_client
            .track_request(connection_id.0, request_id, request);
    }
}

fn desktop_wsl_windows_codex_home_warning(
    client_name: &str,
    codex_home: &Path,
    is_wsl: bool,
) -> Option<ConfigWarningNotification> {
    if !is_wsl
        || !is_codex_desktop_client(client_name)
        || !looks_like_windows_path_in_wsl(codex_home)
    {
        return None;
    }

    let codex_home = codex_home.display();
    Some(ConfigWarningNotification {
        summary: "Codex Desktop is running in WSL with Windows-backed CODEX_HOME.".to_string(),
        details: Some(format!(
            "Codex persistent state is currently loaded from {codex_home}. This preserves existing Windows Desktop auth and threads while plugin and bundled skill caches can use native WSL storage. To move all Codex state to native WSL, start the WSL app-server with CODEX_DESKTOP_WSL_NATIVE_CODEX_HOME=1 after warning that native WSL state may require signing in again unless auth is migrated."
        )),
        path: None,
        range: None,
    })
}

fn is_codex_desktop_client(client_name: &str) -> bool {
    matches!(
        client_name,
        "Codex Desktop" | "codex_desktop" | "codex-desktop" | "codex_chatgpt_desktop"
    )
}

fn looks_like_windows_path_in_wsl(path: &Path) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let bytes = normalized.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() && bytes[2] == b'/' {
        return true;
    }

    let mut components = normalized.split('/').filter(|part| !part.is_empty());
    let Some(mnt) = components.next() else {
        return false;
    };
    if !mnt.eq_ignore_ascii_case("mnt") {
        return false;
    }
    let Some(drive) = components.next() else {
        return false;
    };
    let drive = drive.as_bytes();
    drive.len() == 1 && drive[0].is_ascii_alphabetic()
}

fn is_wsl_runtime() -> bool {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WSL_DISTRO_NAME").is_some() {
            return true;
        }
        match std::fs::read_to_string("/proc/version") {
            Ok(version) => version.to_lowercase().contains("microsoft"),
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::desktop_wsl_windows_codex_home_warning;

    #[test]
    fn desktop_wsl_windows_codex_home_warning_describes_state_tradeoff() {
        let warning = desktop_wsl_windows_codex_home_warning(
            "codex_chatgpt_desktop",
            Path::new("/mnt/c/Users/alice/.codex"),
            /*is_wsl*/ true,
        )
        .expect("warning");

        assert!(
            warning.summary.contains("Windows-backed CODEX_HOME"),
            "unexpected summary: {}",
            warning.summary
        );
        let details = warning.details.expect("details");
        assert!(
            details.contains("CODEX_DESKTOP_WSL_NATIVE_CODEX_HOME=1"),
            "unexpected details: {details}"
        );
        assert!(
            details.contains("plugin and bundled skill caches"),
            "unexpected details: {details}"
        );
        assert!(
            details.contains("signing in again"),
            "unexpected details: {details}"
        );
    }

    #[test]
    fn desktop_wsl_windows_codex_home_warning_ignores_non_desktop_clients() {
        assert!(
            desktop_wsl_windows_codex_home_warning(
                "codex_cli",
                Path::new("/mnt/c/Users/alice/.codex"),
                /*is_wsl*/ true,
            )
            .is_none()
        );
    }

    #[test]
    fn desktop_wsl_windows_codex_home_warning_ignores_native_wsl_home() {
        assert!(
            desktop_wsl_windows_codex_home_warning(
                "codex_chatgpt_desktop",
                Path::new("/home/alice/.codex"),
                /*is_wsl*/ true,
            )
            .is_none()
        );
    }
}

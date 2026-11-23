use atalanta_common::jsonline::JsonLineClient;
use tokio::process::Child;
use tokio::time::Duration;

use crate::config::FlutterBridgeConfig;
use crate::protocol::{FlutterCommand, FlutterResponse};

/// Manages the connection to the Flutter test process.
pub struct FlutterProcess {
    config: FlutterBridgeConfig,
    /// The process, when the driver started it. Only then may it be shut down.
    child: Option<Child>,
    client: JsonLineClient,
}

impl FlutterProcess {
    pub fn new(config: FlutterBridgeConfig) -> Self {
        let mut client = JsonLineClient::new("Flutter bridge", config.port);
        client.timeout = Duration::from_secs(config.reply_timeout_secs.max(1));
        Self {
            config,
            child: None,
            client,
        }
    }

    /// Connect to the bridge the app's test entry point is running.
    pub async fn start(&mut self) -> anyhow::Result<()> {
        if let Err(e) = self.client.connect().await {
            tracing::info!("No Flutter bridge to connect to: {e}");
            anyhow::bail!(
                "Flutter bridge not running on port {} for project {}. \
                 Add the atalanta_bridge Dart package (crates/atalanta-flutter-bridge/dart) \
                 to the app and start it before tests — see the atalanta-flutter-bridge README.",
                self.config.port,
                self.config.project_path
            )
        }
        Ok(())
    }

    /// Send a command and receive its reply.
    ///
    /// Requests carry ids, so a late reply is never taken for the answer to
    /// a later command; a timed-out or failed command also drops the
    /// connection.
    pub async fn send(&mut self, cmd: &FlutterCommand) -> anyhow::Result<FlutterResponse> {
        self.client.request(cmd).await
    }

    /// Disconnect. The bridge is shut down only if this driver started it:
    /// one the user started keeps serving the next run (VEL-140).
    pub async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if self.client.is_connected() {
                let _ = self.send(&FlutterCommand::Shutdown).await;
            }
            let _ = child.kill().await;
        }
        self.client.disconnect();
    }

    pub fn is_running(&self) -> bool {
        self.client.is_connected()
    }

    /// Whether this driver started the bridge process.
    pub fn owns_process(&self) -> bool {
        self.child.is_some()
    }
}

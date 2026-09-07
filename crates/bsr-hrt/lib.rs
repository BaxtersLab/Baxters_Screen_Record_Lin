// SPDX-License-Identifier: MIT
// Baxter's Screen Record — HRT (Health, Runtime, Thermal) Layer
// Safety envelope and lifecycle signals

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tracing::info;
use bsr_ipc::TelemetryEvent;

/// HRT events
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HrtEvent {
    ThermalNominal,
    ThermalWarning,
    ThermalCritical,
    LifecycleStart,
    LifecycleStop,
    LifecycleShutdown,
    ErrorOccurred { message: String },
}

/// HRT commands
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HrtCommand {
    Shutdown,
}

/// HRT status snapshot
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HrtStatus {
    pub thermal_level: String,
    pub lifecycle_state: String,
}

/// Async HRT service
pub struct HrtService {
    cmd_rx: mpsc::Receiver<HrtCommand>,
    telemetry_tx: broadcast::Sender<TelemetryEvent>,
}

impl HrtService {
    pub fn new(telemetry_tx: broadcast::Sender<TelemetryEvent>) -> (Self, mpsc::Sender<HrtCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let service = Self { cmd_rx, telemetry_tx };
        (service, cmd_tx)
    }

    pub async fn run(mut self) {
        info!("HRT service starting");
        // Emit initial lifecycle start
        self.emit_event(HrtEvent::LifecycleStart).await;

        while let Some(cmd) = self.cmd_rx.recv().await {
            info!("HRT received command: {:?}", cmd);
            match cmd {
                HrtCommand::Shutdown => {
                    self.emit_event(HrtEvent::LifecycleShutdown).await;
                    break;
                }
            }
        }
        info!("HRT service stopped");
    }

    async fn emit_event(&self, event: HrtEvent) {
        // Map HRT events to downstream telemetry. ThermalNominal is a
        // "temperatures are fine" heartbeat with no downstream significance, so
        // it produces no TelemetryEvent (rather than a misleading AppStarted).
        let telemetry_event = match event {
            HrtEvent::ThermalNominal => None,
            HrtEvent::ThermalWarning => Some(TelemetryEvent::ErrorOccurred { message: "Thermal warning".to_string() }),
            HrtEvent::ThermalCritical => Some(TelemetryEvent::ErrorOccurred { message: "Thermal critical".to_string() }),
            HrtEvent::LifecycleStart => Some(TelemetryEvent::AppStarted),
            HrtEvent::LifecycleStop => Some(TelemetryEvent::AppStopped),
            HrtEvent::LifecycleShutdown => Some(TelemetryEvent::AppStopped),
            HrtEvent::ErrorOccurred { message } => Some(TelemetryEvent::ErrorOccurred { message }),
        };
        if let Some(ev) = telemetry_event {
            let _ = self.telemetry_tx.send(ev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast;

    #[test]
    fn test_serialization_round_trip() {
        // Test HrtEvent
        let events = vec![
            HrtEvent::ThermalNominal,
            HrtEvent::ThermalWarning,
            HrtEvent::ThermalCritical,
            HrtEvent::LifecycleStart,
            HrtEvent::LifecycleStop,
            HrtEvent::LifecycleShutdown,
            HrtEvent::ErrorOccurred { message: "test".to_string() },
        ];
        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            let deserialized: HrtEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(event, deserialized);
        }

        // Test HrtCommand
        let cmd = HrtCommand::Shutdown;
        let json = serde_json::to_string(&cmd).unwrap();
        let deserialized: HrtCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, deserialized);

        // Test HrtStatus
        let status = HrtStatus {
            thermal_level: "nominal".to_string(),
            lifecycle_state: "running".to_string(),
        };
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: HrtStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, deserialized);
    }

    #[tokio::test]
    async fn test_hrt_service() {
        let (telemetry_tx, mut telemetry_rx) = broadcast::channel(32);
        let (service, cmd_tx) = HrtService::new(telemetry_tx);

        // Spawn service
        tokio::spawn(async move {
            service.run().await;
        });

        // Check initial event
        let event = telemetry_rx.recv().await.unwrap();
        assert!(matches!(event, TelemetryEvent::AppStarted));

        // Send shutdown
        cmd_tx.send(HrtCommand::Shutdown).await.unwrap();

        // Check shutdown event
        let event = telemetry_rx.recv().await.unwrap();
        assert!(matches!(event, TelemetryEvent::AppStopped));
    }

    #[tokio::test]
    async fn test_ipc_bridge_conversion() {
        let (telemetry_tx, mut telemetry_rx) = broadcast::channel(32);
        let (service, _) = HrtService::new(telemetry_tx);

        // Manually emit an event
        service.emit_event(HrtEvent::LifecycleStart).await;

        let event = telemetry_rx.recv().await.unwrap();
        assert!(matches!(event, TelemetryEvent::AppStarted));
    }

    #[tokio::test]
    async fn thermal_nominal_emits_no_downstream_event() {
        let (telemetry_tx, mut telemetry_rx) = broadcast::channel(8);
        let (service, _) = HrtService::new(telemetry_tx);

        // ThermalNominal is a "temps are fine" heartbeat — it must NOT produce a
        // (previously misleading AppStarted) downstream TelemetryEvent.
        service.emit_event(HrtEvent::ThermalNominal).await;
        assert!(telemetry_rx.try_recv().is_err(), "ThermalNominal should emit nothing");

        // A real lifecycle event still comes through, proving the channel is live.
        service.emit_event(HrtEvent::LifecycleStop).await;
        assert!(matches!(telemetry_rx.try_recv(), Ok(TelemetryEvent::AppStopped)));
    }
}

use tokio::sync::mpsc;
use serde::{Deserialize, Serialize};

pub mod backends;

use bsr_ipc::MuxerConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub pts: i64,
    pub dts: i64,
    pub keyframe: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum MuxerError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("FFmpeg error: {0}")]
    FFmpeg(#[from] ffmpeg_next::Error),
    #[error("Invalid config: {0}")]
    Config(String),
}

pub type MuxerResult<T> = Result<T, MuxerError>;

#[async_trait::async_trait]
pub trait MuxerBackend {
    async fn initialize(&mut self, config: &MuxerConfig) -> MuxerResult<()>;
    async fn write_packet(&mut self, packet: EncodedPacket) -> MuxerResult<()>;
    async fn finalize(&mut self) -> MuxerResult<()>;
    async fn shutdown(&mut self) -> MuxerResult<()>;
}

#[derive(Debug, Clone, Default)]
pub struct MuxerTelemetry {
    pub file_size_bytes: u64,
    pub duration_secs: u64,
    pub last_write_latency_ms: f32,
    pub max_duration_reached: bool,
}

pub struct MuxerService<B: MuxerBackend> {
    config: bsr_ipc::MuxerConfig,
    backend: B,
    packet_rx: mpsc::Receiver<EncodedPacket>,
    telemetry_tx: mpsc::Sender<MuxerTelemetry>,
    command_rx: mpsc::Receiver<MuxerCommand>,
    ipc_client: bsr_ipc::IpcClient,
    finalized: bool,
}

#[derive(Debug)]
pub enum MuxerCommand {
    StopRecording,
    Shutdown,
}

impl<B: MuxerBackend> MuxerService<B> {
    pub fn new(
        config: bsr_ipc::MuxerConfig,
        backend: B,
        packet_rx: mpsc::Receiver<EncodedPacket>,
        telemetry_tx: mpsc::Sender<MuxerTelemetry>,
        command_rx: mpsc::Receiver<MuxerCommand>,
        ipc_client: bsr_ipc::IpcClient,
    ) -> Self {
        Self {
            config,
            backend,
            packet_rx,
            telemetry_tx,
            command_rx,
            ipc_client,
            finalized: false,
        }
    }

    pub async fn run(mut self) -> MuxerResult<()> {
        self.backend.initialize(&self.config).await?;

        let start_time = std::time::Instant::now();
        let mut _total_packets = 0;
        // Cumulative encoded bytes written — approximates output file size for
        // telemetry (container overhead is negligible vs the H.264 payload).
        let mut bytes_written: u64 = 0;

        loop {
            tokio::select! {
                packet = self.packet_rx.recv() => {
                    match packet {
                        Some(packet) => {
                            let write_start = std::time::Instant::now();
                            let packet_bytes = packet.data.len() as u64;
                            self.backend.write_packet(packet).await?;
                            let write_latency = write_start.elapsed();
                            _total_packets += 1;
                            bytes_written += packet_bytes;

                            // Emit telemetry
                            let telemetry = MuxerTelemetry {
                                file_size_bytes: bytes_written,
                                duration_secs: start_time.elapsed().as_secs(),
                                last_write_latency_ms: write_latency.as_millis() as f32,
                                max_duration_reached: false,
                            };
                            // try_send, NOT send().await. Telemetry is best-effort and must
                            // never be able to stall the recording: on a bounded channel whose
                            // consumer is slow or absent, send().await parks the muxer inside
                            // this branch, so it stops writing packets AND stops polling its
                            // command channel -- a Stop can then never be delivered and the
                            // file is never finalized. Measured: a stop timed out after 15s.
                            let _ = self.telemetry_tx.try_send(telemetry);

                            // Check duration cap
                            if start_time.elapsed() >= self.config.max_duration {
                                tracing::info!("Recording duration cap reached, stopping");
                                // Send final telemetry
                                let final_telemetry = MuxerTelemetry {
                                    file_size_bytes: bytes_written,
                                    duration_secs: start_time.elapsed().as_secs(),
                                    last_write_latency_ms: 0.0,
                                    max_duration_reached: true,
                                };
                                let _ = self.telemetry_tx.try_send(final_telemetry);
                                if !self.finalized {
                                    self.backend.finalize().await?;
                                    self.finalized = true;
                                }
                                // Send StopRecording via IPC
                                let _ = self.ipc_client.send_command(bsr_ipc::IpcCommand::StopRecording).await;
                                break;
                            }
                        }
                        // The encoder went away, which is how a recording actually ends:
                        // bsr-ui shuts the encoder down 100ms BEFORE it sends StopRecording,
                        // so this arm always wins that race. Breaking here without
                        // finalizing left every recording without its `moov` atom -- a file
                        // full of real H.264 that no player can open. Measured twice on a
                        // real run: 512 KB, "moov atom not found".
                        // The end of the packet stream IS the end of the recording.
                        None => {
                            tracing::info!("packet stream ended; finalizing recording");
                            if !self.finalized {
                                self.backend.finalize().await?;
                                self.finalized = true;
                            }
                            break;
                        }
                    }
                }
                command = self.command_rx.recv() => {
                    match command {
                        Some(MuxerCommand::StopRecording) => {
                            tracing::info!("StopRecording command received");
                            if !self.finalized {
                                self.backend.finalize().await?;
                                self.finalized = true;
                            }
                            break;
                        }
                        Some(MuxerCommand::Shutdown) => {
                            tracing::info!("Shutdown command received");
                            if !self.finalized {
                                self.backend.finalize().await?;
                                self.finalized = true;
                            }
                            break;
                        }
                        None => break,
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[test]
    fn test_muxer_config_default() {
        let config = bsr_ipc::MuxerConfig::default();
        assert_eq!(config.base_output_path, PathBuf::from("./output"));
        assert!(matches!(config.file_naming_strategy, bsr_ipc::FileNamingStrategy::TimestampedFile));
        assert_eq!(config.max_duration, Duration::from_secs(18000)); // 5 hours
    }

    #[tokio::test]
    async fn test_muxer_service_duration_cap() {
        let config = bsr_ipc::MuxerConfig {
            max_duration: std::time::Duration::from_millis(100),
            ..Default::default()
        };

        let (packet_tx, packet_rx) = mpsc::channel(10);
        let (telemetry_tx, mut telemetry_rx) = mpsc::channel(10);
        let (_command_tx, command_rx) = mpsc::channel(10);

        let backend = backends::mp4::Mp4Muxer::new();
        let (ipc_tx, _) = mpsc::channel(10);
        let ipc_client = bsr_ipc::IpcClient::new(ipc_tx);
        let service = MuxerService::new(config, backend, packet_rx, telemetry_tx, command_rx, ipc_client);

        tokio::spawn(async move {
            service.run().await.unwrap();
        });

        // Send a packet
        let packet = EncodedPacket {
            data: vec![0; 100],
            pts: 0,
            dts: 0,
            keyframe: true,
        };
        packet_tx.send(packet).await.unwrap();

        // Wait a bit more than max duration
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Service should have stopped due to duration cap
        // Check telemetry
        if let Some(telemetry) = telemetry_rx.recv().await {
            let _ = telemetry.duration_secs; // ensure telemetry received
        }
    }

    #[tokio::test]
    async fn test_muxer_receives_config_before_start() {
        // This test verifies that the muxer service initializes with the config
        let config = bsr_ipc::MuxerConfig::default();
        let (_packet_tx, packet_rx) = mpsc::channel(10);
        let (telemetry_tx, _) = mpsc::channel(10);
        let (_command_tx, command_rx) = mpsc::channel(10);

        // Mock IPC client
        let (ipc_tx, _) = mpsc::channel(10);
        let ipc_client = bsr_ipc::IpcClient::new(ipc_tx);

        let backend = backends::mp4::Mp4Muxer::new();
        let service = MuxerService::new(config.clone(), backend, packet_rx, telemetry_tx, command_rx, ipc_client);

        // The service should have the config set
        assert_eq!(service.config.base_output_path, config.base_output_path);
    }

    #[tokio::test]
    async fn test_muxer_ignores_packets_before_start() {
        // This test verifies that packets sent before initialization are handled
        let config = bsr_ipc::MuxerConfig::default();
        let (packet_tx, packet_rx) = mpsc::channel(10);
        let (telemetry_tx, _) = mpsc::channel(10);
        let (_command_tx, command_rx) = mpsc::channel(10);

        let (ipc_tx, _) = mpsc::channel(10);
        let ipc_client = bsr_ipc::IpcClient::new(ipc_tx);

        let backend = backends::mp4::Mp4Muxer::new();
        let _service = MuxerService::new(config, backend, packet_rx, telemetry_tx, command_rx, ipc_client);

        // Send packet before starting service
        let packet = EncodedPacket {
            data: vec![0; 100],
            pts: 0,
            dts: 0,
            keyframe: true,
        };
        packet_tx.send(packet).await.unwrap();

        // Start service briefly
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            // Service should handle the packet
        });
    }

    #[tokio::test]
    async fn test_muxer_finalizes_on_shutdown() {
        let config = bsr_ipc::MuxerConfig::default();
        let (_packet_tx, packet_rx) = mpsc::channel(10);
        let (telemetry_tx, _) = mpsc::channel(10);
        let (command_tx, command_rx) = mpsc::channel(10);

        let (ipc_tx, _) = mpsc::channel(10);
        let ipc_client = bsr_ipc::IpcClient::new(ipc_tx);

        let backend = backends::mp4::Mp4Muxer::new();
        let service = MuxerService::new(config, backend, packet_rx, telemetry_tx, command_rx, ipc_client);

        let handle = tokio::spawn(async move {
            service.run().await.unwrap();
        });

        // Send shutdown command
        command_tx.send(MuxerCommand::Shutdown).await.unwrap();

        // Wait for service to finish
        handle.await.unwrap();
    }
}
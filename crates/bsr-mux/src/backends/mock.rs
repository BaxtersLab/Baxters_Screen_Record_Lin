// SPDX-License-Identifier: MIT
use crate::{MuxerBackend, MuxerResult, EncodedPacket};
use async_trait::async_trait;
use bsr_ipc::MuxerConfig;

pub struct MockMuxer {
    initialized: bool,
    packets_written: usize,
    finalized: bool,
}

impl MockMuxer {
    pub fn new() -> Self {
        Self {
            initialized: false,
            packets_written: 0,
            finalized: false,
        }
    }
}

#[async_trait]
impl MuxerBackend for MockMuxer {
    async fn initialize(&mut self, _config: &MuxerConfig) -> MuxerResult<()> {
        self.initialized = true;
        Ok(())
    }

    async fn write_packet(&mut self, _packet: EncodedPacket) -> MuxerResult<()> {
        if !self.initialized {
            return Err(crate::MuxerError::Config("Not initialized".to_string()));
        }
        self.packets_written += 1;
        Ok(())
    }

    async fn finalize(&mut self) -> MuxerResult<()> {
        if !self.initialized {
            return Err(crate::MuxerError::Config("Not initialized".to_string()));
        }
        self.finalized = true;
        Ok(())
    }

    async fn shutdown(&mut self) -> MuxerResult<()> {
        self.finalized = true;
        Ok(())
    }
}
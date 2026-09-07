// SPDX-License-Identifier: MIT
// Unit tests for bsr-core/config.rs

#[cfg(test)]
mod tests {
    use crate::config::BsrConfig;

    #[test]
    fn test_default_config() {
        let config = BsrConfig::default();
        assert!(config.output.output_folder.contains("BSR"));
        assert_eq!(config.hotkeys.start_recording, "Ctrl+Shift+R");
    }
}

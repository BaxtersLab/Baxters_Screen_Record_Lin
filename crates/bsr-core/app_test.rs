// SPDX-License-Identifier: MIT
// Unit tests for bsr-core/app.rs

#[cfg(test)]
mod tests {
    use crate::app::{AppState, AppMode, RecordingState};
    use crate::config::BsrConfig;

    #[test]
    fn test_app_state_init() {
        let config = BsrConfig::default();
        let app_state = AppState::new(config);
        assert_eq!(*app_state.app_mode_rx().borrow(), AppMode::Visible);
        assert_eq!(*app_state.recording_state_rx().borrow(), RecordingState::Idle);
    }

    #[test]
    fn test_set_app_mode() {
        let config = BsrConfig::default();
        let app_state = AppState::new(config);
        app_state.set_app_mode(AppMode::Stealth);
        assert_eq!(*app_state.app_mode_rx().borrow(), AppMode::Stealth);
    }

    #[test]
    fn test_set_recording_state() {
        let config = BsrConfig::default();
        let app_state = AppState::new(config);
        app_state.set_recording_state(RecordingState::Recording);
        assert_eq!(*app_state.recording_state_rx().borrow(), RecordingState::Recording);
    }

    #[test]
    fn test_single_instance_guard_excludes_second_instance() {
        use crate::app::SingleInstanceGuard;
        // Uncommon fixed loopback port so the test doesn't clash with the app.
        const TEST_PORT: u16 = 52111;
        let first = SingleInstanceGuard::acquire(TEST_PORT).expect("first acquire should succeed");
        // While the first guard is held, a second acquire on the same port fails.
        assert!(
            SingleInstanceGuard::acquire(TEST_PORT).is_err(),
            "second acquire must fail while first is held"
        );
        // After releasing the first, the port is available again.
        drop(first);
        assert!(
            SingleInstanceGuard::acquire(TEST_PORT).is_ok(),
            "re-acquire after release should succeed"
        );
    }
}

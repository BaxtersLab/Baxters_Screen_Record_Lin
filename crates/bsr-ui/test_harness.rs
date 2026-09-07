// SPDX-License-Identifier: MIT
//! A headless egui harness for `AppWindow`.
//!
//! Four defects were found in the save modal by the operator clicking, and none by a
//! test, for one reason: `AppWindow` could not be constructed outside a real `eframe`
//! run, so the entire UI surface had no coverage. `AppWindow::new_for_context` and
//! `AppWindow::tick` removed that dependency; this drives them.
//!
//! **It runs the real code path.** `tick()` is the whole body of `eframe::App::update`,
//! so a test here exercises the same polling, the same modal, and the same widget code
//! the operator does — no window, no GPU, no compositor.
//!
//! Widgets are located by the text egui actually painted, read out of the tessellated
//! output. That is deliberately end-of-pipeline: it can only find a label if it really
//! got drawn, so a control hidden behind a layout bug fails the test rather than passing
//! on the strength of the state behind it.

use egui::Pos2;

use crate::{AppWindow, BsrConfig};

pub struct UiHarness {
    pub ctx: egui::Context,
    pub app: AppWindow,
    /// Text drawn in the last frame, with where it was drawn.
    texts: Vec<(String, egui::Rect)>,
    /// Viewport commands issued in the last frame — minimise, focus, and so on.
    pub commands: Vec<egui::ViewportCommand>,
    queued: Vec<egui::Event>,
    screen: egui::Rect,
    _rt: tokio::runtime::Runtime,
}

impl UiHarness {
    pub fn new() -> Self {
        // A real runtime: the app spawns onto this handle, and a test that silently had
        // no runtime would pass while every spawned path did nothing.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let ctx = egui::Context::default();
        let app = AppWindow::new_headless(&ctx, BsrConfig::default(), rt.handle().clone(), None);
        Self {
            ctx,
            app,
            texts: Vec::new(),
            commands: Vec::new(),
            queued: Vec::new(),
            screen: egui::Rect::from_min_size(Pos2::ZERO, egui::vec2(1000.0, 900.0)),
            _rt: rt,
        }
    }

    /// Render one frame, collecting the text that was painted.
    pub fn frame(&mut self) {
        let mut input = egui::RawInput {
            screen_rect: Some(self.screen),
            ..Default::default()
        };
        input.events = std::mem::take(&mut self.queued);

        let app = &mut self.app;
        let output = self.ctx.run(input, |ctx| app.tick(ctx));

        // Deliberately NOT tessellated: tessellation turns text into a mesh and the
        // string is gone. `output.shapes` still carries the galley, which is what makes
        // a widget findable by its label.
        self.commands.clear();
        for (_, vp) in &output.viewport_output {
            self.commands.extend(vp.commands.iter().cloned());
        }

        self.texts.clear();
        for clipped in &output.shapes {
            collect_text(&clipped.shape, &mut self.texts);
        }
    }

    /// Replace the save-location picker with one that answers immediately.
    ///
    /// Lets the whole browse round trip be driven with no desktop and no portal: the
    /// request, the reply, and everything the UI does with it.
    pub fn set_picker(&mut self, reply: crate::PickerResult) {
        let reply = std::sync::Arc::new(std::sync::Mutex::new(Some(reply)));
        self.app.picker = crate::LocationPicker::new(move |_name, _dir, tx| {
            if let Some(r) = reply.lock().unwrap().take() {
                let _ = tx.send(r);
            }
        });
    }

    /// A picker that accepts the request and then never answers, like a portal whose
    /// dialog never appears.
    pub fn set_silent_picker(&mut self) {
        self.app.picker = crate::LocationPicker::new(|_name, _dir, tx| {
            // Deliberately hold the sender so the channel stays open rather than
            // closing: a closed channel is a different failure from silence.
            std::mem::forget(tx);
        });
    }

    /// Click a widget to focus it, then replace its contents by typing.
    ///
    /// Select-all then type, which is what a person does. Anything less tests a code
    /// path nobody uses.
    pub fn replace_text(&mut self, locate: &str, new_text: &str) {
        self.click(locate);
        self.queued.push(egui::Event::Key {
            key: egui::Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        self.queued.push(egui::Event::Text(new_text.to_string()));
        self.frame();
    }

    /// Run several frames, for state that settles over more than one.
    pub fn frames(&mut self, n: usize) {
        for _ in 0..n {
            self.frame();
        }
    }

    /// Every piece of text painted in the last frame.
    pub fn visible_text(&self) -> Vec<String> {
        self.texts.iter().map(|(t, _)| t.clone()).collect()
    }

    /// Whether any painted text contains `needle`.
    pub fn shows(&self, needle: &str) -> bool {
        self.texts.iter().any(|(t, _)| t.contains(needle))
    }

    /// Where a piece of painted text is, if it was painted at all.
    ///
    /// **Exact matches win.** A substring search alone is a trap: "Save a copy of your
    /// recording." contains "Save", so `click("Save")` hit the modal's heading instead of
    /// its button and the test failed for a reason that had nothing to do with the app.
    /// A harness that clicks the wrong thing produces false bug reports, which is worse
    /// than no harness.
    pub fn find(&self, needle: &str) -> Option<egui::Rect> {
        if let Some((_, r)) = self.texts.iter().find(|(t, _)| t.trim() == needle) {
            return Some(*r);
        }
        self.texts.iter().find(|(t, _)| t.contains(needle)).map(|(_, r)| *r)
    }

    /// How many painted strings contain `needle`. Use it to assert a locator is
    /// unambiguous before relying on it.
    pub fn matches(&self, needle: &str) -> usize {
        self.texts.iter().filter(|(t, _)| t.contains(needle)).count()
    }

    /// Click the widget whose label contains `needle`. Panics if it was never painted,
    /// because "the button is not on screen" and "the button did nothing" are different
    /// failures and must not be confused.
    pub fn click(&mut self, needle: &str) {
        let rect = self.find(needle).unwrap_or_else(|| {
            panic!(
                "no widget labelled {needle:?} was painted. Visible text was: {:#?}",
                self.visible_text()
            )
        });
        let pos = rect.center();
        self.queued.push(egui::Event::PointerMoved(pos));
        self.queued.push(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        });
        self.queued.push(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        });
        self.frame();
    }
}

fn collect_text(shape: &egui::epaint::Shape, out: &mut Vec<(String, egui::Rect)>) {
    use egui::epaint::Shape;
    match shape {
        Shape::Text(t) => out.push((t.galley.text().to_owned(), t.visual_bounding_rect())),
        Shape::Vec(v) => v.iter().for_each(|s| collect_text(s, out)),
        _ => {}
    }
}

#[cfg(test)]
mod ui_tests {
    use super::UiHarness;

    /// The harness must actually be rendering the app, or every test below is vacuous.
    #[test]
    fn the_app_renders_its_main_controls() {
        let mut h = UiHarness::new();
        h.frame();
        assert!(h.shows("Record space"), "visible text was {:#?}", h.visible_text());
        assert!(h.shows("Telemetry"));
        assert!(h.shows("Diagnostics"));
        assert!(h.shows("Set top-left"), "the corner controls must be on screen");
    }

    /// The save modal is what four hand-found defects lived in.
    #[test]
    fn the_save_modal_appears_with_a_folder_a_name_and_a_browse_button() {
        let mut h = UiHarness::new();
        h.frame();
        assert!(!h.shows("Save a copy"), "no modal until a recording is finished");

        h.app.pending_local_save = Some("take.mp4".to_string());
        // An egui Window needs a frame to settle its layout before it paints anything.
        h.frames(3);
        assert!(h.shows("Save a copy"), "the modal should be up; painted: {:#?}", h.visible_text());
        assert!(h.shows("Folder:"), "it must offer a folder, not just a name");
        assert!(h.shows("Filename:"));
        assert!(h.shows("Browse"), "the location picker must be reachable");
    }

    /// **The per-frame reset.** Clearing the folder used to snap straight back to the
    /// default on the next frame, so a path could never be pasted in. This is the exact
    /// bug, expressed as the operator hit it: empty the field, let time pass, look again.
    #[test]
    fn clearing_the_folder_does_not_snap_back_to_the_default() {
        let mut h = UiHarness::new();
        h.app.pending_local_save = Some("take.mp4".to_string());
        // An egui Window needs a frame to settle its layout before it paints anything.
        h.frames(3);

        h.app.save_dest_folder.clear();
        h.frames(5);
        assert_eq!(
            h.app.save_dest_folder, "",
            "the folder box refilled itself; it cannot be cleared to paste a path into"
        );
    }

    /// And a folder the operator sets must survive, which is the other half of the same
    /// bug: the modal used to write back values captured at the start of the frame.
    #[test]
    fn an_edited_folder_survives_across_frames() {
        let mut h = UiHarness::new();
        h.app.pending_local_save = Some("take.mp4".to_string());
        // An egui Window needs a frame to settle its layout before it paints anything.
        h.frames(3);

        h.app.save_dest_folder = "/var/tmp".to_string();
        h.frames(5);
        assert_eq!(h.app.save_dest_folder, "/var/tmp", "an edited folder must persist");
    }

    /// **The click that "did nothing".** Clicking Browse must at minimum reach the
    /// handler; the handler sets a status synchronously before any portal is contacted.
    /// This separates "the button is not wired" from "the portal did not answer", which
    /// took two rounds of guessing to distinguish by hand.
    #[test]
    fn clicking_browse_reaches_the_handler() {
        let mut h = UiHarness::new();
        h.app.pending_local_save = Some("take.mp4".to_string());
        // An egui Window needs a frame to settle its layout before it paints anything.
        h.frames(3);
        assert!(h.app.save_status.is_none(), "nothing attempted yet");

        h.click("Browse");
        assert!(
            h.app.save_status.is_some(),
            "clicking Browse did not reach browse_for_save_location — the button is not wired"
        );
        assert!(h.shows("A save dialog has opened"), "and it must say so in the modal");
    }

    /// Feedback must be inside the modal, because the modal covers the Diagnostics panel.
    /// Posting only to Diagnostics is writing to the one place that cannot be read.
    #[test]
    fn save_status_is_shown_inside_the_modal() {
        let mut h = UiHarness::new();
        h.app.pending_local_save = Some("take.mp4".to_string());
        h.app.save_status = Some("Save dialog failed: no portal".to_string());
        h.frames(3);
        assert!(
            h.shows("Save dialog failed"),
            "the status must be painted in the modal, not only in Diagnostics"
        );
    }

    /// Cancel closes the modal without writing anything.
    #[test]
    fn cancel_closes_the_modal() {
        let mut h = UiHarness::new();
        h.app.pending_local_save = Some("take.mp4".to_string());
        // An egui Window needs a frame to settle its layout before it paints anything.
        h.frames(3);
        h.click("Cancel");
        assert!(h.app.pending_local_save.is_none(), "Cancel must dismiss the modal");
        h.frame();
        assert!(!h.shows("Save a copy"), "and it must stop being painted");
    }

    /// A corner pick arms on click and disarms on Esc — the paths that used to open a
    /// fullscreen overlay and wedge the whole render loop.
    #[test]
    fn arming_a_corner_is_reflected_in_the_ui() {
        let mut h = UiHarness::new();
        h.frame();
        assert!(h.app.pending_pick.is_none());

        h.click("Set top-left");
        assert_eq!(h.app.pending_pick, Some(crate::Corner::TopLeft), "the button must arm the pick");
        assert!(h.shows("Cancel pick"), "and the way out must be visible");

        h.click("Cancel pick");
        assert!(h.app.pending_pick.is_none(), "Cancel pick must disarm");
    }

    /// The record-space readout must follow the settings, since it is the only feedback
    /// telling the operator what will actually be recorded.
    #[test]
    fn the_record_space_readout_follows_the_crop() {
        let mut h = UiHarness::new();
        h.frame();
        assert!(h.shows("Recording the full screen"), "default is the whole screen");

        h.app.model.settings.crop_left = 200;
        h.app.model.settings.crop_right = 200;
        h.frame();
        assert!(
            h.shows("1520x1080"),
            "the readout must show the cropped size; visible text was {:#?}",
            h.visible_text()
        );
    }
}

#[cfg(test)]
mod picker_tests {
    use super::UiHarness;

    fn open_modal(h: &mut UiHarness) {
        h.app.pending_local_save = Some("take.mp4".to_string());
        h.frames(3);
    }

    /// The full round trip: click Browse, the picker answers with a path, and the modal
    /// adopts both halves of it. None of this had coverage before.
    #[test]
    fn a_chosen_location_updates_the_folder_and_the_filename() {
        let mut h = UiHarness::new();
        h.set_picker(Ok(Some(std::path::PathBuf::from("/var/tmp/clips/final take.mp4"))));
        open_modal(&mut h);

        h.click("Browse");
        h.frames(3);

        assert_eq!(h.app.save_dest_folder, "/var/tmp/clips", "the folder must follow the choice");
        assert_eq!(h.app.pending_local_save.as_deref(), Some("final take.mp4"), "and so must the name");
        assert!(h.app.save_status.is_none(), "a successful pick leaves no warning behind");
        assert!(h.shows("final take.mp4"), "the modal must show the chosen name");
    }

    /// Cancelling must say so and change nothing.
    #[test]
    fn a_cancelled_dialog_changes_nothing_and_says_so() {
        let mut h = UiHarness::new();
        h.set_picker(Ok(None));
        open_modal(&mut h);
        let folder_before = h.app.save_dest_folder.clone();

        h.click("Browse");
        h.frames(3);

        assert_eq!(h.app.save_dest_folder, folder_before, "cancel must not move the folder");
        assert!(h.shows("cancelled"), "and the modal must say it was cancelled");
    }

    /// A failure must surface in the modal, not vanish into a filtered log line.
    #[test]
    fn a_failed_dialog_is_reported_in_the_modal() {
        let mut h = UiHarness::new();
        h.set_picker(Err("could not open the save dialog: no portal".to_string()));
        open_modal(&mut h);

        h.click("Browse");
        h.frames(3);

        assert!(
            h.shows("no portal"),
            "the reason must be visible where the operator is looking; painted: {:#?}",
            h.visible_text()
        );
    }

    /// **The exact shape of "Browse does nothing".** A picker that accepts the request
    /// and never answers must not leave the operator waiting forever with no explanation.
    #[test]
    fn a_dialog_that_never_answers_eventually_explains_itself() {
        let mut h = UiHarness::new();
        h.set_silent_picker();
        open_modal(&mut h);

        h.click("Browse");
        h.frames(3);
        assert!(h.shows("A save dialog has opened"), "it should say a dialog is up");

        // Bring the deadline forward rather than sleeping 45s in a test.
        h.app.pick_deadline = Some(std::time::Instant::now());
        h.frames(3);

        assert!(
            h.shows("Gave up waiting"),
            "a silent dialog must time out and say so; painted: {:#?}",
            h.visible_text()
        );
        assert!(h.app.save_pick_rx.is_none(), "and the in-flight pick must be cleared");
    }
}

#[cfg(test)]
mod typing_tests {
    use super::UiHarness;

    /// **Typing a folder must actually take.** The operator typed a real, existing
    /// directory and the modal kept showing the old destination — so this drives the
    /// widget the way a person does rather than setting the backing field directly,
    /// which is the difference between testing the UI and testing around it.
    #[test]
    fn typing_a_folder_changes_the_destination() {
        let mut h = UiHarness::new();
        h.app.pending_local_save = Some("take.mp4".to_string());
        h.app.save_dest_folder = "/home/somebody/Videos".to_string();
        h.frames(3);
        assert!(h.shows("/home/somebody/Videos"), "the starting folder should be on screen");

        h.replace_text("/home/somebody/Videos", "/var/tmp");
        h.frames(3);

        assert_eq!(h.app.save_dest_folder, "/var/tmp", "typing must reach the setting");
        assert!(
            h.shows("/var/tmp/take.mp4"),
            "the destination preview must follow what was typed; painted: {:#?}",
            h.visible_text()
        );
    }
}

#[cfg(test)]
mod save_end_to_end {
    use super::UiHarness;

    /// The operator's exact sequence: a finished recording, open the save modal, type a
    /// real folder, press Save — and the copy must land there.
    ///
    /// Every part of this was previously only reachable by hand, which is why four
    /// defects in this modal reached the operator before any test.
    #[test]
    fn typing_a_folder_and_pressing_save_writes_the_copy_there() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src_dir = tmp.path().join("Videos").join("BSR");
        std::fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join("recording-2026-09-06.mp4");
        std::fs::write(&src, b"pretend this is an mp4 with real bytes").unwrap();

        let dest = tmp.path().join("nlp");
        std::fs::create_dir_all(&dest).unwrap();

        let mut h = UiHarness::new();
        h.app.current_recording_path = Some(src.clone());
        h.app.pending_local_save = Some("recording-2026-09-06.mp4".to_string());
        h.app.save_dest_folder = tmp.path().join("Videos").to_string_lossy().into_owned();
        h.frames(3);

        // Type the destination the way a person does.
        let start = tmp.path().join("Videos").to_string_lossy().into_owned();
        h.replace_text(&start, &dest.to_string_lossy());
        h.frames(3);
        assert_eq!(
            h.app.save_dest_folder,
            dest.to_string_lossy(),
            "the typed folder must reach the setting"
        );

        h.click("Save");
        h.frames(2);

        let written = dest.join("recording-2026-09-06.mp4");
        assert!(
            written.is_file(),
            "Save did not write the copy. Modal painted: {:#?}",
            h.visible_text()
        );
        assert_eq!(
            std::fs::read(&written).unwrap(),
            std::fs::read(&src).unwrap(),
            "the copy must be byte-identical to the recording"
        );
        assert!(src.is_file(), "and the original must still be there");
        assert!(h.app.pending_local_save.is_none(), "the modal should close after saving");
    }

    /// Saving into the recording's own folder under its own name must stay refused —
    /// that is the case that used to truncate the take to zero bytes.
    #[test]
    fn saving_onto_the_recording_itself_is_still_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("take.mp4");
        std::fs::write(&src, b"the only copy of something irreplaceable").unwrap();

        let mut h = UiHarness::new();
        h.app.current_recording_path = Some(src.clone());
        h.app.pending_local_save = Some("take.mp4".to_string());
        h.app.save_dest_folder = tmp.path().to_string_lossy().into_owned();
        h.frames(3);

        assert!(h.shows("That is the recording itself"), "it must warn");
        h.click("Save");
        h.frames(2);

        assert_eq!(
            std::fs::read(&src).unwrap(),
            b"the only copy of something irreplaceable",
            "the recording must be untouched — fs::copy onto itself truncates to zero"
        );
    }
}

#[cfg(test)]
mod disabled_reason_tests {
    use super::UiHarness;

    /// A greyed-out Save with no explanation is indistinguishable from a broken one.
    /// Every reason it can be unavailable must name itself.
    #[test]
    fn a_disabled_save_always_says_why() {
        // No recording to copy at all.
        let mut h = UiHarness::new();
        h.app.pending_local_save = Some("take.mp4".to_string());
        h.app.current_recording_path = None;
        h.frames(3);
        assert!(h.shows("no recording to copy"), "painted: {:#?}", h.visible_text());

        // A folder that is not there.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("take.mp4");
        std::fs::write(&src, b"x").unwrap();
        let mut h = UiHarness::new();
        h.app.current_recording_path = Some(src.clone());
        h.app.pending_local_save = Some("take.mp4".to_string());
        h.app.save_dest_folder = tmp.path().join("nowhere").to_string_lossy().into_owned();
        h.frames(3);
        assert!(h.shows("that folder does not exist"));

        // The recording itself.
        let mut h = UiHarness::new();
        h.app.current_recording_path = Some(src.clone());
        h.app.pending_local_save = Some("take.mp4".to_string());
        h.app.save_dest_folder = tmp.path().to_string_lossy().into_owned();
        h.frames(3);
        assert!(h.shows("that is the recording itself"));

        // An empty name.
        let mut h = UiHarness::new();
        h.app.current_recording_path = Some(src);
        h.app.pending_local_save = Some("   ".to_string());
        h.app.save_dest_folder = tmp.path().to_string_lossy().into_owned();
        h.frames(3);
        assert!(h.shows("enter a filename"));
    }

    /// And when nothing is wrong, no scolding.
    #[test]
    fn a_valid_destination_shows_no_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("take.mp4");
        std::fs::write(&src, b"x").unwrap();
        let dest = tmp.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();

        let mut h = UiHarness::new();
        h.app.current_recording_path = Some(src);
        h.app.pending_local_save = Some("take.mp4".to_string());
        h.app.save_dest_folder = dest.to_string_lossy().into_owned();
        h.frames(3);
        assert!(!h.shows("Can't save"), "painted: {:#?}", h.visible_text());
    }
}

#[cfg(test)]
mod window_tests {
    use super::UiHarness;

    fn minimised(h: &UiHarness) -> Option<bool> {
        h.commands.iter().find_map(|c| match c {
            egui::ViewportCommand::Minimized(v) => Some(*v),
            _ => None,
        })
    }

    /// The window must get out of its own shot when recording starts, and come back when
    /// the recording is finalised — otherwise the operator is left staring at a desktop
    /// with no obvious way to see the result.
    #[test]
    fn recording_minimises_the_window_and_finishing_restores_it() {
        let mut h = UiHarness::new();
        h.frame();
        assert_eq!(minimised(&h), None, "nothing asked for at rest");

        h.app.model.settings.minimize_while_recording = true;
        h.app.pending_window_cmd = Some(true);
        h.frame();
        assert_eq!(minimised(&h), Some(true), "recording must minimise the window");

        h.app.pending_window_cmd = Some(false);
        h.frame();
        assert_eq!(minimised(&h), Some(false), "finishing must restore it");
        assert!(
            h.commands.iter().any(|c| matches!(c, egui::ViewportCommand::Focus)),
            "and raise it, or it comes back behind everything"
        );
    }

    /// The window must come to the front on launch, or it opens behind whatever is
    /// maximised and — with no dock entry — cannot be recovered at all.
    #[test]
    fn the_window_raises_itself_on_launch() {
        let mut h = UiHarness::new();
        h.frame();
        assert!(
            h.commands.iter().any(|c| matches!(c, egui::ViewportCommand::Focus)),
            "the first frame must ask to be raised"
        );
        h.frame();
        assert!(
            !h.commands.iter().any(|c| matches!(c, egui::ViewportCommand::Focus)),
            "but only once — repeating it would steal focus from the operator's work"
        );
    }

    /// The request is one-shot. Re-sending minimise every frame would fight the operator
    /// if they restored the window by hand mid-recording.
    #[test]
    fn the_window_request_is_not_repeated() {
        let mut h = UiHarness::new();
        h.app.pending_window_cmd = Some(true);
        h.frame();
        assert_eq!(minimised(&h), Some(true));
        h.frame();
        assert_eq!(minimised(&h), None, "the request must not repeat");
    }

    /// The tray is the only reliable way back to a minimised window on this box, so
    /// "Open" must actually restore and raise it. It was an empty match arm with a
    /// comment claiming eframe did it — eframe does not.
    #[test]
    fn restoring_the_window_unminimises_and_raises_it() {
        let mut h = UiHarness::new();
        h.frame();
        h.app.pending_window_cmd = Some(false);
        h.frame();
        assert_eq!(minimised(&h), Some(false), "must un-minimise");
        assert!(
            h.commands.iter().any(|c| matches!(c, egui::ViewportCommand::Focus)),
            "and raise, or it comes back behind whatever is maximised"
        );
    }

    /// And it is a choice, not a decree.
    #[test]
    fn hiding_while_recording_can_be_turned_off() {
        let mut h = UiHarness::new();
        h.frame();
        assert!(h.shows("Hide while recording"), "the toggle must be reachable");
        assert!(
            h.app.model.settings.minimize_while_recording,
            "on by default, since being in your own recording is never wanted"
        );
    }
}

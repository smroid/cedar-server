// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{sleep, spawn, JoinHandle},
    time::Duration,
};

// Our state, shared between ActivityLed methods and the worker thread.
pub struct ActivityLed {
    state: Arc<Mutex<SharedState>>,
    worker_thread: Option<JoinHandle<()>>,  // Executes worker().
}

// Blink cadence for the Ready state (before the first RPC). Has no effect once
// received_rpc() has been called, when the LED is held off.
#[derive(Clone, Copy, PartialEq)]
pub enum BlinkPattern {
    // 1hz, 50% duty cycle (500ms on, 500ms off).
    Regular,

    // 4hz, 20% duty cycle (50ms on, 200ms off).
    Fast,
}

impl BlinkPattern {
    // (on_ticks, period_ticks) at the worker's tick resolution (25ms).
    fn timing(self) -> (u32, u32) {
        match self {
            BlinkPattern::Regular => (20, 40), // 500ms on / 1000ms period
            BlinkPattern::Fast => (2, 10),     // 50ms on / 250ms period
        }
    }
}

// State shared between worker thread and the ActivityLed methods.
struct SharedState {
    // Set by stop(); the worker thread exits when it sees this.
    stop_request: bool,

    // Set by received_rpc().
    received_rpc: bool,

    // Ready-state blink cadence; changed by set_blink_pattern().
    blink_pattern: BlinkPattern,
}

// The ActivityLed controls the state of the board's activity LED (see
// LED_NAMES). By default the OS drives this LED from a trigger, e.g. "disk"
// activity on the Raspberry Pi.
//
// When ActivityLed is constructed, it takes over the activity LED and manages
// it in three states:
//
// * Ready: the LED is blinked on and off. This occurs when ActivityLed has been
//   created but received_rpc() has not been called yet. The blink cadence is
//   the current BlinkPattern (Regular by default; see set_blink_pattern()).
// * Connected: the LED is turned off. This occurs when received_rpc() has been
//   called at least once.
// * Released: The LED is handed back to the trigger it had when ActivityLed
//   took it over. This occurs when the stop() method is called.

impl ActivityLed {
    // Takes over the activity LED, blinking it with the Regular pattern.
    pub fn new(got_signal: Arc<AtomicBool>) -> Self {
        let mut activity_led = ActivityLed {
            state: Arc::new(Mutex::new(SharedState {
                stop_request: false,
                received_rpc: false,
                blink_pattern: BlinkPattern::Regular,
            })),
            worker_thread: None,
        };
        let cloned_state = activity_led.state.clone();
        activity_led.worker_thread = Some(spawn(move || {
            ActivityLed::worker(cloned_state, got_signal);
        }));
        activity_led
    }

    // Indicates that Cedar has received an RPC from a client. We turn the
    // activity LED off.
    pub fn received_rpc(&self) {
        self.state.lock().unwrap().received_rpc = true;
    }

    // Sets the Ready-state blink cadence. Takes effect at the start of the next
    // blink phase; has no visible effect once received_rpc() has been called.
    pub fn set_blink_pattern(&self, pattern: BlinkPattern) {
        self.state.lock().unwrap().blink_pattern = pattern;
    }

    // Resumes blinking with the most recently set BlinkPattern, undoing the
    // effect of received_rpc(). Used e.g. when Cedar wants to signal that
    // connected clients should be considered gone again.
    pub fn resume_blinking(&self) {
        self.state.lock().unwrap().received_rpc = false;
    }

    // Releases the activity LED back to the trigger it had when we took it
    // over. Blocks until the worker thread has reverted the LED; the
    // worker polls stop_request on a short tick, so this returns quickly.
    // Idempotent: a second call (or a call after Drop's request) is a no-op.
    pub fn stop(&mut self) {
        if let Some(worker_thread) = self.worker_thread.take() {
            self.state.lock().unwrap().stop_request = true;
            worker_thread.join().unwrap();
        }
    }

    fn worker(
        state: Arc<Mutex<SharedState>>,
        got_signal: Arc<AtomicBool>,
    ) {
        // Raspberry Pi 5 reverses the control signal to the ACT led.
        // On hosts without a device tree (e.g. x86 dev machines) this file is
        // absent; default to empty (non-Pi5).
        let processor_model =
            fs::read_to_string("/sys/firmware/devicetree/base/model")
                .unwrap_or_default()
                .trim_end_matches('\0')
                .to_string();
        let is_rpi5 = processor_model.contains("Raspberry Pi 5");
        let off_value = if is_rpi5 { "1" } else { "0" };
        let on_value = if is_rpi5 { "0" } else { "1" };

        // See jeffgeerling.com/blogs/jeff-geerling/
        // controlling-pwr-act-leds-raspberry-pi
        let led_dir = match find_led_dir() {
            Some(dir) => dir,
            None => {
                log::info!("No activity LED found (looked for {:?})", LED_NAMES);
                return;
            }
        };
        let brightness_path = led_dir.join("brightness");
        let trigger_path = led_dir.join("trigger");

        // Remember the LED's trigger so stop() can hand the LED back to it.
        // This must be read before the brightness write below, which clears
        // the trigger.
        let original_trigger = match current_trigger(&trigger_path) {
            Some(trigger) => trigger,
            None => "none".to_string(),
        };

        // The worker wakes on a short fixed tick so stop()/got_signal are seen
        // promptly, and drives the Ready blink by counting ticks: the LED is on
        // for the pattern's on_ticks, then off for the rest of its period_ticks.
        // TICK divides every pattern timing evenly (see BlinkPattern::timing).
        let tick = Duration::from_millis(25);

        #[derive(PartialEq)]
        enum LedState {
            Ready,
            ConnectedOff,
        }
        let mut led_state = LedState::Ready;
        // Ticks elapsed in the current blink period, and whether the LED is
        // currently lit, so we only write brightness on a transition.
        let mut phase_ticks: u32 = 0;
        let mut led_on = false;
        if let Err(e) = fs::write(&brightness_path, off_value) {
            log::warn!("Error writing to LED: {:?}", e);
        }
        loop {
            sleep(tick);
            let (stop_request, received_rpc, blink_pattern) = {
                let locked_state = state.lock().unwrap();
                (
                    locked_state.stop_request,
                    locked_state.received_rpc,
                    locked_state.blink_pattern,
                )
            };
            if stop_request {
                break;
            }
            if got_signal.load(Ordering::Relaxed) {
                break;
            }
            let want_connected_off = received_rpc;
            if led_state != LedState::ConnectedOff && want_connected_off {
                fs::write(&brightness_path, off_value).unwrap_or(());
                led_state = LedState::ConnectedOff;
            } else if led_state == LedState::ConnectedOff && !want_connected_off
            {
                // resume_blinking() was called: restart the blink phase.
                led_state = LedState::Ready;
                phase_ticks = 0;
                led_on = false;
            }
            if led_state == LedState::ConnectedOff {
                continue;
            }
            // Ready: advance the blink phase and write brightness on a change.
            let (on_ticks, period_ticks) = blink_pattern.timing();
            phase_ticks = (phase_ticks + 1) % period_ticks;
            let want_on = phase_ticks < on_ticks;
            if want_on != led_on {
                let value = if want_on { on_value } else { off_value };
                fs::write(&brightness_path, value).unwrap_or(());
                led_on = want_on;
            }
        }
        // Hand the LED back to the trigger it had before we took it over.
        fs::write(&trigger_path, &original_trigger).unwrap_or(());
    }
}

// Names of the activity LED under /sys/class/leds on the boards we run on.
const LED_NAMES: [&str; 2] = ["ACT", "board-led"];

// The /sys/class/leds directory of the first of LED_NAMES that exists.
fn find_led_dir() -> Option<PathBuf> {
    for name in LED_NAMES {
        let dir = Path::new("/sys/class/leds").join(name);
        if dir.exists() {
            return Some(dir);
        }
    }
    None
}

// The LED's active trigger, read from its trigger file.
fn current_trigger(trigger_path: &Path) -> Option<String> {
    let contents = fs::read_to_string(trigger_path).ok()?;
    active_trigger(&contents)
}

// The active trigger in a trigger file's contents: the entry in brackets,
// e.g. "mmc0" in "none default-on [mmc0] mmc1".
fn active_trigger(contents: &str) -> Option<String> {
    for entry in contents.split_whitespace() {
        if entry.starts_with('[') && entry.ends_with(']') {
            return Some(entry.trim_start_matches('[').trim_end_matches(']').to_string());
        }
    }
    None
}

impl Drop for ActivityLed {
    // Ensures the worker thread stops and reverts the LED even if stop()
    // was never called (early return, panic unwind, etc.).
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_trigger_is_the_bracketed_entry() {
        assert_eq!(active_trigger("none default-on [mmc0] mmc1::\n"),
                   Some("mmc0".to_string()));
        assert_eq!(active_trigger("[none] timer heartbeat"), Some("none".to_string()));
        assert_eq!(active_trigger("none timer [heartbeat]"),
                   Some("heartbeat".to_string()));
    }

    #[test]
    fn no_active_trigger() {
        assert_eq!(active_trigger(""), None);
        assert_eq!(active_trigger("none timer heartbeat"), None);
    }
}

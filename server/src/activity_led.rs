// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::fs;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{JoinHandle, sleep, spawn};
use std::time::Duration;

pub struct ActivityLed {
    // Our state, shared between ActivityLed methods and the worker thread.
    state: Arc<Mutex<SharedState>>,

    // Executes worker().
    worker_thread: Option<JoinHandle<()>>,
}

// State shared between worker thread and the ActivityLed methods.
struct SharedState {
    // Set by stop(); the worker thread exits when it sees this.
    stop_request: bool,

    // Set by received_rpc().
    received_rpc: bool,
}

// The ActivityLed controls the state of the Raspberry Pi activity LED. By
// default, this LED is configured by the Rpi to indicated system "disk"
// activity.
//
// When ActivityLed is constructed, it takes over the Raspberry Pi activity LED
// and manages it in three states:
//
// * Ready: the LED is blinked on and off at 1hz. This occurs when ActivityLed
//   has been created but received_rpc() has not not been called yet.
// * Connected: the LED is turned off. This occurs when received_rpc() has been
//   called at least once.
// * Released: The LED is re-configured back to the Raspberry Pi default, where
//   it indicates "disk" activity. This occurs when the stop() method is called.

impl ActivityLed {
    // Initiates the activity LED to blinking at 1hz.
    pub fn new(got_signal: Arc<AtomicBool>) -> Self {
        let mut activity_led = ActivityLed{
            state: Arc::new(Mutex::new(
                SharedState{
                    stop_request: false,
                    received_rpc: false,
                })),
            worker_thread: None,
        };
        let cloned_state = activity_led.state.clone();
        let cloned_got_signal = got_signal.clone();
        activity_led.worker_thread =
            Some(spawn(|| {
                ActivityLed::worker(cloned_state, cloned_got_signal);
            }));
        activity_led
    }

    // Indicates that Cedar has received an RPC from a client. We turn the
    // activity LED off.
    pub fn received_rpc(&self) {
        self.state.lock().unwrap().received_rpc = true;
    }

    // Releases the activity LED back to its OS-defined "disk" activity
    // indicator.
    pub fn stop(&mut self) {
        self.state.lock().unwrap().stop_request = true;
        self.worker_thread.take().unwrap().join().unwrap();
    }

    fn worker(state: Arc<Mutex<SharedState>>, got_signal: Arc<AtomicBool>) {
	// Raspberry Pi 5 reverses the control signal to the ACT led.
        let processor_model =
            fs::read_to_string("/sys/firmware/devicetree/base/model").unwrap()
            .trim_end_matches('\0').to_string();
	let is_rpi5 = processor_model.contains("Raspberry Pi 5");
	let off_value = if is_rpi5 { "1" } else { "0" };
	let on_value = if is_rpi5 { "0" } else { "1" };

        // https://www.jeffgeerling.com/blogs/jeff-geerling/controlling-pwr-act-leds-raspberry-pi
        let brightness_path = "/sys/class/leds/ACT/brightness";
        let trigger_path = "/sys/class/leds/ACT/trigger";

        let delay = Duration::from_millis(500);

        #[derive(PartialEq)]
        enum LedState {
            ReadyOff,
            ReadyOn,
            ConnectedOff,
        }
        let mut led_state = LedState::ReadyOff;
        fs::write(brightness_path, off_value).unwrap();

        loop {
            sleep(delay);
            if state.lock().unwrap().stop_request {
                break;
            }
            if got_signal.load(Ordering::Relaxed) {
                break;
            }
            if led_state != LedState::ConnectedOff &&
                state.lock().unwrap().received_rpc
            {
		fs::write(brightness_path, off_value).unwrap();
                led_state = LedState::ConnectedOff;
                continue;
            }
            match led_state {
                LedState::ReadyOff => {
                    fs::write(brightness_path, on_value).unwrap();
                    led_state = LedState::ReadyOn;
                },
                LedState::ReadyOn => {
                    fs::write(brightness_path, off_value).unwrap();
                    led_state = LedState::ReadyOff;
                },
                LedState::ConnectedOff => {}
            };
        }
        // Revert LED back to system default state (disk activity).
        fs::write(trigger_path, "mmc0").unwrap();
    }
}

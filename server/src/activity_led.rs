// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::fs;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{JoinHandle, sleep, spawn};
use std::time::{Duration, SystemTime};

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
// * Idle: the LED is blinked on and off at 1hz. This occurs when ActivityLed
//   has been created but received_rpc() has not not been called recently.
// * Connected: the LED is turned off. This occurs when received_rpc() is being
//   called often enough.
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
    // activity LED off; if too much time occurs without received_rpc() being
    // called again, we will resume blinking the LED at 1hz.
    pub fn received_rpc(&self) {
        self.state.lock().unwrap().received_rpc = true;
    }

    // Releases the activity LED back to its OS-defined "disk" activity
    // indicator. Currently there is no way to transition out of the released
    // state after stop() is called.
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

        let blink_delay = Duration::from_millis(500);

        // How often we look for received_rpc() when we're in the ConnectedOff state.
        let connected_poll = Duration::from_millis(500);

        // How long we can go without received_rpc() before we revert to Idle
        // state.
        let connected_timeout = Duration::from_secs(5);

        let mut last_rpc_time = SystemTime::now();

        enum LedState {
            IdleOff,
            IdleOn,
            ConnectedOff,
        }
        let mut led_state = LedState::IdleOff;
        fs::write(brightness_path, off_value).unwrap();

        fn process_received_rpc(state: &Arc<Mutex<SharedState>>,
                                last_rpc_time: &mut SystemTime) -> bool {
            let mut locked_state = state.lock().unwrap();
            let received_rpc = locked_state.received_rpc;
            if received_rpc {
                *last_rpc_time = SystemTime::now();
                locked_state.received_rpc = false;
            }
            received_rpc
        }

        loop {
            if state.lock().unwrap().stop_request {
                break;
            }
            if got_signal.load(Ordering::Relaxed) {
                break;
            }
            match led_state {
                LedState::IdleOff => {
                    sleep(blink_delay);
                    if process_received_rpc(&state, &mut last_rpc_time) {
			fs::write(brightness_path, off_value).unwrap();
                        led_state = LedState::ConnectedOff;
                        continue;
                    }
                    fs::write(brightness_path, on_value).unwrap();
                    led_state = LedState::IdleOn;
                },
                LedState::IdleOn => {
                    sleep(blink_delay);
                    if process_received_rpc(&state, &mut last_rpc_time) {
			fs::write(brightness_path, off_value).unwrap();
                        led_state = LedState::ConnectedOff;
                        continue;
                    }
                    fs::write(brightness_path, off_value).unwrap();
                    led_state = LedState::IdleOff;
                },
                LedState::ConnectedOff => {
                    sleep(connected_poll);
                    if process_received_rpc(&state, &mut last_rpc_time) {
                        continue;
                    }
                    let elapsed = SystemTime::now().duration_since(last_rpc_time);
                    if let Err(_e) = elapsed {
                        // This can happen when the client sends a time update
                        // to Cedar server.
                        last_rpc_time = SystemTime::now();  // Start countdown fresh.
                    } else {
                        if *elapsed.as_ref().unwrap() > connected_timeout {
                            // Revert to Idle state.
                            fs::write(brightness_path, on_value).unwrap();
                            led_state = LedState::IdleOn;
                        }
                    }
                },
            };
        }
        // Revert LED back to system default state (disk activity).
        fs::write(trigger_path, "mmc0").unwrap();
    }
}

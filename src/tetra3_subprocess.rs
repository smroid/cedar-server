// Copyright (c) 2023 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader};
use std::process::{Command, Child, Stdio, ChildStdout, ChildStderr};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use log::{error, info, warn};

use canonical_error::{CanonicalError, failed_precondition_error};

// Manage execution of the tetra3_server plate solver as a subprocess.
// We start the subprocess, and:
// * Consume the subprocess stdout/stderr incrementally, posting each line
//   to info!/warn! logs.
// * If the subprocess unexpectedly exits, log to error! and re-start
//   the subprocess.
// * We install a ^C handler, and ensure that the subprocess is killed
//   before we exit.

pub struct Tetra3Subprocess {
    tetra3_script_path: OsString,
    tetra3_database: OsString,
    pid: Arc<Mutex<u32>>,
    stopping: Arc<Mutex<bool>>,
}

impl Drop for Tetra3Subprocess {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Tetra3Subprocess {
    fn make_child(tetra3_script_path: &OsString,
                  tetra3_database: &OsString) -> Result<Child, CanonicalError> {
        match Command::new("python")
            .arg(tetra3_script_path)
            .arg(tetra3_database)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn() {
                Err(e) => {
                    return Err(failed_precondition_error(
                        format!("Command::spawn error: {:?}", e).as_str()));
                },
                Ok(mut child) => {
                    // We've sucessfully spawned the subprocess, but we're not
                    // out of the woods yet. The subprocess might exit right
                    // away with an error.
                    thread::sleep(Duration::from_secs(1));
                    let exit_status = child.try_wait().expect(
                        "Unexpected child.try_wait() error");
                    if exit_status.is_some() {
                        let output = child.wait_with_output().expect(
                            "Unexpected child.wait_with_output() error");
                        return Err(failed_precondition_error(
                            format!("Command failed with: {:?}", output).as_str()));
                    }
                    info!("Tetra3 subprocess started");
                    Ok(child)
                }
            }
    }

    fn make_stdout_worker(stdout: ChildStdout) -> JoinHandle<()> {
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                let len = reader.read_line(&mut line)
                    .expect("reading from pipe should not fail");
                if len == 0 {
                    break;  // Reached EOF.
                }
                info!("{}", line);
            }
        })
    }
    fn make_stderr_worker(stderr: ChildStderr) -> JoinHandle<()> {
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            loop {
                let mut line = String::new();
                let len = reader.read_line(&mut line)
                    .expect("reading from pipe should not fail");
                if len == 0 {
                    break;  // Reached EOF.
                }
                warn!("{}", line);
            }
        })
    }

    fn make_wait_worker(&mut self, mut child: Child) {
        let got_signal = Arc::new(AtomicBool::new(false));
        let got_signal2 = got_signal.clone();
        ctrlc::set_handler(move || {
            info!("Got control-c");
            got_signal2.store(true, Ordering::Relaxed);
        }).unwrap();

        let tetra3_script_path = self.tetra3_script_path.clone();
        let tetra3_database = self.tetra3_database.clone();
        let pid = self.pid.clone();
        let stopping = self.stopping.clone();
        thread::spawn(move || {
            loop {
                let stdout_worker = Self::make_stdout_worker(child.stdout.take().unwrap());
                let stderr_worker = Self::make_stderr_worker(child.stderr.take().unwrap());
                let child_status;
                loop {
                    if got_signal.load(Ordering::Relaxed) {
                        info!("Killing {:?}", tetra3_script_path);
                        child.kill().unwrap();
                        info!("Exiting");
                        std::process::exit(-1);
                    }
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            child_status = status;
                            break;
                        },
                        Ok(None) => {
                            thread::sleep(Duration::from_millis(10));
                            continue;  // Wait again for signal or child exit.
                        },
                        Err(e) => panic!("Unexpected child.wait() error {:?}", e),
                    }
                }
                stdout_worker.join().unwrap();
                stderr_worker.join().unwrap();
                if *stopping.lock().unwrap() {
                    info!("Tetra3 subprocess stopped");
                    break;
                }
                error!("Tetra3 unexpectedly exited with status={:?}; will respawn",
                       child_status);
                // Re-spawn subprocess.
                child = Self::make_child(&tetra3_script_path, &tetra3_database).unwrap();
                *pid.lock().unwrap() = child.id();
            }
        });
    }

    // Assumes PYPATH is properly set up.
    pub fn new(tetra3_script_path: impl AsRef<OsStr>,
               tetra3_database: impl AsRef<OsStr>) -> Result<Self, CanonicalError> {
        let tetra3_script_path: OsString = tetra3_script_path.as_ref().to_os_string();
        let tetra3_database: OsString = tetra3_database.as_ref().to_os_string();
        let child = Self::make_child(&tetra3_script_path, &tetra3_database)?;
        let pid = child.id();
        let mut t3_subprocess = Tetra3Subprocess{
            tetra3_script_path, tetra3_database,
            pid: Arc::new(Mutex::new(pid)),
            stopping: Arc::new(Mutex::new(false)),
        };
        t3_subprocess.make_wait_worker(child);
        thread::sleep(Duration::from_secs(2));
        Ok(t3_subprocess)
    }

    // tetra3_server.py traps SIGINT and uses this to cancel the in-progress solve.
    pub fn send_interrupt_signal(&mut self) {
        self.send_signal("INT");
    }

    pub fn stop(&mut self) {
        *self.stopping.lock().unwrap() = true;
        self.send_signal("KILL");
    }

    fn send_signal(&mut self, sig: &str) {
        let pid = self.pid.lock().unwrap();
        // From https://stackoverflow.com/questions/49210815/how-do-i-send-a-signal-to-a-child-subprocess
        let mut kill = Command::new("kill")
            .args(["-s", sig, &pid.to_string()])
            .spawn().unwrap();
        kill.wait().unwrap();
    }
}

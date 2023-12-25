use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader};
use std::process::{Command, Child, Stdio, ChildStdout, ChildStderr};
use std::sync::{Arc, Mutex};
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

pub struct Tetra3Subprocess {
    tetra3_script_path: OsString,
    tetra3_database: OsString,
    state: Arc<Mutex<State>>,
    stopping: Arc<Mutex<bool>>,
}

struct State {
    child: Child,
    stdout_worker: Option<JoinHandle<()>>,
    stderr_worker: Option<JoinHandle<()>>,
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
    fn make_wait_worker(&mut self) {
        let state = self.state.clone();
        let stopping = self.stopping.clone();
        let tetra3_script_path = self.tetra3_script_path.clone();
        let tetra3_database = self.tetra3_database.clone();
        thread::spawn(move || {
            loop {
                let mut locked_state = state.lock().unwrap();
                let status = locked_state.child.wait().expect(
                    "Unexpected child.wait() error");
                locked_state.stdout_worker.take().unwrap().join().unwrap();
                locked_state.stderr_worker.take().unwrap().join().unwrap();
                if *stopping.lock().unwrap() {
                    info!("Tetra3 subprocess stopped");
                    break;
                }
                error!("Tetra3 subprocess unexpectedly exited with status={:?}; will respawn",
                       status);
                // Re-spawn subprocess.
                let state = Self::make_state(&tetra3_script_path,
                                             &tetra3_database).unwrap();
                locked_state.child = state.child;
                locked_state.stdout_worker = state.stdout_worker;
                locked_state.stderr_worker = state.stderr_worker;
            }
        });
    }

    fn make_state(tetra3_script_path: &OsString,
                  tetra3_database: &OsString) -> Result<State, CanonicalError> {
        let mut child = Self::make_child(&tetra3_script_path, &tetra3_database)?;
        let stdout_worker = Self::make_stdout_worker(child.stdout.take().unwrap());
        let stderr_worker = Self::make_stderr_worker(child.stderr.take().unwrap());
        Ok(State{child,
                 stdout_worker: Some(stdout_worker),
                 stderr_worker: Some(stderr_worker)})
    }

    // Assumes PYPATH is properly set up.
    pub fn new(tetra3_script_path: impl AsRef<OsStr>,
               tetra3_database: impl AsRef<OsStr>) -> Result<Self, CanonicalError> {
        let tetra3_script_path: OsString = tetra3_script_path.as_ref().to_os_string();
        let tetra3_database: OsString = tetra3_database.as_ref().to_os_string();
        let state = Self::make_state(&tetra3_script_path, &tetra3_database)?;
        let mut t3_subprocess = Tetra3Subprocess{
            tetra3_script_path, tetra3_database,
            state: Arc::new(Mutex::new(state)),
            stopping: Arc::new(Mutex::new(false)),
        };
        t3_subprocess.make_wait_worker();
        Ok(t3_subprocess)
    }

    pub fn stop(&mut self) {
        *self.stopping.lock().unwrap() = true;
        let id = self.state.lock().unwrap().child.id();
        // From https://stackoverflow.com/questions/49210815/how-do-i-send-a-signal-to-a-child-subprocess
        let mut kill = Command::new("kill")
            .args(["-s", "TERM", &id.to_string()])
            .spawn().unwrap();
        kill.wait().unwrap();
    }
}

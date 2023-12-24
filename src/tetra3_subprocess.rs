use std::ffi::OsStr;
use std::io::{BufRead, BufReader};
use std::process::{Command, Child, Stdio, ChildStdout, ChildStderr};
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
use log::{error, info, warn};

use canonical_error::{CanonicalError, failed_precondition_error};

pub struct Tetra3Subprocess {
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
    fn make_child(tetra3_database: impl AsRef<OsStr>)
                  -> Result<Child, CanonicalError> {
        match Command::new("python")
            .arg("tetra3_server")
            .arg(tetra3_database)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn() {
                Err(e) => {
                    return Err(failed_precondition_error(
                        format!("Command::spawn error: {:?}", e).as_str()));
                },
                Ok(child) => {
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
                error!("Tetra3 subprocess unexpectedly exited with status={:?}",
                       status);
                // TODO: re-spawn subprocess
            }
        });
    }

    pub fn new(tetra3_database: impl AsRef<OsStr>) -> Result<Self, CanonicalError> {
        let mut child = Self::make_child(tetra3_database)?;
        let stdout_worker = Self::make_stdout_worker(child.stdout.take().unwrap());
        let stderr_worker = Self::make_stderr_worker(child.stderr.take().unwrap());
        let state = State{
            child,
            stdout_worker: Some(stdout_worker),
            stderr_worker: Some(stderr_worker),
        };
        let mut t3_subprocess = Tetra3Subprocess{
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

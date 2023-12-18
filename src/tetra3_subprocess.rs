use std::ffi::OsStr;
use std::process::{Command, Child, Stdio, ChildStdout, ChildStderr};

use canonical_error::{CanonicalError,
                      failed_precondition_error, invalid_argument_error,
                      internal_error};

pub struct Tetra3Subprocess {
    child: Child,
    stdout: ChildStdout,
    stderr: ChildStderr,
}

impl Drop for Tetra3Subprocess {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Tetra3Subprocess {
    pub fn new(tetra3_database: impl AsRef<OsStr>) -> Result<Self, CanonicalError> {
        let mut child = match Command::new("python")
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
                Ok(c) => c
            };
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        // Things to do in a worker thread(s):
        // each subprocess stdout line -> info!
        // each subprocess stderr line -> warn!
        // subprocess crash -> error!, and re-spawn the subprocess

        Ok(Tetra3Subprocess{child, stdout, stderr})
    }

    pub fn stop(&mut self) {
        // TODO(smr): terminate the subprocess.
    }
}

// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

// Name shown for a pooled thread that is not currently running named work.
const IDLE_NAME: &str = "idle";

/// Names the calling thread for as long as this guard lives, restoring the
/// name to "idle" when dropped. The name is what shows in
/// /proc/<pid>/task/<tid>/comm, and thus in the GetCpuUsageReport RPC.
///
/// Tokio's `thread_name` setting applies to its async worker threads and its
/// spawn_blocking pool alike, so by default blocking work is indistinguishable
/// from async work in a thread listing. Hold one of these at the top of a
/// spawn_blocking closure to label what the thread is actually doing:
///
/// ```ignore
/// tokio::task::spawn_blocking(move || {
///     let _name = ThreadName::new("jpeg-encode");
///     encode(&image)
/// })
/// ```
///
/// Blocking threads are pooled and reused, so without the reset on drop a
/// finished thread would keep advertising work it is no longer doing.
#[must_use = "the thread is renamed only while the guard is alive"]
pub struct ThreadName {
    // Not Send: the guard must be dropped on the thread it renamed, since
    // prctl(PR_SET_NAME) acts on the calling thread.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl ThreadName {
    /// Renames the calling thread to `name`. Linux limits the name to 15
    /// bytes plus a NUL, so longer names are truncated.
    pub fn new(name: &str) -> Self {
        set_thread_name(name);
        ThreadName {
            _not_send: std::marker::PhantomData,
        }
    }
}

impl Drop for ThreadName {
    fn drop(&mut self) {
        set_thread_name(IDLE_NAME);
    }
}

fn set_thread_name(name: &str) {
    const MAX_LEN: usize = 15;
    // Truncate on a char boundary so a multi-byte name can't be split.
    let mut end = name.len().min(MAX_LEN);
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    let mut buf = [0_u8; MAX_LEN + 1];
    buf[..end].copy_from_slice(&name.as_bytes()[..end]);
    // Safety: buf is NUL-terminated (zero-initialized, and we write at most
    // MAX_LEN of its MAX_LEN+1 bytes), and PR_SET_NAME only reads from it.
    unsafe {
        libc::prctl(libc::PR_SET_NAME, buf.as_ptr() as libc::c_ulong, 0, 0, 0);
    }
}

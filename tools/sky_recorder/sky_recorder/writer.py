"""Writes images and manifest.csv rows for one recording session.

Images are encoded on a small thread pool so fetching is never blocked by PNG
compression. Manifest rows are written by one thread, in fetch order, once each
frame's plate solution is resolved, so row order is capture order (the
simulator replays manifest order).
"""

import io
import logging
import os
import queue
import threading
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from typing import Optional

from PIL import Image as PILImage

from . import solutions

log = logging.getLogger(__name__)

# The first 18 columns match the server-side CEDAR_BENCH_* sampler's
# manifest, so existing corpora and tooling (simulator, stacking) read both.
COLUMNS = [
    "filename", "frame_id", "solution_id", "exposure_ms", "width", "height",
    "solved", "from_imu", "ra_deg", "dec_deg", "roll_deg", "fov_deg",
    "rmse_arcsec", "num_matches", "solve_ms", "readout_time_iso",
    "burst_id", "burst_index",
    # Added by sky_recorder.
    "camera_gain", "frame_gap", "solve_status",
]


@dataclass
class Pending:
    seq: int
    image: object  # client.Image
    burst_id: int
    burst_index: int
    frame_gap: Optional[int]  # frame_id delta to previous burst member.


def manifest_row(filename, p: Pending, info: solutions.SolveInfo) -> str:
    img = p.image
    head = [filename, img.frame_id,
            "" if info.solution_id is None else info.solution_id,
            img.exposure_ms, img.width, img.height]
    ps = info.solution
    if ps is not None:
        solve_ms = ""
        if ps.HasField("solve_time"):
            solve_ms = "%.2f" % (ps.solve_time.ToNanoseconds() / 1e6)
        mid = ["true", str(ps.solution_from_imu).lower(),
               "%.6f" % ps.image_sky_coord.ra, "%.6f" % ps.image_sky_coord.dec,
               "%.4f" % ps.roll, "%.4f" % ps.fov, "%.2f" % ps.rmse,
               ps.num_matches, solve_ms]
    else:
        mid = ["false", "false", "", "", "", "", "", "", ""]
    tail = [img.acquire_time.strftime("%Y-%m-%dT%H:%M:%S%z"),
            p.burst_id, p.burst_index,
            "" if img.camera_gain is None else img.camera_gain,
            "" if p.frame_gap is None else p.frame_gap,
            info.status]
    return ",".join(str(v) for v in head + mid + tail)


def image_filename(p: Pending, ext: str) -> str:
    img = p.image
    return "img_%06d_%dms_%s.%s" % (
        p.seq, img.exposure_ms, img.acquire_time.strftime("%Y%m%d_%H%M%S"),
        ext)


class SessionWriter:
    def __init__(self, session_dir, tracker, save_format="png",
                 solution_timeout=5.0, only_solved=False,
                 max_unsolved=None, max_frames=None, encode_threads=2,
                 max_in_flight=4):
        self.dir = session_dir
        self.tracker = tracker
        self.save_format = save_format
        self.solution_timeout = solution_timeout
        self.only_solved = only_solved
        self.max_unsolved = max_unsolved
        self.max_frames = max_frames
        self._pool = ThreadPoolExecutor(max_workers=encode_threads,
                                        thread_name_prefix="encode")
        # Bounds memory (each frame is ~2 MB) and how far fetching can run
        # ahead of the kept-frame/byte counters that the limits check.
        self._queue = queue.Queue(maxsize=max_in_flight)
        self._lock = threading.Lock()
        self.kept = 0
        self.unsolved_kept = 0
        self.discarded = 0
        self.bytes_written = 0
        self.status_counts = {}
        path = os.path.join(session_dir, "manifest.csv")
        self._manifest = open(path, "w", buffering=1)
        self._manifest.write(",".join(COLUMNS) + "\n")
        self._thread = threading.Thread(target=self._run,
                                        name="manifest", daemon=True)
        self._thread.start()

    def submit(self, pending: Pending):
        """Queues a frame; blocks while max_in_flight frames are pending."""
        future = self._pool.submit(self._encode, pending)
        self._queue.put((pending, future))

    def close(self):
        """Drains all submitted frames and closes the manifest."""
        self._queue.put(None)
        self._thread.join()
        self._pool.shutdown()
        self._manifest.close()

    def _encode(self, p: Pending):
        name = image_filename(p, self.save_format)
        path = os.path.join(self.dir, name)
        if self.save_format == "bmp" and p.image.data[:2] == b"BM":
            with open(path, "wb") as f:
                f.write(p.image.data)
        else:
            PILImage.open(io.BytesIO(p.image.data)).save(path)
        return name, path

    def _keep(self, status) -> bool:
        if self.max_frames is not None and self.kept >= self.max_frames:
            return False
        if status in (solutions.SOLVED, solutions.IMU):
            return True
        if self.only_solved:
            return False
        if (self.max_unsolved is not None
                and self.unsolved_kept >= self.max_unsolved):
            return False
        return True

    def _run(self):
        while True:
            item = self._queue.get()
            if item is None:
                return
            pending, future = item
            try:
                name, path = future.result()
            except Exception:
                log.exception("Failed to save frame %d",
                              pending.image.frame_id)
                continue
            info = self.tracker.wait_for(pending.image.frame_id,
                                         self.solution_timeout)
            with self._lock:
                self.status_counts[info.status] = (
                    self.status_counts.get(info.status, 0) + 1)
                if not self._keep(info.status):
                    os.remove(path)
                    self.discarded += 1
                    continue
                self.kept += 1
                if info.status not in (solutions.SOLVED, solutions.IMU):
                    self.unsolved_kept += 1
                self.bytes_written += os.path.getsize(path)
            self._manifest.write(manifest_row(name, pending, info) + "\n")
            log.info("%s frame %d burst %d.%d %s", name,
                     pending.image.frame_id, pending.burst_id,
                     pending.burst_index, info.status)

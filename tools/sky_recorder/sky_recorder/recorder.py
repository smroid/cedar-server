"""The recording loop: schedule -> GetImage -> SessionWriter."""

import datetime
import json
import logging
import os
import socket
import threading
import time
from dataclasses import dataclass, asdict
from typing import Optional

import grpc

from . import client, schedule, solutions, writer
from .cedar_client import cedar_pb2

log = logging.getLogger(__name__)


@dataclass
class Options:
    server: str
    out: str
    every: float = 10.0
    burst: int = 1
    duration: Optional[float] = None  # Seconds.
    max_frames: Optional[int] = None
    max_megabytes: Optional[float] = None
    max_unsolved: Optional[int] = None
    only_solved: bool = False
    save_format: str = "png"           # png | bmp | jpg
    jpeg_quality: Optional[int] = None  # Only with save_format=jpg.
    solution_timeout: float = 5.0
    fetch_timeout: float = 60.0  # A 2 MB frame over weak WiFi can take 20 s.


def make_session_dir(out: str) -> str:
    base = os.path.join(out, datetime.datetime.now().strftime("%Y%m%d_%H%M%S"))
    path, n = base, 1
    while os.path.exists(path):
        n += 1
        path = "%s_%d" % (base, n)
    os.makedirs(path)
    return path


class Recorder:
    def __init__(self, opts: Options, stub=None, clock=time.monotonic):
        self.opts = opts
        self.stub = stub or client.connect(opts.server)
        self.clock = clock
        self.stop_event = threading.Event()
        self.session_dir = None
        self.stop_reason = None

    def stop(self, reason="stopped"):
        if self.stop_reason is None:
            self.stop_reason = reason
        self.stop_event.set()

    def _session_info(self, frame_result):
        info = frame_result.server_information
        return {
            "server": self.opts.server,
            "client_host": socket.gethostname(),
            "cedar_server_version": info.cedar_server_version,
            "product_name": info.product_name,
            "operating_mode": cedar_pb2.OperatingMode.Name(
                frame_result.operation_settings.operating_mode),
            "options": asdict(self.opts),
            "start_time": datetime.datetime.now().astimezone().isoformat(),
        }

    def _write_session(self, info):
        with open(os.path.join(self.session_dir, "session.json"), "w") as f:
            json.dump(info, f, indent=2)
            f.write("\n")

    def _sleep(self, seconds, start):
        """Waits, waking early on stop or when --duration runs out."""
        if self.opts.duration is not None:
            seconds = min(seconds,
                          self.opts.duration - (self.clock() - start))
        self.stop_event.wait(max(0.0, seconds))

    def run(self):
        o = self.opts
        frame_result = client.get_frame(self.stub)
        if (frame_result.operation_settings.operating_mode
                != cedar_pb2.OPERATE):
            log.warning("Server is not in OPERATE mode: no plate solves, "
                        "so frames will be recorded as 'unknown'.")
        self.session_dir = make_session_dir(o.out)
        session = self._session_info(frame_result)
        self._write_session(session)
        log.info("Recording to %s", self.session_dir)

        fmt = cedar_pb2.JPEG if o.save_format == "jpg" else cedar_pb2.BMP
        tracker = solutions.SolutionTracker(self.stub).start()
        sink = writer.SessionWriter(
            self.session_dir, tracker, save_format=o.save_format,
            solution_timeout=o.solution_timeout, only_solved=o.only_solved,
            max_unsolved=o.max_unsolved, max_frames=o.max_frames)
        sched = schedule.Schedule(o.every, o.burst)
        start = self.clock()
        prev_frame_id = None
        last_burst = None
        seq = 0
        backoff = 1.0
        try:
            while not self.stop_event.is_set():
                now = self.clock()
                if o.duration is not None and now - start >= o.duration:
                    self.stop("duration reached")
                    break
                if o.max_frames is not None and sink.kept >= o.max_frames:
                    self.stop("max frames reached")
                    break
                if (o.max_megabytes is not None
                        and sink.bytes_written >= o.max_megabytes * 1e6):
                    self.stop("max megabytes reached")
                    break
                action = sched.decide(now)
                if isinstance(action, schedule.WaitUntil):
                    self._sleep(action.t - now, start)
                    continue
                timeout = o.fetch_timeout
                if o.duration is not None:
                    timeout = min(timeout,
                                  max(1.0, o.duration - (now - start)))
                try:
                    img = client.fetch_image(
                        self.stub, prev_frame_id, fmt=fmt,
                        quality=o.jpeg_quality, timeout=timeout)
                except grpc.RpcError as e:
                    if e.code() not in client.RETRYABLE:
                        raise
                    log.warning("GetImage: %s (%s); retrying in %.0fs",
                                e.code().name, e.details(), backoff)
                    self._sleep(backoff, start)
                    backoff = min(backoff * 2, 10.0)
                    continue
                backoff = 1.0
                sched.fetched()
                gap = None
                if action.index > 0 and last_burst == action.burst_id:
                    gap = img.frame_id - prev_frame_id
                prev_frame_id = img.frame_id
                last_burst = action.burst_id
                session.setdefault("camera_model", img.camera_model)
                sink.submit(writer.Pending(seq, img, action.burst_id,
                                           action.index, gap))
                seq += 1
        finally:
            sink.close()
            tracker.stop()
            session.update({
                "end_time": datetime.datetime.now().astimezone().isoformat(),
                "stop_reason": self.stop_reason or "error",
                "frames_fetched": seq,
                "frames_kept": sink.kept,
                "frames_discarded": sink.discarded,
                "bytes_written": sink.bytes_written,
                "solve_status_counts": sink.status_counts,
            })
            self._write_session(session)
        return session

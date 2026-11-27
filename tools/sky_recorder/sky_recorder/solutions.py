"""Pairs images with plate solutions via a background GetFrames stream.

GetImage carries no plate solution, but its frame_id is the same id that
FrameResult.frame_id carries, so we follow GetFrames and remember recent
solutions by frame_id.
"""

import collections
import logging
import threading
import time
from dataclasses import dataclass
from typing import Optional

import grpc

from .cedar_client import cedar_pb2

log = logging.getLogger(__name__)

SOLVED = "solved"      # Camera plate solve.
IMU = "imu"            # Solution interpolated from the IMU, not a solve.
UNSOLVED = "unsolved"  # OPERATE mode, solve attempted, no solution.
UNKNOWN = "unknown"    # No FrameResult for this frame (skipped, SETUP mode,
                       # or timed out).


@dataclass
class SolveInfo:
    status: str
    solution_id: Optional[int] = None
    solution: Optional[object] = None  # cedar_pb2.PlateSolution


def classify(frame_result) -> SolveInfo:
    if frame_result.HasField("plate_solution"):
        ps = frame_result.plate_solution
        return SolveInfo(IMU if ps.solution_from_imu else SOLVED,
                         frame_result.solution_id, ps)
    operating = (frame_result.operation_settings.operating_mode
                 == cedar_pb2.OPERATE)
    return SolveInfo(UNSOLVED if operating else UNKNOWN,
                     frame_result.solution_id)


class SolutionTracker:
    def __init__(self, stub, capacity: int = 1000):
        self._stub = stub
        self._capacity = capacity
        self._results = collections.OrderedDict()  # frame_id -> SolveInfo
        self._max_seen = None
        self._cond = threading.Condition()
        self._stopped = False
        self._call = None
        self._thread = threading.Thread(
            target=self._run, name="solution-tracker", daemon=True)

    def start(self):
        self._thread.start()
        return self

    def stop(self):
        with self._cond:
            self._stopped = True
            call = self._call
            self._cond.notify_all()
        if call is not None:
            call.cancel()
        self._thread.join(timeout=5)

    def record(self, frame_result):
        """Stores one FrameResult. Called by the stream thread (and tests)."""
        info = classify(frame_result)
        fid = frame_result.frame_id
        with self._cond:
            prev = self._results.get(fid)
            # A real solve wins over an IMU interpolation for the same frame.
            if prev is None or prev.status != SOLVED:
                self._results[fid] = info
                self._results.move_to_end(fid)
            while len(self._results) > self._capacity:
                self._results.popitem(last=False)
            if self._max_seen is None or fid > self._max_seen:
                self._max_seen = fid
            self._cond.notify_all()

    def wait_for(self, frame_id: int, timeout: float) -> SolveInfo:
        """Returns the result for frame_id, waiting up to `timeout` seconds.

        Returns UNKNOWN early once a later frame has been seen without this
        one, since the server never reports frames out of order.
        """
        deadline = time.monotonic() + timeout
        with self._cond:
            while True:
                info = self._results.get(frame_id)
                if info is not None:
                    return info
                if self._max_seen is not None and self._max_seen > frame_id:
                    return SolveInfo(UNKNOWN)
                remaining = deadline - time.monotonic()
                if remaining <= 0 or self._stopped:
                    return SolveInfo(UNKNOWN)
                self._cond.wait(remaining)

    def _run(self):
        backoff = 1.0
        while True:
            with self._cond:
                if self._stopped:
                    return
                call = self._stub.GetFrames(cedar_pb2.FrameRequest())
                self._call = call
            try:
                for frame_result in call:
                    self.record(frame_result)
                    backoff = 1.0
            except grpc.RpcError as e:
                with self._cond:
                    if self._stopped:
                        return
                log.warning("GetFrames stream ended (%s); retrying in %.0fs",
                            e.code(), backoff)
            with self._cond:
                self._cond.wait(backoff)
            backoff = min(backoff * 2, 10.0)

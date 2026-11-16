"""When to fetch images: a pure, clock-injected sampling schedule.

One schedule covers both policies:
  * every N seconds:         Schedule(period_s=N, burst=1)
  * burst of M every N s:    Schedule(period_s=N, burst=M)
  * continuous:              Schedule(period_s=0, burst=1)

Bursts start on a start-to-start cadence. If a burst takes longer than the
period, the next burst starts immediately; missed periods are not queued up.
"""

from dataclasses import dataclass
from typing import Optional


@dataclass(frozen=True)
class Fetch:
    """Fetch the next image as member `index` of burst `burst_id`."""
    burst_id: int
    index: int


@dataclass(frozen=True)
class WaitUntil:
    """Fetch nothing until monotonic time `t`."""
    t: float


class Schedule:
    def __init__(self, period_s: float, burst: int = 1):
        if period_s < 0:
            raise ValueError("period must be >= 0")
        if burst < 1:
            raise ValueError("burst must be >= 1")
        self.period_s = period_s
        self.burst = burst
        self._burst_id = -1
        self._index = burst  # No burst in progress.
        self._next_start: Optional[float] = None  # None: start right away.

    def decide(self, now: float):
        """Returns Fetch or WaitUntil for monotonic time `now`."""
        if self._index < self.burst:
            return Fetch(self._burst_id, self._index)
        if self._next_start is not None and now < self._next_start:
            return WaitUntil(self._next_start)
        self._burst_id += 1
        self._index = 0
        self._next_start = now + self.period_s
        return Fetch(self._burst_id, 0)

    def fetched(self):
        """Records that the image returned by the last Fetch was obtained."""
        if self._index < self.burst:
            self._index += 1

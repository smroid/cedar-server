import pytest

from sky_recorder.schedule import Fetch, Schedule, WaitUntil


def run(sched, events):
    """events: list of (now, fetched?) -> list of decisions."""
    out = []
    for now, fetched in events:
        d = sched.decide(now)
        out.append(d)
        if fetched and isinstance(d, Fetch):
            sched.fetched()
    return out


def test_every_n_seconds():
    s = Schedule(10)
    assert s.decide(0) == Fetch(0, 0)
    s.fetched()
    assert s.decide(3) == WaitUntil(10)
    assert s.decide(10) == Fetch(1, 0)
    s.fetched()
    assert s.decide(10.5) == WaitUntil(20)


def test_burst_is_start_to_start():
    s = Schedule(10, burst=3)
    assert run(s, [(0, 1), (1, 1), (2, 1), (3, 0)]) == [
        Fetch(0, 0), Fetch(0, 1), Fetch(0, 2), WaitUntil(10)]


def test_overrun_burst_starts_next_immediately_without_queueing():
    s = Schedule(10, burst=2)
    run(s, [(0, 1), (25, 1)])  # Burst 0 overran into periods 1 and 2.
    assert s.decide(25) == Fetch(1, 0)  # Starts now...
    s.fetched()
    s.decide(26)
    s.fetched()
    assert s.decide(27) == WaitUntil(35)  # ...and cadence resets from 25.


def test_failed_fetch_retries_same_slot():
    s = Schedule(10, burst=2)
    assert s.decide(0) == Fetch(0, 0)
    assert s.decide(1) == Fetch(0, 0)  # No fetched() call: same slot.


def test_continuous():
    s = Schedule(0)
    assert run(s, [(0, 1), (0, 1), (0.1, 1)]) == [
        Fetch(0, 0), Fetch(1, 0), Fetch(2, 0)]


@pytest.mark.parametrize("period,burst", [(-1, 1), (1, 0)])
def test_rejects_bad_params(period, burst):
    with pytest.raises(ValueError):
        Schedule(period, burst)

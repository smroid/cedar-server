import csv
import json
import os

import pytest
from PIL import Image

from sky_recorder import writer
from sky_recorder.cedar_client import cedar_pb2
from sky_recorder.recorder import Options, Recorder

from fake_server import H, W, Running, pixels


@pytest.fixture
def server(request):
    kw = getattr(request, "param", {})
    s = Running(**kw)
    yield s
    s.close()


def record(server, tmp_path, **kw):
    opts = Options(server=server.address, out=str(tmp_path), **kw)
    rec = Recorder(opts)
    session = rec.run()
    with open(os.path.join(rec.session_dir, "manifest.csv")) as f:
        rows = list(csv.DictReader(f))
    return rec, session, rows


def test_burst_manifest_images_and_solutions(server, tmp_path):
    rec, session, rows = record(server, tmp_path, every=0.2, burst=3,
                                max_frames=6)
    assert session["stop_reason"] == "max frames reached"
    assert len(rows) == 6
    assert list(rows[0].keys()) == writer.COLUMNS
    assert [(r["burst_id"], r["burst_index"]) for r in rows] == [
        ("0", "0"), ("0", "1"), ("0", "2"), ("1", "0"), ("1", "1"), ("1", "2")]
    assert rows[0]["frame_gap"] == "" and rows[3]["frame_gap"] == ""
    for r in rows:
        fid = int(r["frame_id"])
        if r["burst_index"] != "0":
            assert int(r["frame_gap"]) >= 1
        assert r["exposure_ms"] == "123"
        assert r["camera_gain"] == "42"
        assert r["filename"].endswith(".png")
        assert "_123ms_" in r["filename"]
        img = Image.open(os.path.join(rec.session_dir, r["filename"]))
        assert img.size == (W, H) and img.mode == "L"
        assert img.tobytes() == pixels(fid)  # Lossless round trip.
        if fid % 2 == 0:
            assert r["solve_status"] == "solved" and r["solved"] == "true"
            assert float(r["ra_deg"]) == pytest.approx(100.0 + fid)
            assert r["solve_ms"] == "4.00"
            assert r["num_matches"] == "17"
        else:
            assert r["solve_status"] == "unsolved" and r["solved"] == "false"
            assert r["ra_deg"] == ""
    meta = json.load(open(os.path.join(rec.session_dir, "session.json")))
    assert meta["cedar_server_version"] == "fake-1.0"
    assert meta["camera_model"] == "FakeCam"
    assert meta["frames_kept"] == 6


def test_never_refetches_same_frame(server, tmp_path):
    _, _, rows = record(server, tmp_path, every=0, max_frames=10)
    ids = [int(r["frame_id"]) for r in rows]
    assert ids == sorted(set(ids))


def test_only_solved(server, tmp_path):
    rec, session, rows = record(server, tmp_path, every=0, max_frames=4,
                                only_solved=True)
    assert rows and all(r["solve_status"] == "solved" for r in rows)
    files = {f for f in os.listdir(rec.session_dir) if f.endswith(".png")}
    assert files == {r["filename"] for r in rows}  # Discards were deleted.


def test_max_unsolved(server, tmp_path):
    _, _, rows = record(server, tmp_path, every=0, max_frames=8,
                        max_unsolved=1)
    assert sum(r["solve_status"] == "unsolved" for r in rows) == 1


def test_bmp_saves_server_bytes(server, tmp_path):
    rec, _, rows = record(server, tmp_path, every=0, max_frames=2,
                          save_format="bmp")
    path = os.path.join(rec.session_dir, rows[0]["filename"])
    assert path.endswith(".bmp")
    assert Image.open(path).tobytes() == pixels(int(rows[0]["frame_id"]))


def test_max_megabytes(server, tmp_path):
    limit_mb = 0.001
    _, session, rows = record(server, tmp_path, every=0,
                              max_megabytes=limit_mb)
    assert session["stop_reason"] == "max megabytes reached"
    assert session["bytes_written"] >= limit_mb * 1e6
    # Overshoot is bounded by frames already in flight (writer.max_in_flight
    # queued plus one being fetched and one being finished).
    per_frame = session["bytes_written"] / len(rows)
    assert len(rows) <= limit_mb * 1e6 / per_frame + 6


def test_duration(server, tmp_path):
    _, session, _ = record(server, tmp_path, every=0.05, duration=0.3)
    assert session["stop_reason"] == "duration reached"


@pytest.mark.parametrize("server", [{"mode": cedar_pb2.SETUP}],
                         indirect=True)
def test_setup_mode_frames_are_unknown(server, tmp_path):
    _, _, rows = record(server, tmp_path, every=0, max_frames=3,
                        solution_timeout=1.0)
    assert rows and all(r["solve_status"] == "unknown" for r in rows)

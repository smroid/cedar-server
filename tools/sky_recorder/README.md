# sky_recorder

Records real-sky frames from a Cedar server, each paired with its plate
solution, for building benchmark corpora. It is a plain gRPC client: it uses
`GetImage` for the full-resolution frames and `GetFrames` for the solutions, and
needs no server changes.

```
pip install -r requirements.txt
python -m sky_recorder --server 192.168.4.1:80 --out ~/corpus --every 10
python -m sky_recorder --server 192.168.4.1:80 --out ~/corpus --every 30 --burst 20
```

`--every N` starts a capture every N seconds (`0` captures continuously).
`--burst M` fetches M frames back to back per capture. Limits: `--duration`,
`--max-frames`, `--max-megabytes`, `--max-unsolved`, `--only-solved`. See
`--help` for the rest.

Each run writes a new `<out>/<YYYYmmdd_HHMMSS>/` containing PNG frames,
`manifest.csv` (one row per frame) and `session.json` (run metadata).
`solve_status` is `solved`, `imu`, `unsolved`, or `unknown` (the server did not
solve that frame). The server must be in Operate mode for frames to get
solutions.

A full-resolution frame is about 2 MB. Over WiFi, bursts therefore skip
frames; `frame_gap` shows how many. For consecutive frames, run the recorder
on the Cedar box itself (`--server localhost:80`), adding `--save-format bmp`
there to skip PNG encoding.

Tests: `pytest` (uses an in-process fake server). After changing the protos,
run `./gen_protos.sh`.

"""CLI: python -m sky_recorder --server HOST:PORT --out DIR [policy] [limits]"""

import argparse
import logging
import signal
import sys

from .recorder import Options, Recorder


def parse_args(argv=None) -> Options:
    p = argparse.ArgumentParser(
        prog="sky_recorder",
        description="Record real-sky frames + plate solutions from a Cedar "
                    "server into a new session directory.")
    p.add_argument("--server", required=True,
                   help="Cedar server host:port, e.g. 192.168.4.1:80")
    p.add_argument("--out", required=True,
                   help="Output root; each run creates a timestamped subdir")
    g = p.add_argument_group("sampling")
    g.add_argument("--every", type=float, default=10.0, metavar="SECONDS",
                   help="Start a capture every SECONDS (0 = continuous). "
                        "Default 10")
    g.add_argument("--burst", type=int, default=1, metavar="M",
                   help="Frames per capture, fetched back to back. Default 1")
    g = p.add_argument_group("limits")
    g.add_argument("--duration", type=float, metavar="SECONDS")
    g.add_argument("--max-frames", type=int)
    g.add_argument("--max-megabytes", type=float,
                   help="Stop once this many MB of images are written")
    g.add_argument("--max-unsolved", type=int,
                   help="Discard unsolved frames after keeping this many")
    g.add_argument("--only-solved", action="store_true",
                   help="Keep only frames with a plate solution")
    g = p.add_argument_group("output")
    g.add_argument("--save-format", choices=["png", "bmp", "jpg"],
                   default="png",
                   help="png (default, lossless), bmp (lossless, no encode "
                        "CPU), jpg (lossy, fetched as JPEG: least bandwidth)")
    g.add_argument("--jpeg-quality", type=int,
                   help="With --save-format jpg; server default 90")
    g.add_argument("--solution-timeout", type=float, default=5.0,
                   help="Seconds to wait for a frame's plate solution")
    g.add_argument("--fetch-timeout", type=float, default=60.0,
                   help="Seconds to wait for one image. Default 60")
    p.add_argument("-q", "--quiet", action="store_true")
    a = p.parse_args(argv)
    if a.every < 0:
        p.error("--every must be >= 0")
    if a.burst < 1:
        p.error("--burst must be >= 1")
    return Options(
        server=a.server, out=a.out, every=a.every, burst=a.burst,
        duration=a.duration, max_frames=a.max_frames,
        max_megabytes=a.max_megabytes, max_unsolved=a.max_unsolved,
        only_solved=a.only_solved, save_format=a.save_format,
        jpeg_quality=a.jpeg_quality, solution_timeout=a.solution_timeout,
        fetch_timeout=a.fetch_timeout,
    ), a.quiet


def main(argv=None):
    opts, quiet = parse_args(argv)
    logging.basicConfig(
        level=logging.WARNING if quiet else logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s", datefmt="%H:%M:%S")
    rec = Recorder(opts)

    def on_sigint(signum, frame):
        if rec.stop_event.is_set():
            raise KeyboardInterrupt
        logging.info("Stopping after the current frame (Ctrl-C again to "
                     "abort)...")
        rec.stop("interrupted")

    signal.signal(signal.SIGINT, on_sigint)
    session = rec.run()
    print("%s: kept %d of %d frames (%.1f MB), %s; stop: %s" % (
        rec.session_dir, session["frames_kept"], session["frames_fetched"],
        session["bytes_written"] / 1e6, session["solve_status_counts"],
        session["stop_reason"]))
    return 0


if __name__ == "__main__":
    sys.exit(main())

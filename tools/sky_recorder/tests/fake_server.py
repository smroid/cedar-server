"""An in-process fake Cedar server serving synthetic frames."""

import io
import threading
from concurrent import futures

import grpc
from google.protobuf import duration_pb2, timestamp_pb2
from PIL import Image

from sky_recorder.cedar_client import cedar_pb2, cedar_pb2_grpc

W, H = 64, 48


def pixels(frame_id):
    return bytes((frame_id * 7 + i) % 256 for i in range(W * H))


class FakeCedar(cedar_pb2_grpc.CedarServicer):
    """Frames tick every `frame_interval` s. Even frames solve, odd frames
    don't (in OPERATE mode)."""

    def __init__(self, frame_interval=0.01, mode=cedar_pb2.OPERATE,
                 chunk=1000):
        self.frame_interval = frame_interval
        self.mode = mode
        self.chunk = chunk
        self.frame_id = 0
        self.cond = threading.Condition()
        self.stopped = False
        self.image_requests = []
        threading.Thread(target=self._tick, daemon=True).start()

    def _tick(self):
        import time
        while not self.stopped:
            time.sleep(self.frame_interval)
            with self.cond:
                self.frame_id += 1
                self.cond.notify_all()

    def _next(self, prev, context):
        with self.cond:
            while prev is not None and self.frame_id == prev:
                if not context.is_active() or self.stopped:
                    return None
                self.cond.wait(0.1)
            return self.frame_id

    def frame_result(self, fid):
        fr = cedar_pb2.FrameResult(frame_id=fid, solution_id=fid)
        fr.operation_settings.operating_mode = self.mode
        fr.server_information.cedar_server_version = "fake-1.0"
        if self.mode == cedar_pb2.OPERATE and fid % 2 == 0:
            ps = fr.plate_solution
            ps.image_sky_coord.ra = 100.0 + fid
            ps.image_sky_coord.dec = 20.0
            ps.roll = 5.0
            ps.fov = 12.8
            ps.rmse = 3.25
            ps.num_matches = 17
            ps.solve_time.FromMilliseconds(4)
        return fr

    def GetFrame(self, request, context):
        return self.frame_result(self.frame_id)

    def GetFrames(self, request, context):
        prev = request.prev_frame_id if request.HasField("prev_frame_id") \
            else None
        while context.is_active():
            fid = self._next(prev, context)
            if fid is None:
                return
            yield self.frame_result(fid)
            prev = fid

    def GetImage(self, request, context):
        prev = request.prev_frame_id if request.HasField("prev_frame_id") \
            else None
        self.image_requests.append(prev)
        fid = self._next(prev, context)
        buf = io.BytesIO()
        Image.frombytes("L", (W, H), pixels(fid)).save(buf, format="BMP")
        data = buf.getvalue()
        first = cedar_pb2.ImageResult(
            frame_id=fid, width=W, height=H, camera_gain=42,
            acquire_time=timestamp_pb2.Timestamp(seconds=1_780_000_000),
            exposure_time=duration_pb2.Duration(nanos=123_456_789))
        first.camera.model = "FakeCam"
        for i in range(0, len(data), self.chunk):
            r = first if i == 0 else cedar_pb2.ImageResult()
            r.image_chunk = data[i:i + self.chunk]
            yield r


class Running:
    def __init__(self, **kw):
        self.servicer = FakeCedar(**kw)
        self.server = grpc.server(futures.ThreadPoolExecutor(max_workers=8))
        cedar_pb2_grpc.add_CedarServicer_to_server(self.servicer, self.server)
        port = self.server.add_insecure_port("127.0.0.1:0")
        self.address = "127.0.0.1:%d" % port
        self.server.start()

    def close(self):
        self.servicer.stopped = True
        self.server.stop(0)

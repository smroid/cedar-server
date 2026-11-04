"""Thin wrappers over the Cedar gRPC service."""

import datetime
from dataclasses import dataclass
from typing import Optional

import grpc

from .cedar_client import cedar_pb2, cedar_pb2_grpc

# Errors that mean "try again shortly": calibrating, not acquiring (e.g. no
# client has woken the camera yet), network hiccup.
RETRYABLE = (
    grpc.StatusCode.FAILED_PRECONDITION,
    grpc.StatusCode.UNAVAILABLE,
    grpc.StatusCode.DEADLINE_EXCEEDED,
)


@dataclass
class Image:
    frame_id: int
    width: int
    height: int
    acquire_time: datetime.datetime  # Timezone-aware, client's local zone.
    exposure_ms: int
    camera_gain: Optional[int]
    camera_model: str
    data: bytes  # Encoded image, as returned by the server.


def connect(server: str):
    """Returns a Cedar stub for `server` ("host:port")."""
    channel = grpc.insecure_channel(server)
    return cedar_pb2_grpc.CedarStub(channel)


def fetch_image(stub, prev_frame_id: Optional[int],
                fmt=cedar_pb2.BMP, quality: Optional[int] = None,
                timeout: float = 30.0) -> Image:
    """Fetches the newest image (newer than prev_frame_id, if given)."""
    req = cedar_pb2.ImageRequest(format=fmt)
    if prev_frame_id is not None:
        req.prev_frame_id = prev_frame_id
    if quality is not None:
        req.quality = quality
    first = None
    chunks = []
    for result in stub.GetImage(req, timeout=timeout):
        if first is None:
            first = result
        chunks.append(result.image_chunk)
    if first is None:
        raise RuntimeError("GetImage returned an empty stream")
    acquire = first.acquire_time.ToDatetime(
        tzinfo=datetime.timezone.utc).astimezone()
    # Truncate to whole ms, as the server-side sampler did.
    exposure_ms = (first.exposure_time.seconds * 1000
                   + first.exposure_time.nanos // 1_000_000)
    return Image(
        frame_id=first.frame_id,
        width=first.width,
        height=first.height,
        acquire_time=acquire,
        exposure_ms=exposure_ms,
        camera_gain=(first.camera_gain if first.HasField("camera_gain")
                     else None),
        camera_model=first.camera.model if first.HasField("camera") else "",
        data=b"".join(chunks),
    )


def get_frame(stub, timeout: float = 10.0):
    """Returns the server's current FrameResult without waiting."""
    return stub.GetFrame(cedar_pb2.FrameRequest(), timeout=timeout)

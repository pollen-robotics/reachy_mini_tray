#!/usr/bin/env python3
"""Stream head/antenna targets to place a local Reachy Mini into the sleep pose.

This is a tiny, dependency-free debug helper. It talks to the daemon running on
localhost (the same one the tray connects to) and streams `set_target` frames at
a fixed rate, driving the robot from its *current* pose to the canonical "sleep"
(dodo) pose.

Two motion styles:
  - "human" (default): makes it look like a person grabbing the robot and
    seating it into place - ballistic reach + overshoot + corrective nudges +
    regrip pauses + hand tremor, antennas folded on their own timing. See the
    "Human-like motion model" section below.
  - "smooth": a single clean min-jerk glide (robotic, the old behaviour).

Endpoints used (all on the local daemon, default 127.0.0.1:8000):
  GET  /api/state/present_head_pose?use_pose_matrix=true
  GET  /api/state/present_body_yaw
  GET  /api/state/present_antenna_joint_positions
  POST /api/move/set_target                     (streamed, one frame per tick)

The daemon also exposes a WebSocket variant (`/api/move/ws/set_target`) and a
one-shot built-in move (`POST /api/move/play/goto_sleep`). We deliberately
stream small `set_target` frames so the motion is driven entirely from here.

Note: the daemon ignores `set_target` while a recorded/goto move is running, so
make sure nothing else is moving the robot when you run this.

Usage:
  python dodo_stream.py                 # human-like placement into dodo (~2.5s)
  python dodo_stream.py --style smooth  # clean robotic glide
  python dodo_stream.py --duration 4    # slower
  python dodo_stream.py --wake          # placed back to the neutral/init pose
  python dodo_stream.py --seed 42       # reproducible human motion
  python dodo_stream.py --tremor 1.5    # shakier hand
  python dodo_stream.py --random        # one-shot set to a near-dodo pose (no anim)
  python dodo_stream.py --host 127.0.0.1 --port 8000 --hz 50
"""

from __future__ import annotations

import argparse
import http.client
import json
import math
import random
import sys
import time

# --- Canonical poses (kept in sync with reachy_mini/reachy_mini.py) ----------

# Sleep / dodo pose: head tucked down-and-forward, antennas folded back.
SLEEP_HEAD_POSE = [
    [0.911, 0.004, 0.413, -0.021],
    [-0.004, 1.0, -0.001, 0.001],
    [-0.413, -0.001, 0.911, -0.044],
    [0.0, 0.0, 0.0, 1.0],
]
SLEEP_ANTENNAS = (-3.05, 3.05)

# Neutral upright pose (identity head), antennas ~vertical.
INIT_HEAD_POSE = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
]
INIT_ANTENNAS = (-0.1745, 0.1745)


# --- Tiny linear-algebra helpers (pure Python, no numpy) ---------------------


def mat3_to_quat(r: list[list[float]]) -> list[float]:
    """Convert a 3x3 rotation matrix to a quaternion [w, x, y, z]."""
    trace = r[0][0] + r[1][1] + r[2][2]
    if trace > 0.0:
        s = math.sqrt(trace + 1.0) * 2.0
        w = 0.25 * s
        x = (r[2][1] - r[1][2]) / s
        y = (r[0][2] - r[2][0]) / s
        z = (r[1][0] - r[0][1]) / s
    elif r[0][0] > r[1][1] and r[0][0] > r[2][2]:
        s = math.sqrt(1.0 + r[0][0] - r[1][1] - r[2][2]) * 2.0
        w = (r[2][1] - r[1][2]) / s
        x = 0.25 * s
        y = (r[0][1] + r[1][0]) / s
        z = (r[0][2] + r[2][0]) / s
    elif r[1][1] > r[2][2]:
        s = math.sqrt(1.0 + r[1][1] - r[0][0] - r[2][2]) * 2.0
        w = (r[0][2] - r[2][0]) / s
        x = (r[0][1] + r[1][0]) / s
        y = 0.25 * s
        z = (r[1][2] + r[2][1]) / s
    else:
        s = math.sqrt(1.0 + r[2][2] - r[0][0] - r[1][1]) * 2.0
        w = (r[1][0] - r[0][1]) / s
        x = (r[0][2] + r[2][0]) / s
        y = (r[1][2] + r[2][1]) / s
        z = 0.25 * s
    return _normalize_quat([w, x, y, z])


def quat_to_mat3(q: list[float]) -> list[list[float]]:
    """Convert a quaternion [w, x, y, z] to a 3x3 rotation matrix."""
    w, x, y, z = q
    return [
        [1 - 2 * (y * y + z * z), 2 * (x * y - z * w), 2 * (x * z + y * w)],
        [2 * (x * y + z * w), 1 - 2 * (x * x + z * z), 2 * (y * z - x * w)],
        [2 * (x * z - y * w), 2 * (y * z + x * w), 1 - 2 * (x * x + y * y)],
    ]


def _normalize_quat(q: list[float]) -> list[float]:
    n = math.sqrt(sum(c * c for c in q)) or 1.0
    return [c / n for c in q]


def slerp(q0: list[float], q1: list[float], t: float) -> list[float]:
    """Spherical linear interpolation between two quaternions."""
    dot = sum(a * b for a, b in zip(q0, q1))
    # Take the shorter arc.
    if dot < 0.0:
        q1 = [-c for c in q1]
        dot = -dot
    if dot > 0.9995:
        # Nearly parallel: fall back to (normalized) linear interpolation.
        return _normalize_quat([a + t * (b - a) for a, b in zip(q0, q1)])
    theta_0 = math.acos(max(-1.0, min(1.0, dot)))
    sin_theta_0 = math.sin(theta_0)
    s0 = math.sin((1.0 - t) * theta_0) / sin_theta_0
    s1 = math.sin(t * theta_0) / sin_theta_0
    return _normalize_quat([a * s0 + b * s1 for a, b in zip(q0, q1)])


def lerp(a: float, b: float, t: float) -> float:
    return a + (b - a) * t


def smoothstep(t: float) -> float:
    """Ease-in-out so the glide starts and ends gently."""
    t = max(0.0, min(1.0, t))
    return t * t * (3.0 - 2.0 * t)


def clamp01(t: float) -> float:
    return max(0.0, min(1.0, t))


def clamp(v: float, lo: float, hi: float) -> float:
    return max(lo, min(hi, v))


def min_jerk(x: float) -> float:
    """Minimum-jerk (5th order) easing - the natural profile of a single human
    submovement. Applied *per segment*, not globally."""
    x = clamp01(x)
    return x * x * x * (10.0 + x * (-15.0 + 6.0 * x))


def quat_mul(a: list[float], b: list[float]) -> list[float]:
    """Hamilton product a*b of two quaternions [w, x, y, z]."""
    w1, x1, y1, z1 = a
    w2, x2, y2, z2 = b
    return [
        w1 * w2 - x1 * x2 - y1 * y2 - z1 * z2,
        w1 * x2 + x1 * w2 + y1 * z2 - z1 * y2,
        w1 * y2 - x1 * z2 + y1 * w2 + z1 * x2,
        w1 * z2 + x1 * y2 - y1 * x2 + z1 * w2,
    ]


def quat_from_euler(rx: float, ry: float, rz: float) -> list[float]:
    """Small-angle rotation quaternion from extrinsic xyz Euler angles (rad)."""
    cx, sx = math.cos(rx / 2), math.sin(rx / 2)
    cy, sy = math.cos(ry / 2), math.sin(ry / 2)
    cz, sz = math.cos(rz / 2), math.sin(rz / 2)
    qx = [cx, sx, 0.0, 0.0]
    qy = [cy, 0.0, sy, 0.0]
    qz = [cz, 0.0, 0.0, sz]
    return quat_mul(quat_mul(qz, qy), qx)


def pose_to_flat(r: list[list[float]], trans: list[float]) -> list[float]:
    """Build a row-major flattened 4x4 matrix from a 3x3 rotation + translation."""
    return [
        r[0][0], r[0][1], r[0][2], trans[0],
        r[1][0], r[1][1], r[1][2], trans[1],
        r[2][0], r[2][1], r[2][2], trans[2],
        0.0, 0.0, 0.0, 1.0,
    ]


def flat_to_rot_trans(m: list[float]) -> tuple[list[list[float]], list[float]]:
    """Split a flat row-major 4x4 into a 3x3 rotation and a translation."""
    r = [
        [m[0], m[1], m[2]],
        [m[4], m[5], m[6]],
        [m[8], m[9], m[10]],
    ]
    trans = [m[3], m[7], m[11]]
    return r, trans


# --- Daemon HTTP client ------------------------------------------------------


class Daemon:
    """Thin daemon client over a *persistent* keep-alive connection.

    At the stream rate (~50 Hz) opening a fresh TCP connection per frame (what
    urllib does) piles up overhead and occasionally makes the daemon slow enough
    to trip a read timeout. We instead keep one `http.client` connection open and
    reuse it, transparently reconnecting if the socket goes stale.
    """

    def __init__(self, host: str, port: int, timeout: float = 2.0) -> None:
        self.host = host
        self.port = port
        self.timeout = timeout
        self.base = f"http://{host}:{port}"
        self._conn: http.client.HTTPConnection | None = None

    def _connect(self) -> http.client.HTTPConnection:
        self._conn = http.client.HTTPConnection(self.host, self.port, timeout=self.timeout)
        return self._conn

    def close(self) -> None:
        if self._conn is not None:
            try:
                self._conn.close()
            except OSError:
                pass
            self._conn = None

    def __enter__(self) -> "Daemon":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()

    def _request(
        self, method: str, path: str, payload: dict | None = None, retries: int = 0
    ) -> object:
        """Send a request on the persistent connection.

        On any error (timeout, stale keep-alive socket, ...) the connection is
        dropped and, if `retries` allows, re-established and retried after a
        short backoff. The hot streaming path uses `retries=0` so a stall can't
        block pacing (the caller skips the frame instead); one-shot calls use a
        couple of retries so a single daemon hiccup doesn't fail the command.
        """
        data = json.dumps(payload).encode() if payload is not None else None
        headers = {"Content-Type": "application/json"} if data is not None else {}
        last_exc: Exception | None = None
        for attempt in range(retries + 1):
            try:
                conn = self._conn or self._connect()
                conn.request(method, path, body=data, headers=headers)
                resp = conn.getresponse()
                body = resp.read()  # must drain so the socket can be reused
                return json.loads(body.decode()) if body else None
            except (OSError, http.client.HTTPException) as e:
                last_exc = e
                self.close()
                if attempt < retries:
                    time.sleep(0.15)
        assert last_exc is not None
        raise last_exc

    def _get(self, path: str, retries: int = 2) -> object:
        return self._request("GET", path, retries=retries)

    def _post(self, path: str, payload: dict, retries: int = 2) -> object:
        return self._request("POST", path, payload, retries=retries)

    def present_head_pose(self) -> list[float]:
        """Return the current head pose as a flat row-major 4x4 matrix."""
        data = self._get("/api/state/present_head_pose?use_pose_matrix=true")
        return list(data["m"])  # type: ignore[index]

    def present_body_yaw(self) -> float:
        return float(self._get("/api/state/present_body_yaw"))  # type: ignore[arg-type]

    def present_antennas(self) -> tuple[float, float]:
        a = self._get("/api/state/present_antenna_joint_positions")
        return (float(a[0]), float(a[1]))  # type: ignore[index]

    def motor_mode(self) -> str:
        data = self._get("/api/motors/status")
        return str(data["mode"])  # type: ignore[index]

    def set_motor_mode(self, mode: str) -> object:
        # No body; the mode is a path parameter.
        return self._post(f"/api/motors/set_mode/{mode}", {})

    def set_target(
        self,
        head_flat: list[float],
        antennas: tuple[float, float],
        body_yaw: float,
        retries: int = 2,
    ) -> object:
        # Streaming callers pass retries=0 (skip the frame on failure); one-shot
        # callers keep the default so a transient hiccup doesn't fail the command.
        return self._post(
            "/api/move/set_target",
            {
                "target_head_pose": {"m": head_flat},
                "target_antennas": [antennas[0], antennas[1]],
                "target_body_yaw": body_yaw,
            },
            retries=retries,
        )


# --- Human-like motion model -------------------------------------------------
#
# A person grabbing the robot and seating it into the dodo pose does NOT move in
# a single smooth min-jerk glide. Motor-control research models human reaching
# as a chain of *submovements*: one fast ballistic transport that tends to
# overshoot, then a few corrective nudges that home in on the target, with short
# regrip pauses between them and a constant low-amplitude hand tremor on top.
#
# We reproduce that with three ingredients:
#   1. a segmented progress timeline (ballistic reach -> overshoot -> correction
#      -> settle) with dwell pauses, each segment min-jerk on its own;
#   2. band-limited tremor (an Ornstein-Uhlenbeck process) added to the head
#      pose, scaled up while the "hand" moves fast and fading as it settles;
#   3. antennas folded on their own timeline (delayed, quicker, one lagging the
#      other, with a small springy overshoot) - fingers, not the same grip.


class Segment:
    """One piece of a progress timeline: min-jerk from p_from to p_to."""

    __slots__ = ("t0", "dur", "p_from", "p_to")

    def __init__(self, t0: float, dur: float, p_from: float, p_to: float) -> None:
        self.t0, self.dur, self.p_from, self.p_to = t0, dur, p_from, p_to


def build_timeline(
    waypoints: list[float],
    duration: float,
    rng: random.Random,
    dwell_after: set[int],
) -> tuple[list[Segment], float]:
    """Turn progress waypoints into timed min-jerk segments + regrip dwells.

    Segment durations scale with sqrt(distance) (a Fitts-like law: longer hops
    take a bit more time, but sublinearly), then the whole thing is rescaled so
    the moving part fits ``duration``.
    """
    dists = [abs(waypoints[i + 1] - waypoints[i]) for i in range(len(waypoints) - 1)]
    raw = [max(0.05, math.sqrt(d)) for d in dists]
    dwell_raw = {i: rng.uniform(0.10, 0.28) for i in dwell_after}
    total_raw = sum(raw) + sum(dwell_raw.values())
    scale = duration / total_raw if total_raw > 0 else 1.0

    segs: list[Segment] = []
    t = 0.0
    for i, mv in enumerate(raw):
        d = mv * scale
        segs.append(Segment(t, d, waypoints[i], waypoints[i + 1]))
        t += d
        if i in dwell_raw:
            dd = dwell_raw[i] * scale
            segs.append(Segment(t, dd, waypoints[i + 1], waypoints[i + 1]))
            t += dd
    return segs, t


def sample_timeline(segs: list[Segment], total: float, t: float) -> float:
    if t <= 0.0:
        return segs[0].p_from
    if t >= total:
        return segs[-1].p_to
    for s in segs:
        if t < s.t0 + s.dur:
            local = (t - s.t0) / s.dur if s.dur > 0 else 1.0
            return s.p_from + (s.p_to - s.p_from) * min_jerk(local)
    return segs[-1].p_to


# Head translation safety envelope (metres). The sleep pose sits at z=-0.044;
# leave headroom for the overshoot + tremor without pushing the platform past
# its reach.
HEAD_Z_RANGE = (-0.075, 0.045)
HEAD_XY_RANGE = (-0.03, 0.03)

# Tremor (Ornstein-Uhlenbeck) parameters.
TREMOR_TAU = 0.085  # s, correlation time of the shake
TREMOR_ROT_SIGMA = 0.013  # rad (~0.75 deg) at tremor=1.0, full activity
TREMOR_TR_SIGMA = 0.0011  # m (~1.1 mm) at tremor=1.0, full activity
VEL_REF = 1.6  # progress/s that counts as "moving fast" (activity = 1)


def stream_to_pose(
    daemon: Daemon,
    target_head: list[list[float]],
    target_antennas: tuple[float, float],
    target_body_yaw: float,
    duration: float,
    hz: float,
    style: str,
    tremor_scale: float,
    rng: random.Random,
) -> None:
    # Read the starting state so the motion begins exactly where the robot is.
    start_flat = daemon.present_head_pose()
    start_yaw = daemon.present_body_yaw()
    start_ant = daemon.present_antennas()

    r0, t0 = flat_to_rot_trans(start_flat)
    q0 = mat3_to_quat(r0)
    t1 = [target_head[0][3], target_head[1][3], target_head[2][3]]
    q1 = mat3_to_quat([row[:3] for row in target_head[:3]])

    # --- Build the head progress timeline ---
    if style == "smooth":
        head_wp = [0.0, 1.0]
        head_dwell: set[int] = set()
        tremor_scale = 0.0
    else:

        def j(a: float) -> float:
            return rng.uniform(-a, a)

        head_wp = [
            0.0,
            0.45 + j(0.06),  # fast ballistic transport (undershoot)
            0.80 + j(0.05),  # second submovement
            1.06 + j(0.03),  # overshoot past the detent
            0.96 + j(0.02),  # corrective pull-back
            1.0,             # final seat
        ]
        head_dwell = {0, 2}  # regrip after the coarse reach and before seating
    head_segs, head_total = build_timeline(head_wp, duration, rng, head_dwell)

    # --- Antennas: delayed, quicker, springy, left lags right (the "fingers") ---
    ant_delay = duration * rng.uniform(0.12, 0.22)
    ant_dur = duration * rng.uniform(0.35, 0.50)
    ant_lag = duration * rng.uniform(0.06, 0.14)
    ant_wp = [0.0, 1.10, 0.96, 1.0] if style != "smooth" else [0.0, 1.0]
    ant_segs, ant_total = build_timeline(ant_wp, ant_dur, rng, set())

    def ant_progress(t: float, extra_delay: float = 0.0) -> float:
        u = t - ant_delay - extra_delay
        return sample_timeline(ant_segs, ant_total, u) if u > 0 else 0.0

    hold = 0.35  # keep sending the final target so it visibly settles
    total = max(head_total, ant_delay + ant_lag + ant_total) + hold

    period = 1.0 / hz
    frames = max(1, int(math.ceil(total / period)))
    print(
        f"Streaming '{style}' motion: {frames} frames over {total:.1f}s "
        f"@ {hz:.0f} Hz ({daemon.base}) ...",
        flush=True,
    )

    # Ornstein-Uhlenbeck tremor state: [rx, ry, rz, tx, ty, tz].
    a_ou = math.exp(-period / TREMOR_TAU)
    b_ou = math.sqrt(max(0.0, 1.0 - a_ou * a_ou))
    n = [0.0] * 6

    ignored_once = False
    dropped_frames = 0
    dropped_streak = 0
    p_prev = 0.0
    start_wall = time.perf_counter()
    for i in range(frames + 1):
        t = i * period

        p = sample_timeline(head_segs, head_total, t)
        vel = abs(p - p_prev) / period
        p_prev = p
        # Hand shakes more mid-transport, steadies at rest.
        activity = clamp(0.3 + vel / VEL_REF, 0.0, 1.4)

        for k in range(6):
            sigma = TREMOR_ROT_SIGMA if k < 3 else TREMOR_TR_SIGMA
            n[k] = n[k] * a_ou + b_ou * sigma * tremor_scale * activity * rng.gauss(0, 1)

        # Base pose along the start->target geodesic; p may exceed 1 (overshoot),
        # slerp/lerp extrapolate cleanly along the same arc.
        q_base = slerp(q0, q1, p)
        q = quat_mul(quat_from_euler(n[0], n[1], n[2]), q_base)
        r = quat_to_mat3(q)
        trans = [
            clamp(t0[0] + (t1[0] - t0[0]) * p + n[3], *HEAD_XY_RANGE),
            clamp(t0[1] + (t1[1] - t0[1]) * p + n[4], *HEAD_XY_RANGE),
            clamp(t0[2] + (t1[2] - t0[2]) * p + n[5], *HEAD_Z_RANGE),
        ]
        head_flat = pose_to_flat(r, trans)

        pr = ant_progress(t)
        pl = ant_progress(t, ant_lag)
        antennas = (
            start_ant[0] + (target_antennas[0] - start_ant[0]) * pr,
            start_ant[1] + (target_antennas[1] - start_ant[1]) * pl,
        )
        # Slight incidental body yaw from the grip, plus its own tiny wobble.
        body_yaw = start_yaw + (target_body_yaw - start_yaw) * p + 0.35 * n[2]

        # A single slow/lost frame must not kill the run: skip it and keep
        # streaming. The daemon holds the last target between frames anyway.
        try:
            res = daemon.set_target(head_flat, antennas, body_yaw, retries=0)
            dropped_streak = 0
        except OSError as e:
            res = None
            dropped_frames += 1
            dropped_streak += 1
            # Only give up if the daemon has been unreachable for a while.
            if dropped_streak > max(int(hz * 2), 20):
                print(
                    f"\n[error] daemon unresponsive for {dropped_streak} frames "
                    f"({e}); aborting.",
                    file=sys.stderr,
                )
                return

        if (
            isinstance(res, dict)
            and res.get("status") == "ignored"
            and not ignored_once
        ):
            ignored_once = True
            print(
                f"\n[warn] daemon ignored set_target ({res.get('reason')}). "
                "A move is probably running - stop it first.",
                file=sys.stderr,
            )

        if i % max(1, frames // 20) == 0 or i == frames:
            pct = int(100 * i / frames)
            sys.stdout.write(f"\r  progress: {pct:3d}%")
            sys.stdout.flush()

        # Pace against a wall clock so per-frame HTTP latency doesn't stretch the
        # motion out.
        target_wall = start_wall + (i + 1) * period
        sleep_for = target_wall - time.perf_counter()
        if sleep_for > 0:
            time.sleep(sleep_for)

    # Seat exactly on the target so it ends clean (no lingering tremor offset).
    try:
        daemon.set_target(
            pose_to_flat([row[:3] for row in target_head[:3]], t1),
            target_antennas,
            target_body_yaw,
        )
    except OSError:
        pass

    suffix = f" ({dropped_frames} frames dropped)" if dropped_frames else ""
    print(f"\nDone.{suffix}", flush=True)


def ensure_enabled(daemon: Daemon) -> None:
    """Make sure the head motors are torqued on, else set_target moves nothing.

    After a sleep/dodo the robot usually sits in 'disabled' (torque off), so a
    single set_target can't drive the head. We flip it back to 'enabled' first.
    """
    try:
        mode = daemon.motor_mode()
    except OSError:
        return
    if mode != "enabled":
        print(f"[info] motors were '{mode}', enabling them.", flush=True)
        daemon.set_motor_mode("enabled")
        time.sleep(0.2)  # let torque ramp before commanding


# Sleep pose decomposed (from SLEEP_HEAD_POSE) so we can perturb around it.
SLEEP_PITCH = math.atan2(SLEEP_HEAD_POSE[0][2], SLEEP_HEAD_POSE[0][0])  # ~0.427 rad
SLEEP_TRANS = [SLEEP_HEAD_POSE[0][3], SLEEP_HEAD_POSE[1][3], SLEEP_HEAD_POSE[2][3]]


def set_random_pose(daemon: Daemon, rng: random.Random) -> None:
    """One-shot: snap the robot to a near-dodo pose (no animation), for testing.

    Not a fully random pose: it starts from the sleep/dodo pose and nudges it a
    little - head raised a touch (a bit less pitch, slightly higher) and antennas
    a bit less folded, with small asymmetry - so it's a realistic "almost tucked
    in" starting point to test a placement run.
    """
    lift = rng.uniform(0.08, 0.22)  # raise the head: a bit less forward pitch
    pitch = SLEEP_PITCH - lift
    roll = rng.uniform(-0.05, 0.05)
    yaw = rng.uniform(-0.05, 0.05)
    trans = [
        clamp(SLEEP_TRANS[0] + rng.uniform(-0.005, 0.005), *HEAD_XY_RANGE),
        clamp(SLEEP_TRANS[1] + rng.uniform(-0.005, 0.005), *HEAD_XY_RANGE),
        clamp(SLEEP_TRANS[2] + rng.uniform(0.006, 0.020), *HEAD_Z_RANGE),  # raise
    ]
    r = quat_to_mat3(quat_from_euler(roll, pitch, yaw))
    head_flat = pose_to_flat(r, trans)

    # Antennas: from the fully-folded (-3.05, 3.05) sleep values, open them a bit.
    antennas = (
        SLEEP_ANTENNAS[0] + rng.uniform(0.2, 0.7),
        SLEEP_ANTENNAS[1] - rng.uniform(0.2, 0.7),
    )
    body_yaw = rng.uniform(-0.1, 0.1)

    res = daemon.set_target(head_flat, antennas, body_yaw)
    if isinstance(res, dict) and res.get("status") == "ignored":
        print(
            f"[warn] daemon ignored set_target ({res.get('reason')}). "
            "A move is probably running - stop it first.",
            file=sys.stderr,
        )
        return
    print(
        "Set near-dodo pose: "
        f"head lifted {math.degrees(lift):.0f} deg "
        f"(rpy=({roll:+.2f}, {pitch:+.2f}, {yaw:+.2f}) rad), "
        f"z={trans[2]:+.3f} m, "
        f"antennas=({antennas[0]:+.2f}, {antennas[1]:+.2f}), "
        f"body_yaw={body_yaw:+.2f}",
        flush=True,
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1", help="daemon host")
    parser.add_argument("--port", type=int, default=8000, help="daemon port")
    parser.add_argument(
        "--duration",
        type=float,
        default=2.5,
        help="approximate motion duration in seconds (human style adds pauses)",
    )
    parser.add_argument("--hz", type=float, default=50.0, help="stream rate (Hz)")
    parser.add_argument(
        "--style",
        choices=["human", "smooth"],
        default="human",
        help="'human' = hand-placement look (default), 'smooth' = min-jerk glide",
    )
    parser.add_argument(
        "--tremor",
        type=float,
        default=1.0,
        help="hand-tremor scale (0 = none, 1 = default, >1 = shakier)",
    )
    parser.add_argument(
        "--seed", type=int, default=None, help="RNG seed for reproducible motion"
    )
    parser.add_argument(
        "--wake",
        action="store_true",
        help="place the robot into the neutral/init pose instead of sleep",
    )
    parser.add_argument(
        "--random",
        action="store_true",
        help="one-shot: snap to a near-dodo pose (head slightly raised, antennas "
        "slightly opened; no animation), for testing",
    )
    parser.add_argument(
        "--keep-mode",
        action="store_true",
        help="don't auto-enable motors first (leave the current control mode)",
    )
    args = parser.parse_args()

    rng = random.Random(args.seed)

    # `with` guarantees the keep-alive socket to the daemon is closed on every
    # exit path (success, error, or Ctrl-C).
    with Daemon(args.host, args.port) as daemon:
        try:
            if not args.keep_mode:
                ensure_enabled(daemon)

            if args.random:
                set_random_pose(daemon, rng)
                return 0

            if args.wake:
                target_head, target_ant, label = INIT_HEAD_POSE, INIT_ANTENNAS, "init/wake"
            else:
                target_head, target_ant, label = SLEEP_HEAD_POSE, SLEEP_ANTENNAS, "sleep/dodo"

            print(f"Target pose: {label} | style: {args.style}", flush=True)
            stream_to_pose(
                daemon,
                target_head=target_head,
                target_antennas=target_ant,
                target_body_yaw=0.0,
                duration=args.duration,
                hz=args.hz,
                style=args.style,
                tremor_scale=max(0.0, args.tremor),
                rng=rng,
            )
        except OSError as e:
            print(
                f"[error] cannot reach daemon at {daemon.base}: {e}\n"
                "Is the tray / daemon running?",
                file=sys.stderr,
            )
            return 1
        except KeyboardInterrupt:
            print("\nInterrupted.", file=sys.stderr)
            return 130
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

// 3D viewer for the tray logs window.
//
// Vanilla ESM module. Loads three.js + urdf-loader from esm.sh so the
// tray frontend stays bundler-free (matches the rest of `ui/`).
//
// Lifecycle:
//   - init(mountEl)         creates the scene, kicks off URDF load + WS open.
//   - setVisible(true|false) pauses rAF and WS reconnects when hidden.
//   - dispose()              releases GPU, closes WS (called on window unload).
//
// Data flow:
//   ws://127.0.0.1:8000/api/state/ws/full @ 20 Hz
//     -> latestState  (yaw_body, stewart_1..6, antennas, head_pose.m)
//     -> applied to URDF joints inside the rAF loop.

import * as THREE from "https://esm.sh/three@0.160.0";
import { OrbitControls } from "https://esm.sh/three@0.160.0/examples/jsm/controls/OrbitControls.js";
import URDFLoader from "https://esm.sh/urdf-loader@0.12.6?deps=three@0.160.0";
import initKinematicsWasm, {
  calculate_passive_joints,
} from "./kinematics/reachy_mini_kinematics_wasm.js";

const URDF_URL = new URL("./robot-3d/reachy-mini.urdf", import.meta.url).href;
const MESH_BASE = new URL("./robot-3d/meshes/", import.meta.url).href;

const DAEMON_HOST = "127.0.0.1:8000";
const WS_PARAMS = new URLSearchParams({
  frequency: "20",
  with_head_joints: "true",
  with_body_yaw: "true",
  with_antenna_positions: "true",
  use_pose_matrix: "true",
});
const WS_URL = `ws://${DAEMON_HOST}/api/state/ws/full?${WS_PARAMS}`;

// Backoff: 1s, 2s, 4s, ... capped at 30s with ±20% jitter.
const WS_BACKOFF_INITIAL_MS = 1000;
const WS_BACKOFF_MAX_MS = 30000;
const WS_BACKOFF_FACTOR = 2;
const WS_JITTER = 0.2;

const STEWART_JOINT_NAMES = [
  "stewart_1",
  "stewart_2",
  "stewart_3",
  "stewart_4",
  "stewart_5",
  "stewart_6",
];

// Order MUST match the Rust WASM `calculate_passive_joints` output: the 21
// floats are returned as [p1_x, p1_y, p1_z, p2_x, ..., p7_z] with the same
// axis convention as the URDF's `passive_*_{x|y|z}` joint names. The
// desktop app uses the exact same list (see `constants/robotBuffer.ts`).
const PASSIVE_JOINT_NAMES = [
  "passive_1_x", "passive_1_y", "passive_1_z",
  "passive_2_x", "passive_2_y", "passive_2_z",
  "passive_3_x", "passive_3_y", "passive_3_z",
  "passive_4_x", "passive_4_y", "passive_4_z",
  "passive_5_x", "passive_5_y", "passive_5_z",
  "passive_6_x", "passive_6_y", "passive_6_z",
  "passive_7_x", "passive_7_y", "passive_7_z",
];

// Camera preset matches the desktop app's `normal` preset.
const CAMERA = {
  position: [-0.25, 0.35, 0.55],
  target: [0, 0.2, 0],
  fov: 50,
  minDistance: 0.15,
  maxDistance: 0.8,
};

// Match the desktop viewer's lighting roughly, in a simpler form.
const LIGHTING = {
  ambient: 0.45,
  key: 1.6,
  fill: 0.35,
  rim: 0.7,
  rimColor: 0xffb366,
};

const COLORS_LIGHT = {
  bg: 0xe7e7eb,
  fog: 0xe7e7eb,
  gridMajor: 0x999999,
  gridMinor: 0xcccccc,
};
const COLORS_DARK = {
  bg: 0x16181c,
  fog: 0x16181c,
  gridMajor: 0x555555,
  gridMinor: 0x333333,
};

function isDarkMode() {
  return window.matchMedia?.("(prefers-color-scheme: dark)").matches ?? false;
}

function computeReconnectDelay(attempt) {
  const raw = WS_BACKOFF_INITIAL_MS * Math.pow(WS_BACKOFF_FACTOR, attempt);
  const capped = Math.min(raw, WS_BACKOFF_MAX_MS);
  const jitter = capped * WS_JITTER * (Math.random() * 2 - 1);
  return Math.max(WS_BACKOFF_INITIAL_MS, Math.floor(capped + jitter));
}

class Reachy3DViewer {
  constructor() {
    this.scene = null;
    this.camera = null;
    this.renderer = null;
    this.controls = null;
    this.fog = null;
    this.grid = null;
    this.robot = null; // URDFRobot once loaded
    this.mountEl = null;
    this.resizeObserver = null;
    this.themeMql = null;
    this.themeListener = null;
    this.visible = true;
    this.disposed = false;
    this.wireframe = false;

    // WS state
    this.ws = null;
    this.wsReconnectTimer = null;
    this.wsReconnectAttempt = 0;
    this.wsClosedByUs = false;

    // Latest state from the daemon. Read inside the rAF tick.
    this.latest = {
      headJoints: null,
      antennas: null,
      bodyYaw: null,
      headPose: null, // 16-float row-major 4x4 matrix from `head_pose.m`
      lastMessageAt: 0, // performance.now() of the most recent WS message
    };

    // WASM passive-joints computation (the daemon doesn't provide them
    // under --mockup-sim and with the AnalyticalKinematics backend).
    this.wasmReady = false;
    // Reusable typed-array buffers so we don't allocate per frame.
    this._headJointsBuf = new Float64Array(7);
    this._headPoseBuf = new Float64Array(16);

    // External hooks.
    this.onStatusChange = null;
    this.onStats = null;
    this.status = "connecting"; // 'connecting' | 'loading' | 'live' | 'offline' | 'error'

    // Live stats exposed via onStats(stats):
    //   fps      : render frames per second (rolling 1s)
    //   wsHz     : WS messages per second (rolling 1s)
    //   lastAgeMs: ms since last WS message (Infinity if none yet)
    //   peerId   : daemon's central peer id (set once on boot)
    //   robotName: daemon's central robot name (set once on boot)
    this._frameTimes = [];
    this._msgTimes = [];
    this._lastStatsAt = 0;
    this._stats = {
      fps: 0,
      wsHz: 0,
      lastAgeMs: Infinity,
      peerId: null,
      robotName: null,
    };

    this._tick = this._tick.bind(this);
  }

  async init(mountEl, { onStatusChange, onStats } = {}) {
    if (this.disposed) return;
    this.mountEl = mountEl;
    this.onStatusChange = onStatusChange ?? null;
    this.onStats = onStats ?? null;

    const palette = isDarkMode() ? COLORS_DARK : COLORS_LIGHT;

    this.scene = new THREE.Scene();
    this.scene.background = new THREE.Color(palette.bg);
    this.fog = new THREE.Fog(palette.fog, 1, 2.5);
    this.scene.fog = this.fog;

    const { width, height } = mountEl.getBoundingClientRect();
    this.camera = new THREE.PerspectiveCamera(
      CAMERA.fov,
      Math.max(width, 1) / Math.max(height, 1),
      0.05,
      100,
    );
    this.camera.position.set(...CAMERA.position);

    this.renderer = new THREE.WebGLRenderer({
      antialias: true,
      alpha: false,
      powerPreference: "high-performance",
      // Required so screenshot()'s `canvas.toDataURL()` returns a valid
      // image instead of a transparent / black frame.
      preserveDrawingBuffer: true,
    });
    this.renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
    this.renderer.setSize(Math.max(width, 1), Math.max(height, 1), false);
    this.renderer.outputColorSpace = THREE.SRGBColorSpace;
    this.renderer.toneMapping = THREE.ACESFilmicToneMapping;
    this.renderer.toneMappingExposure = 1.0;
    mountEl.appendChild(this.renderer.domElement);

    this.scene.add(new THREE.AmbientLight(0xffffff, LIGHTING.ambient));

    const key = new THREE.DirectionalLight(0xffffff, LIGHTING.key);
    key.position.set(2, 4, 2);
    this.scene.add(key);

    const fill = new THREE.DirectionalLight(0xffffff, LIGHTING.fill);
    fill.position.set(-2, 2, 1.5);
    this.scene.add(fill);

    const rim = new THREE.DirectionalLight(LIGHTING.rimColor, LIGHTING.rim);
    rim.position.set(0, 3, -2);
    this.scene.add(rim);

    this._buildGrid(palette);

    this.controls = new OrbitControls(this.camera, this.renderer.domElement);
    this.controls.enablePan = false;
    this.controls.enableDamping = true;
    this.controls.dampingFactor = 0.05;
    this.controls.target.set(...CAMERA.target);
    this.controls.minDistance = CAMERA.minDistance;
    this.controls.maxDistance = CAMERA.maxDistance;
    this.controls.update();

    this.resizeObserver = new ResizeObserver(() => this._handleResize());
    this.resizeObserver.observe(mountEl);

    this.themeMql = window.matchMedia("(prefers-color-scheme: dark)");
    this.themeListener = () => this._applyPalette(isDarkMode() ? COLORS_DARK : COLORS_LIGHT);
    this.themeMql.addEventListener("change", this.themeListener);

    requestAnimationFrame(this._tick);

    this._setStatus("loading");
    try {
      await this._loadURDF();
    } catch (err) {
      console.error("[viewer3d] URDF load failed", err);
      this._setStatus("error");
      return;
    }

    // Kick the WASM kinematics in parallel with the WS open. If it fails
    // we still render: the head will move, only the Stewart legs (passive
    // joints) won't follow. Non-fatal.
    initKinematicsWasm()
      .then(() => {
        this.wasmReady = true;
        console.info("[viewer3d] kinematics WASM ready");
      })
      .catch((err) => {
        console.warn("[viewer3d] kinematics WASM init failed", err);
      });

    // One-shot fetch for the central peer id (UI footer). Non-fatal: a
    // missing or unreachable endpoint just leaves the footer blank.
    this._fetchPeerInfo();

    this._setStatus("connecting");
    this._connectWebSocket();
  }

  async _fetchPeerInfo() {
    try {
      const resp = await fetch(`http://${DAEMON_HOST}/api/hf-auth/central-robot-status`);
      if (!resp.ok) return;
      const data = await resp.json();
      const robot = Array.isArray(data?.robots) ? data.robots[0] : null;
      if (!robot) return;
      this._stats.peerId = robot.peerId || robot.peer_id || null;
      this._stats.robotName = robot.robotName || robot.meta?.name || null;
      this._emitStats(true);
    } catch (err) {
      // Best-effort; not surfaced anywhere.
    }
  }

  _buildGrid(palette) {
    if (this.grid) {
      this.scene.remove(this.grid);
      this.grid.geometry.dispose();
      this.grid.material.dispose();
    }
    const grid = new THREE.GridHelper(2, 20, palette.gridMajor, palette.gridMinor);
    grid.material.opacity = 0.5;
    grid.material.transparent = true;
    grid.material.fog = true;
    this.scene.add(grid);
    this.grid = grid;
  }

  _applyPalette(palette) {
    if (!this.scene) return;
    this.scene.background = new THREE.Color(palette.bg);
    this.fog.color = new THREE.Color(palette.fog);
    this._buildGrid(palette);
  }

  async _loadURDF() {
    console.info("[viewer3d] URDF_URL =", URDF_URL);
    console.info("[viewer3d] MESH_BASE =", MESH_BASE);

    // Sanity-check the URDF is reachable before delegating to urdf-loader
    // (which fails with an opaque "undefined.children" if the response is
    // not parseable XML, e.g. a 404 page).
    const probe = await fetch(URDF_URL);
    if (!probe.ok) {
      throw new Error(`URDF fetch HTTP ${probe.status} at ${URDF_URL}`);
    }
    const text = await probe.text();
    if (!text.includes("<robot")) {
      throw new Error(`URDF at ${URDF_URL} does not look like XML (got: ${text.slice(0, 80)}…)`);
    }
    console.info("[viewer3d] URDF fetched OK,", text.length, "chars");

    return new Promise((resolve, reject) => {
      const loader = new URDFLoader();

      // Resolve `meshes/foo.stl` paths from the URDF against our ui/ folder.
      // urdf-loader's default STL handler returns a `THREE.Mesh` with a
      // `MeshPhongMaterial`; that material is then replaced by the URDF
      // `<material>` if one is declared. Good enough for the v1 viewer.
      loader.manager.setURLModifier((url) => {
        const filename = url.split("/").pop();
        const resolved = `${MESH_BASE}${filename}`;
        return resolved;
      });

      // Parse the URDF we already fetched, so we control workingPath. The
      // workingPath is prepended to every mesh filename in `parse()`; we
      // pass an empty string so the URL-modifier above is the only thing
      // that rewrites mesh URLs.
      let robot;
      try {
        robot = loader.parse(text, "");
      } catch (err) {
        reject(new Error(`URDF parse failed: ${err.message || err}`));
        return;
      }

      // Wait one tick so STLs have a chance to be queued, then resolve.
      // Mesh loading is async; we don't block on it because the scene
      // renders fine while STLs trickle in.
      // Match the desktop orientation: rotate so the robot stands on the
      // ground plane and faces the camera.
      const group = new THREE.Group();
      group.add(robot);
      robot.rotation.set(-Math.PI / 2, 0, 0);
      group.rotation.set(0, -Math.PI / 2, 0);
      this.scene.add(group);

      this.robot = robot;
      this._applyJoints({ headJoints: [0, 0, 0, 0, 0, 0, 0], antennas: [0, 0] });

      // Light material polish once all STLs are loaded. Also re-apply
      // wireframe if the user toggled it before the meshes finished
      // streaming in.
      const onAllLoaded = () => {
        robot.traverse((child) => {
          if (child.isMesh && child.material) {
            child.material.flatShading = true;
            if (this.wireframe) child.material.wireframe = true;
            child.material.needsUpdate = true;
          }
        });
      };
      if (loader.manager.onLoad) {
        const prev = loader.manager.onLoad;
        loader.manager.onLoad = () => { try { prev(); } catch {} onAllLoaded(); };
      } else {
        loader.manager.onLoad = onAllLoaded;
      }

      resolve(robot);
    });
  }

  _connectWebSocket() {
    if (this.disposed) return;
    if (this.wsReconnectTimer) {
      clearTimeout(this.wsReconnectTimer);
      this.wsReconnectTimer = null;
    }
    this.wsClosedByUs = false;

    let ws;
    try {
      ws = new WebSocket(WS_URL);
    } catch (err) {
      console.warn("[viewer3d] ws construct failed", err);
      this._scheduleReconnect();
      return;
    }
    this.ws = ws;

    ws.addEventListener("open", () => {
      this.wsReconnectAttempt = 0;
      this._setStatus("live");
    });

    ws.addEventListener("message", (evt) => {
      let data;
      try {
        data = JSON.parse(evt.data);
      } catch {
        return;
      }
      this.latest.lastMessageAt = performance.now();
      this._msgTimes.push(this.latest.lastMessageAt);
      // Daemon publishes head_joints as `null` when no hardware (e.g.
      // --mockup-sim) - skip in that case rather than zeroing the model.
      if (Array.isArray(data.head_joints) && data.head_joints.length === 7) {
        this.latest.headJoints = data.head_joints;
      }
      if (Array.isArray(data.antennas_position) && data.antennas_position.length === 2) {
        this.latest.antennas = data.antennas_position;
      }
      if (typeof data.body_yaw === "number") {
        this.latest.bodyYaw = data.body_yaw;
      }
      // `head_pose.m` is a 16-float row-major 4x4 matrix when the WS was
      // opened with `use_pose_matrix=true`. Required by the WASM
      // kinematics to compute the 21 passive joints.
      if (data.head_pose && Array.isArray(data.head_pose.m) && data.head_pose.m.length === 16) {
        this.latest.headPose = data.head_pose.m;
      }
    });

    ws.addEventListener("close", () => {
      this.ws = null;
      if (this.wsClosedByUs || this.disposed) return;
      this._setStatus("offline");
      this._scheduleReconnect();
    });

    ws.addEventListener("error", () => {
      // Errors are followed by 'close', so backoff is handled there.
      this._setStatus("offline");
    });
  }

  _scheduleReconnect() {
    if (this.disposed) return;
    const delay = computeReconnectDelay(this.wsReconnectAttempt);
    this.wsReconnectAttempt += 1;
    this.wsReconnectTimer = setTimeout(() => this._connectWebSocket(), delay);
  }

  _setStatus(status) {
    if (this.status === status) return;
    this.status = status;
    this.onStatusChange?.(status);
  }

  _applyJoints({ headJoints, antennas, passiveJoints }) {
    if (!this.robot || !this.robot.joints) return;
    const joints = this.robot.joints;
    if (headJoints) {
      if (joints["yaw_body"]) {
        this.robot.setJointValue("yaw_body", headJoints[0]);
      }
      for (let i = 0; i < STEWART_JOINT_NAMES.length; i++) {
        const name = STEWART_JOINT_NAMES[i];
        if (joints[name]) {
          this.robot.setJointValue(name, headJoints[i + 1]);
        }
      }
    }
    if (antennas) {
      // Match the desktop convention: left/right are inverted and sign-flipped.
      if (joints["left_antenna"]) {
        this.robot.setJointValue("left_antenna", -antennas[1]);
      }
      if (joints["right_antenna"]) {
        this.robot.setJointValue("right_antenna", -antennas[0]);
      }
    }
    if (passiveJoints && passiveJoints.length >= PASSIVE_JOINT_NAMES.length) {
      for (let i = 0; i < PASSIVE_JOINT_NAMES.length; i++) {
        const name = PASSIVE_JOINT_NAMES[i];
        if (joints[name]) {
          this.robot.setJointValue(name, passiveJoints[i]);
        }
      }
    }
  }

  /**
   * Calculate the 21 passive joint angles for the Stewart platform from
   * the current head pose, using the Rust-compiled WASM kinematics. The
   * daemon doesn't ship these in --mockup-sim mode (and not in USB mode
   * with AnalyticalKinematics either), so we do it client-side; takes
   * < 1ms per call.
   *
   * Returns `null` when WASM isn't ready, or when the upstream state is
   * incomplete (no headJoints + headPose yet).
   */
  _computePassiveJoints() {
    if (!this.wasmReady) return null;
    const hj = this.latest.headJoints;
    const hp = this.latest.headPose;
    if (!hj || hj.length !== 7 || !hp || hp.length !== 16) return null;
    for (let i = 0; i < 7; i++) this._headJointsBuf[i] = hj[i];
    for (let i = 0; i < 16; i++) this._headPoseBuf[i] = hp[i];
    try {
      return calculate_passive_joints(this._headJointsBuf, this._headPoseBuf);
    } catch (err) {
      console.warn("[viewer3d] passive_joints compute failed", err);
      return null;
    }
  }

  _tick() {
    if (this.disposed) return;
    requestAnimationFrame(this._tick);
    if (!this.visible) return;

    const now = performance.now();
    this._frameTimes.push(now);

    if (this.latest.headJoints || this.latest.antennas) {
      const passiveJoints = this._computePassiveJoints();
      this._applyJoints({
        headJoints: this.latest.headJoints,
        antennas: this.latest.antennas,
        passiveJoints,
      });
    }
    this.controls?.update();
    this.renderer?.render(this.scene, this.camera);

    // Emit stats once every ~250 ms so the DOM isn't thrashed at 60 Hz.
    if (now - this._lastStatsAt >= 250) {
      this._lastStatsAt = now;
      this._emitStats(false);
    }
  }

  /**
   * Roll the two timing windows forward and emit the latest stats. Cheap
   * enough to call from the rAF loop a few times per second.
   *
   * @param {boolean} immediate - skip rate-limiting / window trim (used by
   *   the one-shot peer-info fetch so the footer updates as soon as we
   *   have the robot name).
   */
  _emitStats(immediate) {
    const now = performance.now();
    const cutoff = now - 1000; // 1-second rolling window

    while (this._frameTimes.length && this._frameTimes[0] < cutoff) this._frameTimes.shift();
    while (this._msgTimes.length && this._msgTimes[0] < cutoff) this._msgTimes.shift();

    this._stats.fps = this._frameTimes.length;
    this._stats.wsHz = this._msgTimes.length;
    this._stats.lastAgeMs = this.latest.lastMessageAt
      ? now - this.latest.lastMessageAt
      : Infinity;

    this.onStats?.(this._stats);
  }

  _handleResize() {
    if (!this.renderer || !this.mountEl) return;
    const { width, height } = this.mountEl.getBoundingClientRect();
    if (width <= 0 || height <= 0) return;
    this.renderer.setSize(width, height, false);
    this.camera.aspect = width / Math.max(height, 1);
    this.camera.updateProjectionMatrix();
  }

  setVisible(visible) {
    this.visible = !!visible;
    if (this.visible) {
      this._handleResize();
    }
  }

  /** Restore the camera to its default preset (position + target). */
  resetCamera() {
    if (!this.camera || !this.controls) return;
    this.camera.position.set(...CAMERA.position);
    this.controls.target.set(...CAMERA.target);
    this.controls.update();
  }

  /**
   * Toggle wireframe rendering for every mesh in the robot model.
   * Returns the new state so the UI can sync its button.
   */
  toggleWireframe() {
    this.wireframe = !this.wireframe;
    if (this.robot) {
      this.robot.traverse((child) => {
        if (child.isMesh && child.material) {
          if (Array.isArray(child.material)) {
            child.material.forEach((m) => { m.wireframe = this.wireframe; });
          } else {
            child.material.wireframe = this.wireframe;
          }
        }
      });
    }
    return this.wireframe;
  }

  /**
   * Trigger a download of the current viewport as a PNG. Relies on the
   * canvas being created with `preserveDrawingBuffer: true` so the read
   * is valid after rendering.
   */
  screenshot() {
    if (!this.renderer) return;
    // Force a render so the buffer is up-to-date even if the rAF tick
    // hasn't run since the last camera move.
    this.renderer.render(this.scene, this.camera);
    const dataUrl = this.renderer.domElement.toDataURL("image/png");
    const ts = new Date().toISOString().replace(/[:.]/g, "-");
    const a = document.createElement("a");
    a.href = dataUrl;
    a.download = `reachy-mini-${ts}.png`;
    document.body.appendChild(a);
    a.click();
    a.remove();
  }

  dispose() {
    if (this.disposed) return;
    this.disposed = true;
    this.wsClosedByUs = true;
    if (this.wsReconnectTimer) {
      clearTimeout(this.wsReconnectTimer);
      this.wsReconnectTimer = null;
    }
    if (this.ws) {
      try { this.ws.close(); } catch {}
      this.ws = null;
    }
    if (this.themeMql && this.themeListener) {
      this.themeMql.removeEventListener("change", this.themeListener);
    }
    if (this.resizeObserver) {
      this.resizeObserver.disconnect();
      this.resizeObserver = null;
    }
    if (this.controls) {
      this.controls.dispose();
      this.controls = null;
    }
    if (this.scene) {
      this.scene.traverse((obj) => {
        if (obj.isMesh) {
          obj.geometry?.dispose?.();
          if (Array.isArray(obj.material)) {
            obj.material.forEach((m) => m.dispose?.());
          } else {
            obj.material?.dispose?.();
          }
        }
      });
    }
    if (this.renderer) {
      this.renderer.dispose();
      this.renderer.domElement?.remove();
      this.renderer = null;
    }
    this.scene = null;
    this.camera = null;
    this.robot = null;
  }
}

export function createViewer() {
  return new Reachy3DViewer();
}

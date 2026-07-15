// 3D viewer for the tray logs window.
//
 // Vanilla ESM module. Loads three.js + its GLTF/Draco loaders from esm.sh so
// the tray frontend stays bundler-free (matches the rest of `ui/`).
//
// The robot is the rigged, Draco-compressed glb shared with the mobile and
// desktop apps. This module is a straight vanilla-JS port of the mobile
// `ReachyModel.tsx` / `ReachyViz.tsx` (React Three Fiber): same asset, same
// calibration constants, same bone animation (body yaw fix + head 6-DOF pose +
// antennas + CCD neck IK), so the tray viz matches the mobile viz exactly.
//
// Lifecycle:
//   - init(mountEl)          creates the scene, loads the glb + opens the WS.
//   - setVisible(true|false) pauses rAF when hidden.
//   - dispose()              releases GPU, closes WS (called on window unload).
//
// Data flow:
//   ws://127.0.0.1:8000/api/state/ws/full @ 20 Hz
//     -> latest {headPose (4x4 matrix), bodyYaw, antennas}
//     -> smoothed + applied to the glb bones inside the rAF loop.

import * as THREE from "https://esm.sh/three@0.181.0";
import { OrbitControls } from "https://esm.sh/three@0.181.0/examples/jsm/controls/OrbitControls.js";
import { GLTFLoader } from "https://esm.sh/three@0.181.0/examples/jsm/loaders/GLTFLoader.js";
import { DRACOLoader } from "https://esm.sh/three@0.181.0/examples/jsm/loaders/DRACOLoader.js";

const GLB_URL = new URL("./robot-3d/reachy_mini_viz.glb", import.meta.url).href;
// Draco decoder vendored locally (the tray ships offline, so we can't rely on
// three's default CDN decoder path).
const DRACO_PATH = new URL("./draco/", import.meta.url).href;

const DAEMON_HOST = "127.0.0.1:8000";
const WS_PARAMS = new URLSearchParams({
  frequency: "20",
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

// Staleness watchdog: a "live" socket can go silently half-open (daemon
// restarted, USB/motor-comm hiccup) without ever firing `close`/`error`. When
// that happens the 3D model freezes on the last pose while the robot keeps
// moving -> the viz looks disconnected from the real state. At 20 Hz a gap this
// long (~40 missed frames) means the feed is dead; drop the socket so the
// normal backoff reconnect kicks in, and flip the status off `live` so the UI
// reflects reality.
const WS_STALE_TIMEOUT_MS = 2000;

// ============================================================================
// Model calibration - ported verbatim from mobile `ReachyModel.tsx`. The glb is
// exported Z-up (the robot's native frame), so head_pose applies directly.
// ============================================================================
// The glb was modeled ~3x real scale; bring it to the URDF's metres.
const MODEL_SCALE = 0.5;
// Yaw to align the glb's forward with the URDF facing.
const DISPLAY_YAW = -Math.PI;
// head_pose translation (metres) -> model units.
const UNITS_PER_M = 1.7;
// Platform head-Z reach measured from the rig (model units, rel. to rest).
const HEAD_Z_MAX = 0.044;
const HEAD_Z_MIN = -0.085;
// Robot frame (X-fwd, Y-left, Z-up) -> glb model frame: proper change of basis
// R_model = M*R*M^-1 (det +1, no reflection), M = Rz(+90).
const HEAD_FRAME_YAW_OFFSET = Math.PI / 2;
const HEAD_FIX = new THREE.Quaternion().setFromAxisAngle(
  new THREE.Vector3(0, 0, 1),
  HEAD_FRAME_YAW_OFFSET,
);
const HEAD_FIX_INV = HEAD_FIX.clone().invert();
const YAW_SIGN = 1;
const ANT_AXIS = new THREE.Vector3(0, 0, 1); // antenna hinge axis in glb space
const ANT_SIGN = -1;

// The two flexible antennas are the only skinned meshes in the glb; they use a
// near-black, highly-metallic material ('Metal.Black'). With no environment map
// a metallic surface renders almost pure black, so in dark mode the antennas
// vanish against the dark background. In dark mode we tint them a mid grey and
// drop the metalness so the grey actually shows (metallic materials suppress
// diffuse color). Ported from mobile ReachyModel.tsx.
const DARK_ANTENNA_COLOR = "#8f8f8f";
const DARK_ANTENNA_METALNESS = 0.2;

// Exponential smoothing rate (1/s). Higher = snappier, lower = smoother.
const SMOOTH_K = 15;

// Bone node names (verified present in the exported glb).
const BONE = {
  body: "Core",
  head: "Core.001",
  antL: "Antenna.L.002",
  antR: "Antenna.R.002",
};

// Camera preset. Pulled back a bit from the mobile framing (which sits ~20%
// closer) so the robot doesn't fill the whole canvas in the tray's logs pane.
const CAMERA = {
  position: [-0.29, 0.44, 0.64],
  target: [0, 0.2, 0],
  fov: 50,
  near: 0.01,
  far: 50,
  minDistance: 0.12,
  maxDistance: 2.0,
};

// Match the mobile viewer's lighting.
const LIGHTING = {
  ambient: 0.6,
  key: 1.6,
  fill: 0.4,
  rim: 0.7,
  rimColor: 0xffb366,
};

// The 3D viewer always renders on a white background, independent of the OS
// colour scheme, so the robot reads consistently.
const COLORS_LIGHT = {
  bg: 0xffffff,
  fog: 0xffffff,
  gridMajor: 0x999999,
  gridMinor: 0xcccccc,
};

// Extra beat after the first valid pose before revealing the viz, so the
// smoothing has settled and we never show the robot mid-snap.
const REVEAL_SETTLE_MS = 500;
// Fallback: reveal anyway this long after the WS goes live even if no valid
// head pose ever arrives (e.g. --mockup-sim with a null head_pose), so the
// viewer never stays permanently blank.
const REVEAL_FALLBACK_MS = 2500;

function computeReconnectDelay(attempt) {
  const raw = WS_BACKOFF_INITIAL_MS * Math.pow(WS_BACKOFF_FACTOR, attempt);
  const capped = Math.min(raw, WS_BACKOFF_MAX_MS);
  const jitter = capped * WS_JITTER * (Math.random() * 2 - 1);
  return Math.max(WS_BACKOFF_INITIAL_MS, Math.floor(capped + jitter));
}

// GLTFLoader sanitizes '.' out of node names ("Core.001" -> "Core001"),
// so look bones up by a normalized key.
const norm = (s) => s.replace(/[^a-z0-9]/gi, "").toLowerCase();

function toMatrix(p) {
  if (!p || p.length !== 16) return null;
  // row-major (robot) -> three is column-major, so transpose.
  return new THREE.Matrix4().fromArray(p).transpose();
}

// --- Neck leg IK -----------------------------------------------------------
// Each leg is a 4-bone chain that IK-solves in Blender so its tip reaches a
// target riding on the head. glTF can't carry that, so we re-solve it here with
// a small CCD pass.
const LEG_IDS = ["A", "B", "C", "D", "E", "F"];
const LEG_IK_ITERATIONS = 12;
// Per-bone degree of freedom: index 0..3 along the chain.
const LEG_DOF = ["locked", "hingeX", "locked", "free"];
const HINGE_AXIS = new THREE.Vector3(1, 0, 0);

function setupLeg(id, byName, head) {
  const chain = ["001", "002", "003", "004"].map((s) => byName[norm(`Neck.${id}.${s}`)]);
  const ikTarget = byName[norm(`Neck.loc.IK.${id}`)];
  if (chain.some((b) => !b) || !ikTarget) {
    console.warn(`[viewer3d] leg ${id}: missing bones, IK skipped`);
    return null;
  }
  const attach = ikTarget.getWorldPosition(new THREE.Vector3());
  const effector = new THREE.Object3D();
  chain[3].add(effector);
  effector.position.copy(chain[3].worldToLocal(attach.clone()));
  const target = new THREE.Object3D();
  head.add(target);
  target.position.copy(head.worldToLocal(attach.clone()));
  const restQ = chain.map((b) => b.quaternion.clone());
  return { chain, restQ, effector, target };
}

// CCD scratch (module-level, no per-frame allocation). Safe because a single
// viewer instance is on screen at a time and solveLeg runs synchronously.
const _jp = new THREE.Vector3();
const _ep = new THREE.Vector3();
const _tp = new THREE.Vector3();
const _v1 = new THREE.Vector3();
const _v2 = new THREE.Vector3();
const _hw = new THREE.Vector3();
const _cx = new THREE.Vector3();
const _qr = new THREE.Quaternion();
const _qw = new THREE.Quaternion();
const _qp = new THREE.Quaternion();

function solveLeg(leg, iterations) {
  for (let i = 0; i < leg.chain.length; i++) leg.chain[i].quaternion.copy(leg.restQ[i]);
  leg.chain[0].updateWorldMatrix(false, true);
  leg.target.getWorldPosition(_tp);
  for (let it = 0; it < iterations; it++) {
    for (let i = 0; i < leg.chain.length; i++) {
      const dof = LEG_DOF[i];
      if (dof === "locked") continue;
      const joint = leg.chain[i];
      leg.effector.getWorldPosition(_ep);
      if (_ep.distanceToSquared(_tp) < 1e-8) return;
      joint.getWorldPosition(_jp);
      joint.getWorldQuaternion(_qw);
      if (dof === "free") {
        _v1.subVectors(_ep, _jp).normalize();
        _v2.subVectors(_tp, _jp).normalize();
        _qr.setFromUnitVectors(_v1, _v2);
      } else {
        _hw.copy(HINGE_AXIS).applyQuaternion(_qw).normalize();
        _v1.subVectors(_ep, _jp).projectOnPlane(_hw);
        _v2.subVectors(_tp, _jp).projectOnPlane(_hw);
        if (_v1.lengthSq() < 1e-12 || _v2.lengthSq() < 1e-12) continue;
        _v1.normalize();
        _v2.normalize();
        const ang = Math.atan2(_cx.crossVectors(_v1, _v2).dot(_hw), _v1.dot(_v2));
        _qr.setFromAxisAngle(_hw, ang);
      }
      _qr.multiply(_qw);
      joint.parent?.getWorldQuaternion(_qp);
      joint.quaternion.copy(_qp.invert().multiply(_qr));
      joint.updateWorldMatrix(false, true);
    }
  }
}

class Reachy3DViewer {
  constructor() {
    this.scene = null;
    this.camera = null;
    this.renderer = null;
    this.controls = null;
    this.fog = null;
    this.grid = null;
    this.model = null; // glb scene root once loaded
    this.mountEl = null;
    this.resizeObserver = null;
    this.themeMql = null;
    this.themeListener = null;
    this.visible = true;
    this.disposed = false;

    // WS state
    this.ws = null;
    this.wsReconnectTimer = null;
    this.wsReconnectAttempt = 0;
    this.wsClosedByUs = false;

    // Latest state from the daemon. Read inside the rAF tick.
    this.latest = {
      antennas: null, // [rightRad, leftRad]
      bodyYaw: null, // radians
      headPose: null, // 16-float row-major 4x4 matrix from `head_pose.m`
      lastMessageAt: 0, // performance.now() of the most recent WS message
    };

    // --- Rigged-model animation state (mirrors mobile ReachyModel refs) ---
    this.bones = {}; // { body, head, antL, antR }
    this.rest = {}; // rest quaternions, keyed like BONE
    this.headRest = null; // { parentInv, q, p, s }
    this.legs = [];
    // Body-spin axis in the Core bone's PARENT frame (see _setupModel).
    this.bodyYawAxis = new THREE.Vector3(0, 0, 1);
    this.firstPoseFired = false;
    this._lastTickAt = 0;

    // Reveal (fade-in) state.
    this.revealed = false;
    this._revealTimer = null;
    this._revealFallbackTimer = null;

    // Per-instance scratch + smoothing state (no per-frame allocation).
    this._tmp = {
      p: new THREE.Vector3(),
      q: new THREE.Quaternion(),
      s: new THREE.Vector3(),
      worldQ: new THREE.Quaternion(),
      worldP: new THREE.Vector3(),
      mat: new THREE.Matrix4(),
    };
    this._sm = {
      headInit: false,
      yawInit: false,
      antInit: false,
      smP: new THREE.Vector3(),
      smQ: new THREE.Quaternion(),
      tgtP: new THREE.Vector3(),
      tgtQ: new THREE.Quaternion(),
      smYaw: 0,
      smAnt: [0, 0],
    };

    // External hooks.
    this.onStatusChange = null;
    this.onStats = null;
    this.status = "connecting"; // 'connecting' | 'loading' | 'live' | 'offline' | 'error'

    // Live stats exposed via onStats(stats).
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

    const palette = COLORS_LIGHT;

    this.scene = new THREE.Scene();
    this.scene.background = new THREE.Color(palette.bg);
    this.fog = new THREE.Fog(palette.fog, 1, 2.5);
    this.scene.fog = this.fog;

    const { width, height } = mountEl.getBoundingClientRect();
    this.camera = new THREE.PerspectiveCamera(
      CAMERA.fov,
      Math.max(width, 1) / Math.max(height, 1),
      CAMERA.near,
      CAMERA.far,
    );
    this.camera.position.set(...CAMERA.position);

    this.renderer = new THREE.WebGLRenderer({
      antialias: true,
      alpha: false,
      powerPreference: "high-performance",
    });
    this.renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
    this.renderer.setSize(Math.max(width, 1), Math.max(height, 1), false);
    this.renderer.outputColorSpace = THREE.SRGBColorSpace;
    this.renderer.toneMapping = THREE.ACESFilmicToneMapping;
    this.renderer.toneMappingExposure = 1.0;
    // Start hidden; fade in once the first valid pose has settled.
    this.renderer.domElement.style.opacity = "0";
    this.renderer.domElement.style.transition = "opacity 0.35s ease";
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
    this.themeListener = () => this._applyPalette(COLORS_LIGHT);
    this.themeMql.addEventListener("change", this.themeListener);

    this._lastTickAt = performance.now();
    requestAnimationFrame(this._tick);

    this._setStatus("loading");
    try {
      await this._loadModel();
    } catch (err) {
      console.error("[viewer3d] glb load failed", err);
      this._setStatus("error");
      return;
    }

    // One-shot fetch for the central peer id (UI footer). Non-fatal.
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
    this._applyAntennaTint(false);
  }

  /**
   * Recolor the antennas (the glb's only skinned meshes) for dark mode. Clone
   * the antenna material once per instance before mutating it, and stash the
   * original color/metalness so light mode restores them. Ported from the
   * mobile ReachyModel dark-mode effect.
   */
  _applyAntennaTint(dark) {
    if (!this.model) return;
    this.model.traverse((o) => {
      const mesh = o;
      if (!mesh.isSkinnedMesh || !mesh.material) return;
      let mat = mesh.material;
      if (!mat.userData.__antennaClone) {
        mat = mat.clone();
        mat.userData.__antennaClone = true;
        mat.userData.__origColor = mat.color.clone();
        mat.userData.__origMetalness = mat.metalness;
        mesh.material = mat;
      }
      if (dark) {
        mat.color.set(DARK_ANTENNA_COLOR);
        mat.metalness = DARK_ANTENNA_METALNESS;
      } else {
        mat.color.copy(mat.userData.__origColor);
        mat.metalness = mat.userData.__origMetalness;
      }
      mat.needsUpdate = true;
    });
  }

  async _loadModel() {
    console.info("[viewer3d] GLB_URL =", GLB_URL);
    console.info("[viewer3d] DRACO_PATH =", DRACO_PATH);

    const draco = new DRACOLoader();
    draco.setDecoderPath(DRACO_PATH);
    const loader = new GLTFLoader();
    loader.setDRACOLoader(draco);

    const gltf = await loader.loadAsync(GLB_URL);
    draco.dispose();

    const model = gltf.scene;

    // Match the mobile scene-graph transform exactly:
    //   <group rotation.y=DISPLAY_YAW scale=MODEL_SCALE>
    //     <primitive object=model rotation.x=-PI/2 />
    //   </group>
    model.rotation.set(-Math.PI / 2, 0, 0);
    const group = new THREE.Group();
    group.rotation.set(0, DISPLAY_YAW, 0);
    group.scale.setScalar(MODEL_SCALE);
    group.add(model);
    this.scene.add(group);
    this.model = model;

    // World matrices must be current before we snapshot rest poses.
    this.scene.updateMatrixWorld(true);
    this._setupModel(model);

    this._applyAntennaTint(false);

    // Reveal the (rest-pose) model even if no live pose ever arrives, e.g. the
    // daemon is offline. A valid head pose reveals sooner via `_onFirstPose`.
    this._armRevealFallback();
  }

  // Capture bones + rest pose once, mirroring the mobile setup effect.
  _setupModel(model) {
    model.updateWorldMatrix(true, true);
    const modelInv = new THREE.Matrix4().copy(model.matrixWorld).invert();
    const byName = {};
    model.traverse((o) => {
      if (o.name) byName[norm(o.name)] = o;
    });
    for (const [key, name] of Object.entries(BONE)) {
      const node = byName[norm(name)];
      this.bones[key] = node;
      if (node) this.rest[key] = node.quaternion.clone();
      else console.warn(`[viewer3d] bone node not found: ${name}`);
    }
    // Map the model's up axis (Z) into the Core bone's parent frame so a
    // pre-multiplied yaw spins the body about the true vertical (anti-tumbling).
    const bodyNode = this.bones.body;
    if (bodyNode?.parent) {
      const parentModel = new THREE.Matrix4().multiplyMatrices(modelInv, bodyNode.parent.matrixWorld);
      const pq = new THREE.Quaternion();
      parentModel.decompose(new THREE.Vector3(), pq, new THREE.Vector3());
      this.bodyYawAxis.set(0, 0, 1).applyQuaternion(pq.invert()).normalize();
    }
    const head = this.bones.head;
    if (head?.parent) {
      const headModel = new THREE.Matrix4().multiplyMatrices(modelInv, head.matrixWorld);
      const parentModel = new THREE.Matrix4().multiplyMatrices(modelInv, head.parent.matrixWorld);
      const q = new THREE.Quaternion();
      const p = new THREE.Vector3();
      const s = new THREE.Vector3();
      headModel.decompose(p, q, s);
      this.headRest = { parentInv: parentModel.invert(), q, p, s };
      this.legs = LEG_IDS.map((id) => setupLeg(id, byName, head)).filter((l) => l !== null);
    }
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
      this._armRevealFallback();
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
      if (Array.isArray(data.antennas_position) && data.antennas_position.length === 2) {
        this.latest.antennas = data.antennas_position;
      }
      if (typeof data.body_yaw === "number") {
        this.latest.bodyYaw = data.body_yaw;
      }
      // `head_pose.m` is a 16-float row-major 4x4 matrix when the WS was opened
      // with `use_pose_matrix=true`. Drives the whole head 6-DOF pose.
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

  // ── reveal / fade-in (mirrors mobile ReachyViz) ──────────────────────────
  _onFirstPose() {
    if (this.revealed || this._revealTimer !== null) return;
    this._revealTimer = setTimeout(() => this._reveal(), REVEAL_SETTLE_MS);
  }

  _armRevealFallback() {
    if (this.revealed || this._revealFallbackTimer !== null) return;
    this._revealFallbackTimer = setTimeout(() => this._reveal(), REVEAL_FALLBACK_MS);
  }

  _reveal() {
    if (this.revealed) return;
    this.revealed = true;
    if (this.renderer) this.renderer.domElement.style.opacity = "1";
    // Fade out the loading spinner once the robot is on screen.
    this.mountEl?.classList.add("viz-revealed");
    if (this._revealTimer !== null) {
      clearTimeout(this._revealTimer);
      this._revealTimer = null;
    }
    if (this._revealFallbackTimer !== null) {
      clearTimeout(this._revealFallbackTimer);
      this._revealFallbackTimer = null;
    }
  }

  /**
   * Apply the latest (smoothed) pose to the rigged bones. Straight port of the
   * mobile `ReachyModel` useFrame body.
   */
  _updatePose(delta) {
    const { body, head, antL, antR } = this.bones;
    const tmp = this._tmp;
    const sm = this._sm;
    // Exponential smoothing factor (clamp dt so a stalled tab can't jump).
    const a = 1 - Math.exp(-SMOOTH_K * Math.min(delta, 0.05));

    // Body yaw
    if (body && this.rest.body && typeof this.latest.bodyYaw === "number") {
      const tgt = this.latest.bodyYaw ?? 0;
      if (!sm.yawInit) {
        sm.smYaw = tgt;
        sm.yawInit = true;
      } else {
        sm.smYaw += (tgt - sm.smYaw) * a;
      }
      // Pre-multiply: spin about the vertical (parent-frame) axis, then the rest
      // pose. Post-multiplying would spin about the bone's local Z (not vertical
      // -> tumbling).
      tmp.q.setFromAxisAngle(this.bodyYawAxis, YAW_SIGN * sm.smYaw);
      body.quaternion.copy(tmp.q).multiply(this.rest.body);
    }

    // Head 6-DOF from the cartesian pose matrix (robot frame), smoothed then
    // mapped into the model frame, with Z clamped to the platform's reach.
    const m = toMatrix(this.latest.headPose);
    const hr = this.headRest;
    if (head && hr && m) {
      m.decompose(sm.tgtP, sm.tgtQ, tmp.s);
      if (!sm.headInit) {
        sm.smP.copy(sm.tgtP);
        sm.smQ.copy(sm.tgtQ);
        sm.headInit = true;
      } else {
        sm.smP.lerp(sm.tgtP, a);
        sm.smQ.slerp(sm.tgtQ, a);
      }
      tmp.p.copy(sm.smP);
      tmp.q.copy(sm.smQ);
      // Change of basis robot -> model frame (proper rotation, no reflection).
      tmp.q.premultiply(HEAD_FIX).multiply(HEAD_FIX_INV);
      tmp.p.applyQuaternion(HEAD_FIX);
      tmp.worldQ.copy(tmp.q).multiply(hr.q);
      tmp.worldP.copy(hr.p).addScaledVector(tmp.p, UNITS_PER_M);
      tmp.worldP.z = Math.min(hr.p.z + HEAD_Z_MAX, Math.max(hr.p.z + HEAD_Z_MIN, tmp.worldP.z));
      tmp.mat.compose(tmp.worldP, tmp.worldQ, hr.s);
      tmp.mat.premultiply(hr.parentInv);
      tmp.mat.decompose(head.position, head.quaternion, head.scale);
      if (!this.firstPoseFired) {
        this.firstPoseFired = true;
        this._onFirstPose();
      }
    }

    // Neck leg IK: re-solve each leg so its tip tracks the head-mounted target.
    for (const leg of this.legs) solveLeg(leg, LEG_IK_ITERATIONS);

    // Antennas ([rightRad, leftRad])
    const ant = this.latest.antennas;
    if (ant) {
      if (!sm.antInit) {
        sm.smAnt[0] = ant[0];
        sm.smAnt[1] = ant[1];
        sm.antInit = true;
      } else {
        sm.smAnt[0] += (ant[0] - sm.smAnt[0]) * a;
        sm.smAnt[1] += (ant[1] - sm.smAnt[1]) * a;
      }
      if (antL && this.rest.antL) {
        tmp.q.setFromAxisAngle(ANT_AXIS, ANT_SIGN * sm.smAnt[1]);
        antL.quaternion.copy(this.rest.antL).multiply(tmp.q);
      }
      if (antR && this.rest.antR) {
        tmp.q.setFromAxisAngle(ANT_AXIS, ANT_SIGN * sm.smAnt[0]);
        antR.quaternion.copy(this.rest.antR).multiply(tmp.q);
      }
    }
  }

  _tick() {
    if (this.disposed) return;
    requestAnimationFrame(this._tick);

    const now = performance.now();
    const delta = Math.max(0, (now - this._lastTickAt) / 1000);
    this._lastTickAt = now;

    if (!this.visible) return;

    this._frameTimes.push(now);

    // Staleness watchdog: force a reconnect if the feed went silent while we
    // still think we're live (half-open socket). Setting the status off `live`
    // is self-guarding: this branch can't re-fire until a fresh connection
    // flips it back to `live`. `close()` (with wsClosedByUs still false) routes
    // through the normal backoff reconnect.
    if (
      this.status === "live" &&
      this.latest.lastMessageAt &&
      now - this.latest.lastMessageAt > WS_STALE_TIMEOUT_MS
    ) {
      console.warn("[viewer3d] state feed stale, forcing reconnect");
      this._setStatus("offline");
      if (this.ws) {
        try {
          this.ws.close();
        } catch {}
      }
    }

    if (this.model) {
      this._updatePose(delta);
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

  dispose() {
    if (this.disposed) return;
    this.disposed = true;
    this.wsClosedByUs = true;
    if (this.wsReconnectTimer) {
      clearTimeout(this.wsReconnectTimer);
      this.wsReconnectTimer = null;
    }
    if (this._revealTimer !== null) {
      clearTimeout(this._revealTimer);
      this._revealTimer = null;
    }
    if (this._revealFallbackTimer !== null) {
      clearTimeout(this._revealFallbackTimer);
      this._revealFallbackTimer = null;
    }
    if (this.ws) {
      try {
        this.ws.close();
      } catch {}
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
    this.model = null;
    this.bones = {};
    this.rest = {};
    this.headRest = null;
    this.legs = [];
  }
}

export function createViewer() {
  return new Reachy3DViewer();
}

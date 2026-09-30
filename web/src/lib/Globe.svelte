<script lang="ts">
  import { onMount } from "svelte";
  import * as THREE from "three";

  const MIN_PEERS = 30;
  const MAX_PEERS = 45;
  const FADE_OUT = 0.8;
  // Whole globe (land, wire, peers, arcs) scales together.
  const GLOBE_SCALE = 0.9;
  // Peers favour this latitude band (mid-Brazil to southern Canada); poles are rarely visible.
  const BAND_SOUTH = -15;
  const BAND_NORTH = 50;
  const OFF_BAND_WEIGHT = 0.08;

  export type GlobePeer = {
    key: string;
    lat: number;
    lon: number;
    inbound: boolean;
    label: string;
    detail?: string;
    /** This node itself: drawn green, shown as "You!". */
    self?: boolean;
  };
  export type GlobeHover = { peer: GlobePeer; x: number; y: number } | null;

  let {
    peers = [],
    onhover,
  }: {
    peers?: GlobePeer[];
    onhover?: (h: GlobeHover) => void;
  } = $props();

  let host: HTMLDivElement | undefined;
  // Set once the scene exists; syncs real peer dots with the `peers` prop.
  let applyReal: ((list: GlobePeer[]) => void) | null = null;
  // Inside start() the fake peers are also called `peers`; read the prop through this.
  const realPeers = () => peers;

  $effect(() => {
    const list = peers;
    applyReal?.(list);
  });

  onMount(() => {
    const el = host;
    if (!el) return;
    let running = true;
    let frame = 0;
    let ro: ResizeObserver | undefined;
    const disposers: Array<() => void> = [];

    void start();
    return () => {
      running = false;
      cancelAnimationFrame(frame);
      ro?.disconnect();
      for (const d of disposers) d();
    };

    async function start() {
      const land = await loadLand();
      if (!running || !el) return;

      const renderer = new THREE.WebGLRenderer({
        alpha: true,
        antialias: true,
        powerPreference: "low-power",
      });
      renderer.setClearColor(0x000000, 0);
      renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
      el.appendChild(renderer.domElement);

      const scene = new THREE.Scene();
      const camera = new THREE.PerspectiveCamera(28, 1, 0.1, 40);

      const tilt = new THREE.Group();
      tilt.rotation.x = 0.42;
      tilt.rotation.z = -0.18;
      tilt.scale.setScalar(GLOBE_SCALE);
      scene.add(tilt);
      const spin = new THREE.Group();
      tilt.add(spin);

      const sphere = new THREE.Mesh(
        new THREE.SphereGeometry(1, 64, 48),
        new THREE.MeshBasicMaterial({
          color: 0x12100c,
          transparent: true,
          opacity: 0.72,
          depthWrite: false,
        }),
      );
      spin.add(sphere);

      const continents = new THREE.Mesh(
        new THREE.SphereGeometry(1.002, 64, 48),
        new THREE.MeshBasicMaterial({
          map: land.texture,
          transparent: true,
          opacity: 0.55,
          depthWrite: false,
          side: THREE.DoubleSide,
        }),
      );
      spin.add(continents);
      sphere.renderOrder = 0;
      continents.renderOrder = 1;

      const wire = new THREE.Mesh(
        new THREE.SphereGeometry(1.004, 24, 16),
        new THREE.MeshBasicMaterial({
          color: 0xf7931a,
          wireframe: true,
          transparent: true,
          opacity: 0.16,
        }),
      );
      spin.add(wire);

      const POOL_R = 1.03;
      // Real peers: yellow (outbound) or purple (inbound) from IP geolocation; this node is green.
      // Each has a larger invisible sphere so it is easy to hover.
      type Real = { peer: GlobePeer; group: THREE.Group; core: THREE.MeshBasicMaterial; halo: THREE.MeshBasicMaterial; hit: THREE.Mesh };
      const REAL_R = 1.035;
      // Core matches the fake peers (0.015); the soft halo is what makes real peers stand out.
      const realGeo = new THREE.SphereGeometry(0.015, 12, 12);
      const haloGeo = new THREE.SphereGeometry(0.0375, 12, 12);
      const hitGeo = new THREE.SphereGeometry(0.075, 8, 8);
      const hitMat = new THREE.MeshBasicMaterial({ transparent: true, opacity: 0, depthWrite: false, colorWrite: false });
      const reals = new Map<string, Real>();
      let hovered: Real | null = null;
      let pointer: { x: number; y: number } | null = null;
      const ray = new THREE.Raycaster();
      const ndc = new THREE.Vector2();

      function latLonToVec(lat: number, lon: number, r: number): THREE.Vector3 {
        // Same mapping as landPoints(): v = colatitude, u = longitude from -180.
        const theta = ((90 - lat) / 180) * Math.PI;
        const phi = ((lon + 180) / 360) * 2 * Math.PI;
        const ring = Math.sin(theta);
        return new THREE.Vector3(-ring * Math.cos(phi) * r, Math.cos(theta) * r, ring * Math.sin(phi) * r);
      }

      function dropReal(r: Real) {
        spin.remove(r.group);
        r.core.dispose();
        r.halo.dispose();
        if (hovered === r) {
          hovered = null;
          onhover?.(null);
        }
      }

      applyReal = (list: GlobePeer[]) => {
        const want = new Set(list.map((p) => p.key));
        for (const [k, r] of reals) {
          if (!want.has(k)) {
            dropReal(r);
            reals.delete(k);
          }
        }
        for (const p of list) {
          const color = p.self ? 0x3dcf8e : p.inbound ? 0xa78bfa : 0xfde047;
          const have = reals.get(p.key);
          if (have) {
            have.peer = p;
            have.core.color.setHex(color);
            have.halo.color.setHex(color);
            have.group.position.copy(latLonToVec(p.lat, p.lon, REAL_R));
            continue;
          }
          const group = new THREE.Group();
          group.position.copy(latLonToVec(p.lat, p.lon, REAL_R));
          const core = new THREE.MeshBasicMaterial({ color, transparent: true, depthWrite: false, depthTest: false });
          const halo = new THREE.MeshBasicMaterial({
            color,
            transparent: true,
            opacity: 0.18,
            depthWrite: false,
            depthTest: false,
            blending: THREE.AdditiveBlending,
          });
          const haloMesh = new THREE.Mesh(haloGeo, halo);
          const coreMesh = new THREE.Mesh(realGeo, core);
          const hit = new THREE.Mesh(hitGeo, hitMat);
          haloMesh.renderOrder = 4;
          coreMesh.renderOrder = 5;
          group.add(haloMesh, coreMesh, hit);
          spin.add(group);
          reals.set(p.key, { peer: p, group, core, halo, hit });
        }
      };
      applyReal(realPeers());

      function onPointer(e: PointerEvent) {
        const t = e.target as Element | null;
        // Panels and controls sit on top of the globe; do not hover through them.
        pointer = t?.closest?.(".p2p-float, .heights-strip, button, a, input, .switch") ? null : { x: e.clientX, y: e.clientY };
      }
      function onLeave() {
        pointer = null;
      }
      window.addEventListener("pointermove", onPointer, { passive: true });
      document.addEventListener("pointerleave", onLeave);
      disposers.push(() => {
        window.removeEventListener("pointermove", onPointer);
        document.removeEventListener("pointerleave", onLeave);
      });

      function screenOf(obj: THREE.Object3D): { x: number; y: number } {
        const rect = renderer.domElement.getBoundingClientRect();
        const v = new THREE.Vector3().setFromMatrixPosition(obj.matrixWorld).project(camera);
        return { x: rect.left + ((v.x + 1) / 2) * rect.width, y: rect.top + ((1 - v.y) / 2) * rect.height };
      }

      function facesCamera(obj: THREE.Object3D): boolean {
        tmp.setFromMatrixPosition(obj.matrixWorld);
        return tmp.z > 0.12;
      }

      function updateHover() {
        let next: Real | null = null;
        if (pointer && reals.size) {
          const rect = renderer.domElement.getBoundingClientRect();
          if (
            pointer.x >= rect.left &&
            pointer.x <= rect.right &&
            pointer.y >= rect.top &&
            pointer.y <= rect.bottom
          ) {
            ndc.set(((pointer.x - rect.left) / rect.width) * 2 - 1, -((pointer.y - rect.top) / rect.height) * 2 + 1);
            ray.setFromCamera(ndc, camera);
            const hits = ray.intersectObjects([...reals.values()].map((r) => r.hit), false);
            for (const h of hits) {
              const r = [...reals.values()].find((x) => x.hit === h.object) ?? null;
              if (r && facesCamera(r.group)) {
                next = r;
                break;
              }
            }
          }
        }
        if (hovered && !facesCamera(hovered.group)) hovered = null;
        if (next) hovered = next;
        else if (hovered && !pointer) hovered = null;
        else if (hovered && pointer) {
          // Keep the bubble while the pointer stays near the dot as the globe turns.
          const p = screenOf(hovered.group);
          if (Math.hypot(p.x - pointer.x, p.y - pointer.y) > 26) hovered = null;
        }
        onhover?.(hovered ? { peer: hovered.peer, ...screenOf(hovered.group) } : null);
      }

      const pool = landPoints(2800, POOL_R, land);
      const weight = pool.map((pt) => {
        const lat = (Math.asin(Math.max(-1, Math.min(1, pt.y / POOL_R))) * 180) / Math.PI;
        return lat >= BAND_SOUTH && lat <= BAND_NORTH ? 1 : OFF_BAND_WEIGHT;
      });
      const peerGeo = new THREE.SphereGeometry(0.015, 8, 8);
      const PACKET_SEGS = 20;
      const PACKET_LEN = 0.32;

      type Peer = {
        idx: number;
        mesh: THREE.Mesh;
        mat: THREE.MeshBasicMaterial;
        born: number;
        dieAt: number;
        retiring: boolean;
        fadeAt: number | null;
      };
      type Flight = {
        to: Peer;
        curve: THREE.QuadraticBezierCurve3;
        line: THREE.Line;
        pos: THREE.BufferAttribute;
        col: THREE.BufferAttribute;
        t: number;
        speed: number;
      };

      const peers: Peer[] = [];
      const flights: Flight[] = [];
      const used = new Set<number>();
      let nextPeer = 0;
      let nextFlight = 0;
      const clock = new THREE.Clock();
      const tmp = new THREE.Vector3();

      function facingZ(idx: number): number {
        tmp.copy(pool[idx]).applyMatrix4(spin.matrixWorld);
        return tmp.z;
      }

      function inbound(p: Peer): boolean {
        return flights.some((f) => f.to === p && f.t < 1);
      }

      function pickSlot(): number | null {
        let total = 0;
        for (let i = 0; i < pool.length; i++) if (!used.has(i)) total += weight[i];
        if (total <= 0) return null;
        let r = Math.random() * total;
        let last = -1;
        for (let i = 0; i < pool.length; i++) {
          if (used.has(i)) continue;
          last = i;
          r -= weight[i];
          if (r <= 0) return i;
        }
        return last >= 0 ? last : null;
      }

      function makePeer(now: number): Peer | null {
        const idx = pickSlot();
        if (idx == null) return null;
        used.add(idx);
        const mat = new THREE.MeshBasicMaterial({
          color: 0xf7931a,
          transparent: true,
          opacity: 0,
          depthWrite: false,
          depthTest: false,
        });
        const mesh = new THREE.Mesh(peerGeo, mat);
        mesh.position.copy(pool[idx]);
        spin.add(mesh);
        return {
          idx,
          mesh,
          mat,
          born: now,
          dieAt: now + 5 + Math.random() * 5,
          retiring: false,
          fadeAt: null,
        };
      }

      function dropPeer(p: Peer) {
        used.delete(p.idx);
        spin.remove(p.mesh);
        p.mat.dispose();
      }

      function writePacket(f: Flight, head: number) {
        const t1 = Math.min(1, Math.max(0, head));
        const t0 = Math.max(0, t1 - PACKET_LEN);
        const span = Math.max(0.004, t1 - t0);
        let enter = 1;
        if (head < PACKET_LEN) enter = head / PACKET_LEN;
        let leave = 1;
        if (head > 1) leave = Math.max(0, 1 - (head - 1) / 0.12);
        if (t1 > 0.9) leave = Math.min(leave, (1 - t1) / 0.1);
        for (let i = 0; i <= PACKET_SEGS; i++) {
          const k = i / PACKET_SEGS;
          const t = t0 + span * k;
          const pt = f.curve.getPoint(t);
          f.pos.setXYZ(i, pt.x, pt.y, pt.z);
          const fade = (0.42 + 0.58 * k * k) * enter * leave;
          f.col.setXYZ(i, 1.0 * fade, 0.86 * fade, 0.28 * fade);
        }
        f.pos.needsUpdate = true;
        f.col.needsUpdate = true;
      }

      function livePeers(now: number): Peer[] {
        return peers.filter((p) => !p.retiring && now - p.born > 0.45 && now < p.dieAt);
      }

      function makeFlight(): Flight | null {
        const src = livePeers(clock.elapsedTime);
        if (src.length < 2) return null;
        const a = src[Math.floor(Math.random() * src.length)];
        let b = src[Math.floor(Math.random() * src.length)];
        if (b === a) b = src[(src.indexOf(a) + 1) % src.length];
        const pa = pool[a.idx];
        const pb = pool[b.idx];
        const mid = pa.clone().add(pb).multiplyScalar(0.5);
        mid.normalize().multiplyScalar(Math.min(1.36, 1.12 + pa.distanceTo(pb) * 0.26));
        const curve = new THREE.QuadraticBezierCurve3(pa, mid, pb);
        const pos = new THREE.BufferAttribute(new Float32Array((PACKET_SEGS + 1) * 3), 3);
        const col = new THREE.BufferAttribute(new Float32Array((PACKET_SEGS + 1) * 3), 3);
        const geo = new THREE.BufferGeometry();
        geo.setAttribute("position", pos);
        geo.setAttribute("color", col);
        const line = new THREE.Line(
          geo,
          new THREE.LineBasicMaterial({
            vertexColors: true,
            transparent: true,
            depthWrite: false,
            depthTest: false,
            blending: THREE.AdditiveBlending,
          }),
        );
        line.renderOrder = 2;
        spin.add(line);
        const f: Flight = {
          to: b,
          curve,
          line,
          pos,
          col,
          t: 0,
          speed: 0.32 + Math.random() * 0.2,
        };
        writePacket(f, 0);
        return f;
      }

      function dropFlight(f: Flight) {
        spin.remove(f.line);
        f.line.geometry.dispose();
        (f.line.material as THREE.Material).dispose();
      }

      function fit() {
        const w = el.clientWidth || 1;
        const h = el.clientHeight || 1;
        renderer.setSize(w, h, false);
        renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
        camera.aspect = w / h;
        const need = 2.025;
        const half = Math.tan(((camera.fov * Math.PI) / 180) / 2);
        const zFromH = need / (2 * half);
        const zFromW = need / (2 * half * camera.aspect);
        camera.position.set(0.22, 0.08, Math.max(zFromH, zFromW));
        camera.updateProjectionMatrix();
      }

      ro = new ResizeObserver(() => fit());
      ro.observe(el);
      fit();

      for (let i = 0; i < 36; i++) {
        const p = makePeer(0);
        if (!p) break;
        const life = 5 + Math.random() * 5;
        const spent = Math.random() * Math.min(4, life - 1);
        p.born = -spent;
        p.dieAt = life - spent;
        peers.push(p);
      }

      const tick = () => {
        if (!running) return;
        const dt = Math.min(clock.getDelta(), 0.05);
        const now = clock.elapsedTime;

        spin.rotation.y += 0.00045;
        spin.updateMatrixWorld();

        for (let i = peers.length - 1; i >= 0; i--) {
          const p = peers[i];
          const age = now - p.born;
          if (!p.retiring && now >= p.dieAt) p.retiring = true;

          const front = facingZ(p.idx) > 0.05;
          const peak = front ? 1 : 0.78;
          const liveLo = front ? 0.88 : 0.62;
          const liveHi = front ? 0.12 : 0.16;
          let op = peak;
          let scale = 1;

          if (p.retiring) {
            if (inbound(p)) {
              op = liveLo + liveHi * (0.5 + 0.5 * Math.sin(now * 2.2 + p.idx));
            } else {
              if (p.fadeAt == null) p.fadeAt = now;
              const k = 1 - (now - p.fadeAt) / FADE_OUT;
              if (k <= 0) {
                dropPeer(p);
                peers.splice(i, 1);
                continue;
              }
              op = k * peak;
              scale = 0.35 + k * 0.65;
            }
          } else if (age < 0.55) {
            const k = Math.max(0, age) / 0.55;
            op = k * peak;
            scale = 0.3 + k * 0.7;
          } else {
            op = liveLo + liveHi * (0.5 + 0.5 * Math.sin(now * 2.2 + p.idx));
          }
          p.mat.opacity = op;
          p.mesh.scale.setScalar(scale);
          p.mesh.renderOrder = front ? 3 : -1;
        }

        for (const r of reals.values()) {
          const front = facesCamera(r.group);
          const on = hovered === r;
          const pulse = 0.5 + 0.5 * Math.sin(now * 2.4 + r.group.position.x * 9);
          r.core.opacity = front ? 1 : 0.35;
          r.halo.opacity = (front ? 0.16 + 0.14 * pulse : 0.05) + (on ? 0.25 : 0);
          r.group.scale.setScalar(on ? 1.35 : 1);
          r.group.visible = true;
        }
        updateHover();

        while (peers.length < MIN_PEERS) {
          const p = makePeer(now);
          if (!p) break;
          peers.push(p);
        }
        if (now >= nextPeer) {
          if (peers.length < MAX_PEERS && Math.random() < 0.7) {
            const p = makePeer(now);
            if (p) peers.push(p);
          }
          nextPeer = now + 0.4 + Math.random() * 1.1;
        }

        if (now >= nextFlight) {
          if (flights.length < 10) {
            const f = makeFlight();
            if (f) flights.push(f);
          }
          nextFlight = now + 0.2 + Math.random() * 0.35;
        }

        for (let i = flights.length - 1; i >= 0; i--) {
          const f = flights[i];
          f.t += f.speed * dt;
          writePacket(f, f.t);
          if (f.t >= 1 + 0.08) {
            dropFlight(f);
            flights.splice(i, 1);
          }
        }

        renderer.render(scene, camera);
        frame = requestAnimationFrame(tick);
      };
      tick();

      disposers.push(() => {
        for (const r of reals.values()) dropReal(r);
        reals.clear();
        applyReal = null;
        realGeo.dispose();
        haloGeo.dispose();
        hitGeo.dispose();
        hitMat.dispose();
        for (const p of peers) dropPeer(p);
        for (const f of flights) dropFlight(f);
        peerGeo.dispose();
        land.texture.dispose();
        renderer.dispose();
        scene.traverse((obj) => {
          const mesh = obj as THREE.Mesh;
          mesh.geometry?.dispose?.();
          const mat = mesh.material as THREE.Material | THREE.Material[] | undefined;
          if (Array.isArray(mat)) mat.forEach((m) => m.dispose());
          else mat?.dispose?.();
        });
        renderer.domElement.remove();
      });
    }
  });

  type Land = {
    texture: THREE.CanvasTexture;
    mask: Uint8Array;
    w: number;
    h: number;
  };

  function isLandColor(r: number, g: number, b: number): boolean {
    if (r + g + b < 100) return false;
    if (b > r + 10 && b > g) return false;
    if (g > 88 && b > 88 && r < g - 12 && r < b - 8) return false;
    return true;
  }

  function landPoints(n: number, r: number, land: Land): THREE.Vector3[] {
    const out: THREE.Vector3[] = [];
    const step = Math.max(1, Math.floor(Math.sqrt((land.w * land.h) / (n * 6))));
    for (let y = 0; y < land.h; y += step) {
      for (let x = 0; x < land.w; x += step) {
        if (land.mask[y * land.w + x] !== 1) continue;
        const u = (x + 0.5) / land.w;
        const v = (y + 0.5) / land.h;
        const theta = v * Math.PI;
        const phi = u * 2 * Math.PI;
        const ring = Math.sin(theta);
        out.push(
          new THREE.Vector3(
            -ring * Math.cos(phi) * r,
            Math.cos(theta) * r,
            ring * Math.sin(phi) * r,
          ),
        );
      }
    }
    return out;
  }

  async function loadLand(): Promise<Land> {
    try {
      const img = await new Promise<HTMLImageElement>((resolve, reject) => {
        const im = new Image();
        im.onload = () => resolve(im);
        im.onerror = () => reject(new Error("land"));
        im.src = "/earth-land.jpg";
      });
      const w = 1024;
      const h = 512;
      const c = document.createElement("canvas");
      c.width = w;
      c.height = h;
      const ctx = c.getContext("2d")!;
      ctx.drawImage(img, 0, 0, w, h);
      const pix = ctx.getImageData(0, 0, w, h);
      const mask = new Uint8Array(w * h);
      const d = pix.data;
      for (let i = 0, p = 0; i < d.length; i += 4, p++) {
        const y = Math.floor(p / w);
        const r = d[i];
        const g = d[i + 1];
        const b = d[i + 2];
        const ice = r > 190 && g > 190 && b > 190;
        // Arctic pack ice sits on ocean; keep Antarctic ice as land.
        if (ice && y < h * 0.3) {
          mask[p] = 0;
          continue;
        }
        mask[p] = isLandColor(r, g, b) ? 1 : 0;
      }
      const inland = new Uint8Array(w * h);
      for (let y = 1; y < h - 1; y++) {
        for (let x = 1; x < w - 1; x++) {
          let n = 0;
          for (let dy = -1; dy <= 1; dy++) {
            for (let dx = -1; dx <= 1; dx++) n += mask[(y + dy) * w + (x + dx)];
          }
          inland[y * w + x] = n >= 8 ? 1 : 0;
        }
      }
      for (let i = 0, p = 0; i < d.length; i += 4, p++) {
        if (inland[p]) {
          d[i] = 210;
          d[i + 1] = 164;
          d[i + 2] = 72;
          d[i + 3] = 72;
          mask[p] = 1;
        } else {
          d[i + 3] = 0;
          mask[p] = 0;
        }
      }
      ctx.putImageData(pix, 0, 0);
      const texture = new THREE.CanvasTexture(c);
      texture.colorSpace = THREE.SRGBColorSpace;
      texture.flipY = true;
      texture.anisotropy = 4;
      return { texture, mask, w, h };
    } catch {
      const c = document.createElement("canvas");
      c.width = 4;
      c.height = 2;
      const texture = new THREE.CanvasTexture(c);
      texture.flipY = true;
      return { texture, mask: new Uint8Array(8), w: 4, h: 2 };
    }
  }
</script>

<div class="globe-stage" bind:this={host}></div>

<style>
  .globe-stage {
    width: 100%;
    height: 100%;
  }
  .globe-stage :global(canvas) {
    display: block;
    width: 100%;
    height: 100%;
  }
</style>

// orbit camera in bng km world space. the eye orbits a ground target at
// `dist` along (yaw, pitch); pitch 0 is top-down, and the pitch ceiling only
// opens up once zoomed past the detail threshold, so the far view stays 2d.

export const TAN = Math.tan(25 * Math.PI / 180)  // tan of the half field of view
export const clamp = (v, a, b) => v < a ? a : v > b ? b : v

const cross = (a, b) => [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
const dot = (a, b) => a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
const norm = a => { const l = Math.hypot(...a); return a.map(v => v / l) }

// heightAt(x, y) supplies the terrain height under target/eye (cpu-side)
export function makeCamera(M, canvas, heightAt) {
  const { minx: MX, miny: MY, cell: CELL, ncols: NC, nrows: NR } = M
  const cam = {
    target: [MX + NC * CELL / 2, MY + NR * CELL / 2],
    dist: Math.max(NC, NR) * CELL / 1.9 / TAN,   // whole grid in view
    yaw: 0,
    pitch: 0,
  }
  const distMin = 1 / (M.smax * TAN), distMax = 1 / (M.smin * TAN)

  // map scale: 1 world km -> `scale()` clip units
  const scale = () => 1 / (cam.dist * TAN)
  const maxPitch = () => 1.38 * clamp(2 * scale() / M.detail_scale - .6, 0, 1)

  // eye position + view-space axes
  const basis = () => {
    const { yaw: a, pitch: p, dist: d } = cam
    const h = d * Math.sin(p), ez = heightAt(...cam.target)
    const e = [cam.target[0] + h * Math.sin(a), cam.target[1] - h * Math.cos(a), ez + d * Math.cos(p)]
    const z = norm([e[0] - cam.target[0], e[1] - cam.target[1], e[2] - ez])
    const x = norm(cross([-Math.sin(a) * Math.cos(p), Math.cos(a) * Math.cos(p), Math.sin(p)], z))
    return { e, ez, x, y: cross(z, x), z }
  }

  // column-major view-projection; near/far scale with dist so depth precision
  // follows the zoom
  const viewProj = () => {
    const n = cam.dist * .02, A = 1 / (n / (cam.dist * 40 + 900) - 1)
    const t = 1 / TAN, u = t * canvas.height / canvas.width
    const { e, x, y, z } = basis()
    const c = i => [u * x[i], t * y[i], A * z[i], -z[i]]
    return new Float32Array([...c(0), ...c(1), ...c(2), -u * dot(x, e), -t * dot(y, e), A * (n - dot(z, e)), dot(z, e)])
  }

  // canvas px -> ground plane at the eye's height datum; rays that miss the
  // ground (or would land absurdly far) are cut at F * dist
  const screenToWorld = (px, py, F = 6) => {
    const { e, ez, x, y, z } = basis()
    const nx = (px / canvas.width * 2 - 1) * TAN * canvas.width / canvas.height
    const ny = (1 - py / canvas.height * 2) * TAN
    const d = [0, 1, 2].map(i => nx * x[i] + ny * y[i] - z[i])
    const s = Math.min(d[2] < -1e-4 ? (ez - e[2]) / d[2] : 1 / 0, F * cam.dist)
    return [e[0] + s * d[0], e[1] + s * d[1]]
  }

  // world -> canvas px (null when behind the eye); returns [px, py, w]
  const project = (vp, x, y, z) => {
    const w = vp[3] * x + vp[7] * y + vp[11] * z + vp[15]
    if (w <= 0) return null
    return [(vp[0] * x + vp[4] * y + vp[8] * z + vp[12]) / w * .5 * canvas.width + canvas.width / 2,
            canvas.height / 2 - (vp[1] * x + vp[5] * y + vp[9] * z + vp[13]) / w * .5 * canvas.height, w]
  }

  // zoom by factor f keeping the world point under (px, py) fixed
  const dolly = (f, px, py) => {
    f = clamp(cam.dist * f, distMin, distMax) / cam.dist
    const g = screenToWorld(px, py)
    cam.target[0] += (1 - f) * (g[0] - cam.target[0])
    cam.target[1] += (1 - f) * (g[1] - cam.target[1])
    cam.dist *= f
    cam.pitch = Math.min(cam.pitch, maxPitch())
  }

  // visible cell range [c0, c1, r0, r1, world bounds], clamped to a 16-cell
  // radius around the target so a horizon view can't demand the whole grid
  const cellRect = (F) => {
    const pts = [[0, 0], [canvas.width, 0], [0, canvas.height], [canvas.width, canvas.height]].map(p => screenToWorld(...p, F))
    const xs = pts.map(p => p[0]), ys = pts.map(p => p[1])
    const c = (cam.target[0] - MX) / CELL | 0, r = (cam.target[1] - MY) / CELL | 0, R = 16
    return [clamp((Math.min(...xs) - MX) / CELL | 0, Math.max(0, c - R), NC - 1),
            clamp((Math.max(...xs) - MX) / CELL | 0, 0, Math.min(NC - 1, c + R)),
            clamp((Math.min(...ys) - MY) / CELL | 0, Math.max(0, r - R), NR - 1),
            clamp((Math.max(...ys) - MY) / CELL | 0, 0, Math.min(NR - 1, r + R)),
            [Math.min(...xs), Math.max(...xs), Math.min(...ys), Math.max(...ys)]]
  }

  return Object.assign(cam, { canvas, scale, maxPitch, basis, viewProj, screenToWorld, project, dolly, cellRect })
}

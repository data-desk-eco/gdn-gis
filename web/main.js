// composition root: boot, layer wiring, input, and picking. everything the
// gpu needs each frame lives on one `state` object; `draw()` renders a frame,
// `sched()` debounces a paging pass after the camera settles.

import { TAN, clamp, makeCamera } from './camera.js'
import { makePaging } from './paging.js'
import { initGPU, makeRenderer } from './render.js'
import { makeUI } from './ui.js'
import { EXAGGERATE } from './shaders.js'

const $ = id => document.getElementById(id)
const cv = $('cv'), msg = $('msg')
const die = t => { msg.textContent = t; throw t }

// local dev reads ../dist; production range-reads the public gcs bucket
const D = /^(localhost|127\.|\[?::1)/.test(location.hostname) ? '../dist/' : 'https://storage.googleapis.com/gdn-gis-data/'

const { dev, ctx, fmt } = await initGPU(cv, die)
const M = await (await fetch(D + 'map.json')).json()
const { minx: MX, miny: MY, cell: CELL, ncols: NC, nrows: NR } = M, N = NC * NR
const { n: T0N, step: T0S } = M.t0, T1P = M.t1.p, T1B = T1P * T1P * 2
const TH = M.detail_scale, BTH = M.bldg_scale, [Y0, Y1] = M.yr

const dpr = Math.min(devicePixelRatio || 1, 2)
const state = {
  dpr, RPX: 4.5 * dpr,           // works-ring radius in device px
  mask: 4095,                    // legend visibility bits
  sel: -1,                       // picked works instance
  yr: Y0, lo: Y0, playing: true, // laid-year window
  baseN: 0, coastN: 0, wkN: 0,
}

// cpu twin of the shader's terrain sampler, for label/pick/eye heights
let t0cpu
const heightAt = (x, y) => {
  const cell = state.layers?.terr?.cells.get(clamp((x - MX) / CELL | 0, 0, NC - 1) + NC * clamp((y - MY) / CELL | 0, 0, NR - 1))
  const [g, W, gx, gy] = cell?.cpu
    ? [cell.cpu, T1P, ((x - MX) / CELL % 1) * (T1P - 1), ((y - MY) / CELL % 1) * (T1P - 1)]
    : [t0cpu, T0N, (x - MX) / T0S, (y - MY) / T0S]
  if (!g) return 0
  const x0 = clamp(gx, 0, W - 1.001), y0 = clamp(gy, 0, W - 1.001), dx = x0 % 1, dy = y0 % 1
  const v = (i, j) => g[Math.min((y0 | 0) + j, W - 1) * W + Math.min((x0 | 0) + i, W - 1)]
  return ((v(0, 0) * (1 - dx) + v(1, 0) * dx) * (1 - dy) + (v(0, 1) * (1 - dx) + v(1, 1) * dx) * dy - 1000) * EXAGGERATE * 1e-4
}

const cam = makeCamera(M, cv, heightAt)
state.cam = cam
const renderer = makeRenderer({ dev, ctx, fmt, canvas: cv, M, state })
const paging = makePaging({ dataUrl: D, ncols: NC, scale: cam.scale, cellRect: cam.cellRect, repaint: () => repaint() })
state.layers = paging.layers

function draw() {
  const { vp, bb, s } = renderer.draw()
  ui.placeLabels(vp, bb, s)
}
// coalesce repaints: however many events land between frames, render once
let dirty = false
const repaint = () => {
  if (dirty) return
  dirty = true
  requestAnimationFrame(() => { dirty = false; draw() })
}
const ui = makeUI({ M, state, cam, heightAt, repaint })

// paging runs when the camera has settled for 110 ms
let timer
const sched = () => { clearTimeout(timer); timer = setTimeout(update, 110) }
const update = () => { const done = paging.update(); repaint(); return done }

const resize = () => { cv.width = innerWidth * dpr | 0; cv.height = innerHeight * dpr | 0; repaint(); sched() }
addEventListener('resize', resize)

// --- input: drag pans, right-drag/shift-drag orbits, wheel dollies to the
// cursor, two pointers pinch-zoom + twist + pitch ---
cv.oncontextmenu = e => e.preventDefault()
const pts = new Map()
let px = 0, py = 0, btn = -1, pinchD = 0, pinchA = 0, pinchY = 0, moved = 0
const pinch = () => {
  const [a, b] = [...pts.values()]
  return { d: Math.hypot(a.clientX - b.clientX, a.clientY - b.clientY), a: Math.atan2(b.clientY - a.clientY, b.clientX - a.clientX), mx: (a.clientX + b.clientX) / 2, my: (a.clientY + b.clientY) / 2 }
}
cv.onpointerdown = e => {
  pts.set(e.pointerId, e)
  try { cv.setPointerCapture(e.pointerId) } catch {}
  btn = e.shiftKey ? 2 : e.button
  px = e.clientX; py = e.clientY; moved = 0
  if (pts.size === 2) { const t = pinch(); pinchD = t.d; pinchA = t.a; pinchY = t.my }
}
cv.onpointerup = cv.onpointercancel = e => {
  pts.delete(e.pointerId)
  if (e.type === 'pointerup' && !pts.size && moved < 4) pick(e.clientX * dpr, e.clientY * dpr)
  const rest = [...pts.values()][0]
  if (rest) { px = rest.clientX; py = rest.clientY; btn = 0 } else btn = -1
}
cv.onpointermove = e => {
  if (!pts.has(e.pointerId)) return
  pts.set(e.pointerId, e)
  if (pts.size >= 2) {
    const t = pinch()
    cam.dolly(pinchD / t.d, t.mx * dpr, t.my * dpr)
    cam.yaw += t.a - pinchA
    cam.pitch = clamp(cam.pitch - (t.my - pinchY) * .004, 0, cam.maxPitch())
    pinchD = t.d; pinchA = t.a; pinchY = t.my; moved = 9
    repaint(); sched()
    return
  }
  if (btn < 0) return
  const dx = e.clientX - px, dy = e.clientY - py
  px = e.clientX; py = e.clientY; moved += Math.abs(dx) + Math.abs(dy)
  if (btn === 0) {
    const k = 2 * cam.dist * TAN / (cv.height / dpr), a = cam.yaw
    cam.target[0] -= (Math.cos(a) * dx + Math.sin(a) * dy) * k
    cam.target[1] -= (Math.sin(a) * dx - Math.cos(a) * dy) * k
  } else {
    cam.yaw -= dx * .004
    cam.pitch = clamp(cam.pitch - dy * .004, 0, cam.maxPitch())
  }
  repaint(); sched()
}
cv.onwheel = e => { e.preventDefault(); cam.dolly(Math.exp(e.deltaY * .003), e.offsetX * dpr, e.offsetY * dpr); repaint(); sched() }

// --- picking: nearest works ring by screen distance, else a named building
// in the clicked cell. detail text rides in lazily fetched tsv sidecars ---
let wk, det, tofs
const bldgNames = new Map()
async function pick(x, y) {
  if (cam.scale() < TH) return
  const vp = cam.viewProj(), [bx0, bx1, by0, by1] = cam.cellRect()[4].map((v, i) => v + (i % 2 ? .1 : -.1))
  let best = 9 * dpr
  state.sel = -1
  if (wk) for (let i = 0; i < state.wkN; i++) {
    const wx = wk[i * 4], wy = wk[i * 4 + 1], when = wk[i * 4 + 2] && 1970 + wk[i * 4 + 2] / 365.2425
    if (wx < bx0 || wx > bx1 || wy < by0 || wy > by1
      || !(state.mask >> (wk[i * 4 + 3] > .5 ? 10 : 11) & 1)
      || (when ? when > state.yr && state.yr <= Y1 || when < state.lo : state.lo > Y0)) continue
    const p = cam.project(vp, wx, wy, heightAt(wx, wy) + .004)
    if (!p || p[2] > 3.2 * cam.dist) continue
    const d = Math.hypot(p[0] - x, p[1] - y)
    if (d < best) { best = d; state.sel = i }
  }
  if (state.sel >= 0) {
    det ??= (await (await fetch(D + 'works.tsv')).text()).split('\n')
    const [permit, cat, status, street, town, auth, start, end, tm, loc] = det[state.sel].split('\t')
    return ui.tip([
      `${street || '(no street)'}${town ? ', ' + town : ''}`,
      `${cat} · ${status.toLowerCase()}`,
      [loc, tm].filter(Boolean).join(' · ').toLowerCase(),
      start + (end && end !== start ? ' → ' + end : ''),
      `${auth.toLowerCase()} · permit ${permit}`,
    ].filter(Boolean))
  }
  if (tofs && cam.scale() >= BTH) {
    const [wx, wy] = cam.screenToWorld(x, y)
    const c = clamp((wx - MX) / CELL | 0, 0, NC - 1) + NC * clamp((wy - MY) / CELL | 0, 0, NR - 1)
    if (tofs[c + 1] > tofs[c]) {
      if (!bldgNames.has(c))
        bldgNames.set(c, (await (await fetch(D + 'bldg.tsv', { headers: { Range: `bytes=${tofs[c]}-${tofs[c + 1] - 1}` } })).text()).split('\n'))
      let name = null
      best = 14 * dpr
      for (const row of bldgNames.get(c)) {
        const [bx, by, n] = row.split('\t')
        if (!n) continue
        const p = cam.project(vp, +bx, +by, heightAt(+bx, +by))
        if (!p) continue
        const d = Math.hypot(p[0] - x, p[1] - y)
        if (d < best) { best = d; name = n }
      }
      if (name) return ui.tip([name])
    }
  }
  ui.tip(null)
}

// --- boot: fetch every index + resident blob in parallel, register layers ---
const get = f => fetch(D + f).then(r => r.ok ? r.arrayBuffer() : null).catch(() => null)
resize()
const [pipeIdx, baseAB, t0AB, t1Bits, bldgIdx, bldgTofs, roofIdx, worksAB, coastAB, placesAB] =
  await Promise.all(['map.idx', 'map.base.bin', 'terr0.bin', 'terr1.idx', 'bldg.idx', 'bldg.tofs', 'roof.idx', 'works.f32', 'coast.u16', 'places.tsv'].map(get))
if (!pipeIdx || !baseAB) die('map artefacts missing — run the extractor')

const vbload = bytes => (id, b) => ({ vb: renderer.makeBuffer(b), n: b.byteLength / bytes })
paging.add('pipe', { blob: 'map.bin', counts: new Uint32Array(pipeIdx), bytes: 12, cap: 700, gate: TH, load: vbload(12) })
state.baseVB = renderer.makeBuffer(baseAB)
state.baseN = baseAB.byteLength / 12

if (t0AB) { t0cpu = new Uint16Array(t0AB); renderer.uploadCoarse(t0AB) }
if (t1Bits) {
  // presence bitmap -> 0/1 counts; each cell is one fixed-size height grid
  const bits = new Uint8Array(t1Bits)
  paging.add('terr', {
    blob: 'terr1.bin', counts: Uint8Array.from({ length: N }, (_, i) => bits[i >> 3] >> (i & 7) & 1), bytes: T1B, cap: 248, gate: TH,
    load: (id, b) => { const cpu = new Uint16Array(b.buffer.slice(b.byteOffset, b.byteOffset + T1B)); return { cpu, l: renderer.allocSlot(id, cpu) } },
    onEvict: renderer.freeSlot,
    revisit: (id, e) => { if (e.cpu && e.l == null) e.l = renderer.allocSlot(id, e.cpu) },
  })
}
if (bldgIdx && bldgTofs) {
  tofs = new Uint32Array(bldgTofs)
  paging.add('bldg', { blob: 'bldg.bin', counts: new Uint32Array(bldgIdx), bytes: 12, cap: 250, gate: BTH, load: vbload(12) })
}
if (roofIdx) paging.add('roof', { blob: 'roof.bin', counts: new Uint32Array(roofIdx), bytes: 16, cap: 250, gate: BTH, load: vbload(16) })
if (worksAB) { wk = new Float32Array(worksAB); state.wkVB = renderer.makeBuffer(worksAB); state.wkN = wk.length / 4 }
if (coastAB) { state.coastVB = renderer.makeBuffer(coastAB); state.coastN = coastAB.byteLength / 8 }
if (placesAB) ui.setPlaces(new TextDecoder().decode(placesAB).trim().split('\n').map(l => { const [n, x, y, r] = l.split('\t'); return [+x, +y, 18e3 / +r, n] }))

msg.remove()
await update()
ui.startPlayback()

window.dbg = () => ({
  cam, yr: state.yr, wk, wkN: state.wkN, sel: state.sel, layers: state.layers, draw,
  vpMat: cam.viewProj, proj: cam.project, toWorld: cam.screenToWorld, hgtCPU: heightAt, pick,
  go: (x, y, z, p = 0, a = 0) => { cam.target = [x, y]; cam.dist = 1 / (z * TAN); cam.yaw = a; cam.pitch = p; return update() },
})

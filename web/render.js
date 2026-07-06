// webgpu setup and the per-frame render pass. one shared bind group layout
// (uniforms + the two terrain height textures + the detail-slot lut) serves
// every pipeline; instances stream straight from the paged blobs, so all
// geometry expansion happens in the vertex shaders.

import { shaders } from './shaders.js'

export async function initGPU(canvas, fail) {
  if (!navigator.gpu) fail('this browser has no webgpu')
  const dev = await (await navigator.gpu.requestAdapter())?.requestDevice() || fail('no webgpu adapter')
  const ctx = canvas.getContext('webgpu'), fmt = navigator.gpu.getPreferredCanvasFormat()
  ctx.configure({ device: dev, format: fmt, alphaMode: 'opaque' })
  return { dev, ctx, fmt }
}

const DETAIL_SLOTS = 256  // layers of the lidar detail texture array

export function makeRenderer({ dev, ctx, fmt, canvas, M, state }) {
  const { ncols: NC, nrows: NR } = M, N = NC * NR
  const { n: T0N, step: T0S } = M.t0, T1P = M.t1.p
  const TH = M.detail_scale, BTH = M.bldg_scale, [Y0, Y1] = M.yr
  const BU = GPUBufferUsage, TU = GPUTextureUsage, Q = dev.queue

  // uniforms ×2 (near + far terrain wire grids), detail-slot lut, textures
  const [uni, uni2] = [0, 0].map(() => dev.createBuffer({ size: 128, usage: BU.UNIFORM | BU.COPY_DST }))
  const lutB = dev.createBuffer({ size: N * 4, usage: BU.STORAGE | BU.COPY_DST })
  const tex = size => dev.createTexture({ size, format: 'r16uint', usage: TU.TEXTURE_BINDING | TU.COPY_DST })
  const coarseT = tex([T0N, T0N]), fineT = tex([T1P, T1P, DETAIL_SLOTS])

  const bgl = dev.createBindGroupLayout({
    entries: [{ buffer: {} }, { texture: { sampleType: 'uint' } }, { texture: { sampleType: 'uint', viewDimension: '2d-array' } }, { buffer: { type: 'read-only-storage' } }]
      .map((e, binding) => ({ binding, visibility: GPUShaderStage.VERTEX | GPUShaderStage.FRAGMENT, ...e })),
  })
  const [bg, bg2] = [uni, uni2].map(u => dev.createBindGroup({
    layout: bgl,
    entries: [{ buffer: u }, coarseT.createView(), fineT.createView(), { buffer: lutB }].map((resource, binding) => ({ binding, resource })),
  }))

  // pipeline factory: instanced draws, premultiplied alpha over white, shared
  // depth-stencil target (depth read-only unless a pass opts in)
  const src = shaders(M), mod = {}
  for (const k in src) mod[k] = dev.createShaderModule({ code: src[k] })
  const blend = { color: { srcFactor: 'src-alpha', dstFactor: 'one-minus-src-alpha' }, alpha: { srcFactor: 'one', dstFactor: 'one-minus-src-alpha' } }
  const layout = dev.createPipelineLayout({ bindGroupLayouts: [bgl] })
  const attrs = (arrayStride, a, b) => [{ arrayStride, stepMode: 'instance', attributes: [{ shaderLocation: 0, offset: 0, format: a }, ...(b ? [{ shaderLocation: 1, offset: 8, format: b }] : [])] }]
  const V12 = attrs(12, 'uint16x4', 'uint16x2')
  const mk = (m, constants, buffers, topology = 'line-list', ds, entry = '') => dev.createRenderPipeline({
    layout,
    primitive: { topology },
    vertex: { module: m, entryPoint: 'vs' + entry, constants, buffers },
    fragment: { module: m, entryPoint: 'fs' + entry, constants, targets: [{ format: fmt, blend }] },
    depthStencil: { format: 'depth24plus-stencil8', depthWriteEnabled: false, depthCompare: 'less-equal', ...ds },
  })
  const solid = { depthWriteEnabled: true, depthBias: 2, depthBiasSlopeScale: 2 }
  const pipe = {
    pipes: mk(mod.pipes, { PASS: 0 }, V12, 'triangle-strip'),
    pipesHL: mk(mod.pipes, { PASS: 1 }, V12, 'triangle-strip'),
    bldgEdge: mk(mod.buildings, undefined, V12),
    bldgWall: mk(mod.buildings, undefined, V12, 'triangle-strip', solid, 'f'),
    roof: mk(mod.buildings, undefined, attrs(16, 'uint16x4', 'uint16x4'), 'triangle-list', solid, 'r'),
    works: mk(mod.works, undefined, attrs(16, 'float32x2', 'float32x2'), 'triangle-strip'),
    terrainWire: mk(mod.terrain),
    coastLine: mk(mod.coast, undefined, attrs(8, 'uint16x4')),
    // land/sea: invert stencil per coast-fan triangle, then fill where even
    coastFan: mk(mod.coast, undefined, attrs(8, 'uint16x4'), 'triangle-list',
      { depthCompare: 'always', stencilFront: { compare: 'always', passOp: 'invert' }, stencilBack: { compare: 'always', passOp: 'invert' } }, 'f'),
    sea: mk(mod.coast, undefined, undefined, 'triangle-strip', { stencilFront: { compare: 'equal' }, stencilBack: { compare: 'equal' } }, 's'),
  }

  const makeBuffer = ab => {
    const b = dev.createBuffer({ size: Math.max(12, ab.byteLength), usage: BU.VERTEX | BU.COPY_DST })
    Q.writeBuffer(b, 0, ab)
    return b
  }

  // lidar detail cells live in fineT slots; the lut maps grid cell -> slot
  const lut = new Int32Array(N).fill(-1), freeSlots = [...Array(DETAIL_SLOTS).keys()]
  Q.writeBuffer(lutB, 0, lut)
  const allocSlot = (id, cpu) => {
    const l = freeSlots.pop()
    if (l == null) return null
    Q.writeTexture({ texture: fineT, origin: [0, 0, l] }, cpu, { bytesPerRow: T1P * 2 }, [T1P, T1P, 1])
    lut[id] = l
    Q.writeBuffer(lutB, id * 4, lut, id, 1)
    return l
  }
  const freeSlot = (id, e) => {
    if (e.l == null) return
    freeSlots.push(e.l)
    lut[id] = -1
    Q.writeBuffer(lutB, id * 4, lut, id, 1)
  }
  const uploadCoarse = ab => Q.writeTexture({ texture: coarseT }, ab, { bytesPerRow: T0N * 2 }, [T0N, T0N])

  let depthT
  const uniData = new Float32Array(32)  // reused every frame; writeBuffer copies synchronously
  const writeUni = (buf, vp, cam, grid) => {
    uniData.set(vp)
    uniData.set([state.RPX * 2 / canvas.width, state.RPX * 2 / canvas.height, state.sel, state.yr > Y1 ? 9e3 : state.yr], 16)
    uniData.set(grid, 20)
    uniData.set([state.mask, state.lo > Y0 ? state.lo : 0, cam.pitch, cam.dist], 25)
    Q.writeBuffer(buf, 0, uniData)
  }

  // one pass draws everything, back to front: building fills + roofs write
  // depth so walls occlude; the coast stencil fan classifies sea; wire grids,
  // pipes (base skeleton far out, paged cells at detail), then works rings
  function draw() {
    const { cam, layers } = state, s = cam.scale(), vp = cam.viewProj(), bb = cam.cellRect()[4]

    // near terrain wire grid: ~14 px spacing, capped at 500 lines each way
    const gx = clampT0(bb[0]), gX = clampT0(bb[1]), gy = clampT0(bb[2], 1), gY = clampT0(bb[3], 1)
    const st = Math.max(.05, 14 / (T0S * s * canvas.height / 2), (gX - gx) / 499, (gY - gy) / 499)
    const i0 = Math.floor(gx / st) * st, j0 = Math.floor(gy / st) * st
    const nw = Math.ceil((gX - i0) / st) + 1, nh = Math.ceil((gY - j0) / st) + 1

    // far grid out to the horizon, coarser, only drawn when it adds reach
    const wb = cam.cellRect(40 + 900 / cam.dist)[4]
    const wx = clampT0(wb[0]), wX = clampT0(wb[1]), wy = clampT0(wb[2], 1), wY = clampT0(wb[3], 1)
    const sf = st * Math.ceil(Math.max(1, (wX - wx) / (349 * st), (wY - wy) / (349 * st)))
    const iF = Math.floor(wx / sf) * sf, jF = Math.floor(wy / sf) * sf
    const nwF = Math.ceil((wX - iF) / sf) + 1, nhF = Math.ceil((wY - jF) / sf) + 1

    writeUni(uni, vp, cam, [i0, j0, st, nw, nh])
    writeUni(uni2, vp, cam, [iF, jF, sf, nwF, nhF])

    if (depthT?.width != canvas.width || depthT?.height != canvas.height) {
      depthT?.destroy()
      depthT = dev.createTexture({ size: [canvas.width, canvas.height], format: 'depth24plus-stencil8', usage: TU.RENDER_ATTACHMENT })
    }
    const enc = dev.createCommandEncoder()
    const p = enc.beginRenderPass({
      colorAttachments: [{ view: ctx.getCurrentTexture().createView(), clearValue: { r: 1, g: 1, b: 1, a: 1 }, loadOp: 'clear', storeOp: 'store' }],
      depthStencilAttachment: { view: depthT.createView(), depthClearValue: 1, depthLoadOp: 'clear', depthStoreOp: 'discard', stencilLoadOp: 'clear', stencilStoreOp: 'discard' },
    })
    p.setBindGroup(0, bg)

    if (s >= BTH)
      for (const [L, pl, nv] of [[layers.bldg, pipe.bldgWall, 4], [layers.roof, pipe.roof, 3], [layers.bldg, pipe.bldgEdge, 6]])
        if (L) {
          p.setPipeline(pl)
          for (const o of L.cells.values()) if (o.vis && o.vb) { p.setVertexBuffer(0, o.vb); p.draw(nv, o.n) }
        }

    if (state.coastN) {
      p.setPipeline(pipe.coastFan); p.setVertexBuffer(0, state.coastVB); p.draw(3, state.coastN)
      p.setPipeline(pipe.sea); p.draw(4)
      p.setPipeline(pipe.coastLine); p.draw(2, state.coastN)
    }

    if (cam.pitch > .02 || s >= TH) {
      p.setPipeline(pipe.terrainWire)
      p.draw(2 * Math.max(nw, nh), nw + nh)
      if (sf > st) { p.setBindGroup(0, bg2); p.draw(2 * Math.max(nwF, nhF), nwF + nhF); p.setBindGroup(0, bg) }
    }

    for (const pl of [pipe.pipes, pipe.pipesHL]) {
      p.setPipeline(pl)
      if (state.baseN && s < TH) { p.setVertexBuffer(0, state.baseVB); p.draw(4, state.baseN) }
      if (layers.pipe && s >= TH)
        for (const o of layers.pipe.cells.values()) if (o.vis && o.vb) { p.setVertexBuffer(0, o.vb); p.draw(4, o.n) }
    }

    if (state.wkN && s >= TH) { p.setPipeline(pipe.works); p.setVertexBuffer(0, state.wkVB); p.draw(4, state.wkN) }

    p.end()
    Q.submit([enc.finish()])
    return { vp, bb, s }
  }
  const clampT0 = (v, y) => Math.min(Math.max((v - (y ? M.miny : M.minx)) / T0S, 0), T0N - 1)

  return { draw, makeBuffer, allocSlot, freeSlot, uploadCoarse }
}

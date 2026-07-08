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
  const { nx: T0X, ny: T0Y, step: T0S } = M.t0, T1P = M.t1.p
  const TH = M.detail_scale, BTH = M.bldg_scale, [Y0, Y1] = M.yr
  const BU = GPUBufferUsage, TU = GPUTextureUsage, Q = dev.queue

  // uniforms, detail-slot lut, textures
  const uni = dev.createBuffer({ size: 128, usage: BU.UNIFORM | BU.COPY_DST })
  const lutB = dev.createBuffer({ size: N * 4, usage: BU.STORAGE | BU.COPY_DST })
  const tex = size => dev.createTexture({ size, format: 'r16uint', usage: TU.TEXTURE_BINDING | TU.COPY_DST })
  const coarseT = tex([T0X, T0Y]), fineT = tex([T1P, T1P, DETAIL_SLOTS])

  const bgl = dev.createBindGroupLayout({
    entries: [{ buffer: {} }, { texture: { sampleType: 'uint' } }, { texture: { sampleType: 'uint', viewDimension: '2d-array' } }, { buffer: { type: 'read-only-storage' } }]
      .map((e, binding) => ({ binding, visibility: GPUShaderStage.VERTEX | GPUShaderStage.FRAGMENT, ...e })),
  })
  const bg = dev.createBindGroup({
    layout: bgl,
    entries: [{ buffer: uni }, coarseT.createView(), fineT.createView(), { buffer: lutB }].map((resource, binding) => ({ binding, resource })),
  })

  // pipeline factory: instanced draws, premultiplied alpha over white, shared
  // depth-stencil target (depth read-only unless a pass opts in)
  const src = shaders(M), mod = {}
  for (const k in src) mod[k] = dev.createShaderModule({ code: src[k] })
  const blend = { color: { srcFactor: 'src-alpha', dstFactor: 'one-minus-src-alpha' }, alpha: { srcFactor: 'one', dstFactor: 'one-minus-src-alpha' } }
  const layout = dev.createPipelineLayout({ bindGroupLayouts: [bgl] })
  const attrs = (arrayStride, ...formats) => [{ arrayStride, stepMode: 'instance', attributes: formats.map((format, i) => ({ shaderLocation: i, offset: [0, 8, 12][i], format })) }]
  const V12 = attrs(12, 'uint16x4', 'uint32')
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
    roof: mk(mod.buildings, undefined, attrs(16, 'uint16x4', 'uint16x2', 'uint32'), 'triangle-list', solid, 'r'),
    works: mk(mod.works, undefined, attrs(16, 'float32x2', 'float32x2'), 'triangle-strip'),
    terrainWire: mk(mod.terrain),
    // the hairline is a boundary overlay: lines take no depth bias, so the
    // biased sea plane would win at grazing angles and dash it
    coastLine: mk(mod.coast, undefined, attrs(8, 'uint16x4'), 'line-list', { depthCompare: 'always' }),
    // land/sea: invert stencil per coast-fan triangle, then fill where even
    coastFan: mk(mod.coast, undefined, attrs(8, 'uint16x4'), 'triangle-list',
      { depthCompare: 'always', stencilFront: { compare: 'always', passOp: 'invert' }, stencilBack: { compare: 'always', passOp: 'invert' } }, 'f'),
    // sea writes depth, biased a hair toward the camera so co-planar offshore
    // wire lines (terrain defaults to 0 at sea) fail the depth test cleanly
    sea: mk(mod.coast, undefined, undefined, 'triangle-strip', { depthWriteEnabled: true, depthBias: -2, depthBiasSlopeScale: -2, stencilFront: { compare: 'equal' }, stencilBack: { compare: 'equal' } }, 's'),
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
  const uploadCoarse = ab => Q.writeTexture({ texture: coarseT }, ab, { bytesPerRow: T0X * 2 }, [T0X, T0Y])

  let depthT
  const uniData = new Float32Array(32)  // reused every frame; writeBuffer copies synchronously
  const writeUni = (vp, cam, grid, ma) => {
    uniData.set(vp)
    uniData.set([state.RPX * 2 / canvas.width, state.RPX * 2 / canvas.height, state.sel, state.yr > Y1 ? 9e3 : state.yr], 16)
    uniData.set(grid, 20)
    uniData.set([state.mask, state.lo > Y0 ? state.lo : 0, cam.pitch, cam.dist], 25)
    uniData.set([6, ma], 29)  // wire grid fade-out distance (·dist) + minor-line alpha
    Q.writeBuffer(uni, 0, uniData)
  }

  // one pass draws everything, back to front: building fills + roofs write
  // depth so walls occlude; the coast stencil fan classifies sea and the sea
  // fill writes depth so offshore wire lines are occluded; the wire grid, the
  // coast hairline, pipes (base skeleton far out, paged cells at detail),
  // then works rings
  function draw() {
    const { cam, layers } = state, s = cam.scale(), vp = cam.viewProj(), bb = cam.cellRect()[4]

    // one terrain wire grid covers the whole view (the pitch ceiling bounds
    // ground reach to ~4·dist): ~14 px spacing, capped at 500 lines each way.
    // the step snaps to a pow2 of the base spacing so lines keep fixed world
    // positions while orbiting; the fractional remainder crossfades the minor
    // (odd) lines in via ma
    const gx = clampT0(bb[0]), gX = clampT0(bb[1]), gy = clampT0(bb[2], 1), gY = clampT0(bb[3], 1)
    const rw = Math.max(.05, 14 / (T0S * s * canvas.height / 2), (gX - gx) / 499, (gY - gy) / 499)
    const st = .05 * 2 ** Math.floor(Math.log2(rw / .05))
    const i0 = Math.floor(gx / st) * st, j0 = Math.floor(gy / st) * st
    const nw = Math.ceil((gX - i0) / st) + 1, nh = Math.ceil((gY - j0) / st) + 1

    writeUni(vp, cam, [i0, j0, st, nw, nh], clamp01(2 - rw / st))

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
    }

    if (cam.pitch > .02 || s >= TH) {
      p.setPipeline(pipe.terrainWire)
      p.draw(2 * Math.max(nw, nh), nw + nh)
    }

    if (state.coastN) { p.setPipeline(pipe.coastLine); p.setVertexBuffer(0, state.coastVB); p.draw(2, state.coastN) }

    for (const pl of [pipe.pipes, pipe.pipesHL]) {
      p.setPipeline(pl)
      if (state.baseN && s < TH) { p.setVertexBuffer(0, state.baseVB); p.draw(4, state.baseN) }
      if (layers.pipe && s >= TH)
        for (const o of layers.pipe.cells.values()) if (o.vis && o.vb) { p.setVertexBuffer(0, o.vb); p.draw(4, o.n) }
    }

    // works and the appended timeless markers (nts sites, fatal incidents)
    // both only from detail zoom in — markers freckle the whole-country view
    if (state.wkN && s >= TH) {
      p.setPipeline(pipe.works); p.setVertexBuffer(0, state.wkVB)
      if (state.wkN0) p.draw(4, state.wkN0)
      if (state.wkN > state.wkN0) p.draw(4, state.wkN - state.wkN0, 0, state.wkN0)
    }

    p.end()
    Q.submit([enc.finish()])
    return { vp, bb, s }
  }
  const clampT0 = (v, y) => Math.min(Math.max((v - (y ? M.miny : M.minx)) / T0S, 0), (y ? T0Y : T0X) - 1)
  const clamp01 = v => v < 0 ? 0 : v > 1 ? 1 : v

  return { draw, makeBuffer, allocSlot, freeSlot, uploadCoarse }
}

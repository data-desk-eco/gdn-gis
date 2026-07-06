// demand paging over the 2 km national grid. every heavy layer is one blob
// sorted by cell plus a per-cell record count, so any cell is one contiguous
// byte range: offsets are the prefix sum of counts, visible cells become
// coalesced http range requests, and an lru drops the oldest hidden cells
// once a layer passes its residency cap.
//
// layer spec: {blob, counts, bytes (per record), cap (resident cells),
//   gate (min map scale), load(id, bytes) -> cell, onEvict?, revisit?}
// cell entries carry {vis, t (lru clock), ...load result (vb/n or cpu/slot)}.

export function makePaging({ dataUrl, ncols, scale, cellRect, repaint }) {
  const layers = {}
  let tick = 0

  const add = (id, spec) => {
    let sum = 0
    layers[id] = { ...spec, offsets: Uint32Array.from(spec.counts, c => (sum += c) - c), cells: new Map() }
  }

  async function page(L) {
    const need = []
    for (const e of L.cells.values()) e.vis = false
    if (scale() >= L.gate) {
      const [c0, c1, r0, r1] = cellRect()
      if ((c1 - c0 + 1) * (r1 - r0 + 1) <= 1200)
        for (let r = r0; r <= r1; r++) for (let c = c0; c <= c1; c++) {
          const id = c + ncols * r
          if (!L.counts[id]) continue
          const e = L.cells.get(id)
          if (e) { e.vis = 1; e.t = tick++; L.revisit?.(id, e) }
          else need.push(id)
        }
    }
    if (!need.length) return evict(L)

    // placeholders keep concurrent passes from re-requesting in-flight cells
    need.sort((a, b) => a - b)
    for (const id of need) L.cells.set(id, { vis: 1, t: tick++ })

    // coalesce adjacent cells into single range requests
    const runs = []
    for (const id of need) {
      const r = runs.at(-1)
      if (r && id === r.b + 1) r.b = id; else runs.push({ a: id, b: id })
    }
    await Promise.all(runs.map(async run => {
      try {
        const r = await fetch(dataUrl + L.blob, { headers: { Range: `bytes=${L.offsets[run.a] * L.bytes}-${(L.offsets[run.b] + L.counts[run.b]) * L.bytes - 1}` } })
        if (r.status !== 206) throw 0
        const ab = await r.arrayBuffer()
        let p = 0
        for (let id = run.a; id <= run.b; id++) {
          const len = L.counts[id] * L.bytes
          L.cells.set(id, { ...L.load(id, new Uint8Array(ab, p, len)), vis: L.cells.get(id)?.vis ?? 1, t: tick++ })
          p += len
        }
      } catch {
        for (let id = run.a; id <= run.b; id++)
          if (!L.cells.get(id)?.vb && !L.cells.get(id)?.cpu) L.cells.delete(id)
      }
    }))
    evict(L)
    repaint()
  }

  function evict(L) {
    if (L.cells.size <= L.cap) return
    for (const [id, e] of [...L.cells].filter(([, e]) => !e.vis).sort((a, b) => a[1].t - b[1].t)) {
      if (L.cells.size <= L.cap) break
      L.onEvict?.(id, e)
      e.vb?.destroy()
      L.cells.delete(id)
    }
  }

  // one paging pass over every layer (visibility, fetches, eviction)
  const update = () => Promise.all(Object.values(layers).map(page))

  return { layers, add, update }
}

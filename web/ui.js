// dom chrome: the legend (visibility mask), the laid-year timeline, floating
// place labels, and the click tooltip. all shared ui state lives on `state`;
// anything that changes what the gpu draws just mutates it and repaints.

import { MATERIALS } from './shaders.js'

const $ = id => document.getElementById(id)

export function makeUI({ M, state, cam, heightAt, repaint }) {
  const [Y0, Y1] = M.yr
  const leg = $('leg'), yrs = $('yrs'), los = $('los'), play = $('play'), ylab = $('ylab'), tp = $('tp')

  // legend: one key per material (mask bit = palette slot, 8 unknown, 9 mpdi),
  // the nts transmission layer (12 lines, 13 sites), fatal incidents (14) and
  // the two works severities (10, 11); click toggles the bit
  leg.innerHTML = MATERIALS.map(([hex, label, bit], i) =>
    `<div class=key data-b=${bit ?? (i > 3 ? 8 : 9)}><i style=background:#${hex}></i>${label}</div>`).join('')
    + '<div class=key data-b=12><i style=background:#9096a0></i>nts pipeline</div>'
    + '<div class=key data-b=13><i class=dot style="background:#9096a0;border-color:#9096a0;border-radius:0;transform:rotate(45deg) scale(.8)"></i>nts site</div>'
    + '<div class=key data-b=14><i class=dot style=border-color:#e0170f></i>fatal incident</div>'
    + '<div class=key data-b=10><i class=dot></i>emergency</div><div class=key data-b=11><i class=dot style=border-color:#8b8f99></i>urgent</div>'
  leg.onclick = e => {
    const k = e.target.closest('.key')
    if (!k) return
    state.mask ^= 1 << k.dataset.b
    k.classList.toggle('off')
    repaint()
  }

  // timeline: `lo` and `yr` bound the visible laid-year window; autoplay
  // sweeps yr from Y0 and parks past Y1 (= everything, undated included)
  const setYr = v => {
    state.yr = v
    yrs.value = v
    ylab.textContent = (state.lo > Y0 ? (state.lo | 0) + '–' : '') + Math.min(v | 0, Y1)
    repaint()
  }
  const setPlay = p => { state.playing = p; play.textContent = p ? '⏸︎' : '▶︎' }
  let lastT = 0
  function anim(t) {
    if (!state.playing) return
    if (lastT) setYr(Math.min(state.yr + (t - lastT) / 8e3 * (Y1 + 1 - Y0), Y1 + 1))
    lastT = t
    if (state.yr > Y1) return setPlay(false)
    requestAnimationFrame(anim)
  }
  yrs.min = los.min = Y0
  yrs.max = los.max = Y1 + 1
  yrs.step = los.step = .1
  yrs.value = los.value = Y0
  yrs.oninput = () => { setPlay(false); setYr(Math.max(+yrs.value, state.lo)) }
  los.oninput = () => { setPlay(false); state.lo = Math.min(+los.value, state.yr); los.value = state.lo; setYr(state.yr) }
  play.onclick = () => {
    setPlay(!state.playing)
    if (state.playing) {
      if (state.yr > Y1) state.yr = state.lo
      lastT = 0
      requestAnimationFrame(anim)
    }
  }

  // place labels: a fixed pool of 40 spans, biggest places first, dropped
  // when off-screen or within 64 px of an already placed label
  const MAXL = 40
  const pool = [...Array(MAXL)].map(() => $('pl').appendChild(document.createElement('span')))
  // only touch the dom when a span actually changed — labels redraw every
  // frame, but between camera moves they sit still
  const set = (el, name, css) => {
    if (el._name !== name) { el._name = name; el.textContent = name }
    if (el._css !== css) { el._css = css; el.style.cssText = css }
  }
  let places = []
  function placeLabels(vp, bb, s) {
    let li = 0
    const hits = []
    for (const [x, y, thr, name] of places) {
      if (thr >= s || li === MAXL) break
      if (x < bb[0] || x > bb[1] || y < bb[2] || y > bb[3]) continue
      const q = cam.project(vp, x, y, heightAt(x, y))
      if (!q || q[0] < 0 || q[0] > cam.canvas.width || q[1] < 0 || q[1] > cam.canvas.height
        || hits.some(h => Math.hypot(h[0] - q[0], h[1] - q[1]) < 64 * state.dpr)) continue
      hits.push(q)
      set(pool[li++], name, `left:${q[0] / state.dpr}px;top:${q[1] / state.dpr}px;font-size:${thr < .004 ? 12.5 : thr < .02 ? 11 : thr < .1 ? 10 : 9}px`)
    }
    for (; li < MAXL; li++) set(pool[li], '', 'display:none')
  }

  // tooltip: first line bold, rest plain
  const tip = lines => {
    tp.innerHTML = lines ? lines.map((s, i) => `<${i ? 'span' : 'b'}>${s}</${i ? 'span' : 'b'}>`).join('<br>') : ''
    repaint()
  }

  return { tip, placeLabels, setPlaces: p => places = p, startPlayback: () => requestAnimationFrame(anim) }
}

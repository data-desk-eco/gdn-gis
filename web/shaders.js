// every wgsl module in one place, built from the grid constants in map.json.
//
// all shaders share `common`: one uniform block, the terrain height sampler
// (bilinear over the resident 200 m national grid, with paged lidar detail
// cells slotted into a texture array and located via a per-cell lut), and the
// cell-local u16 coordinate decode. that shared sampler is what drapes every
// layer — pipes, buildings, works — over the terrain for free.
//
// uniform block V (128 B; two instances exist so the near and far terrain
// wire grids can draw with different tw/tw2 in one pass):
//   vp   view-projection, bng km world space
//   ms   (line half-width clip x, clip y, selected work index, year clock)
//   tw   terrain wire grid: (col0, row0, step, ncols)
//   tw2  (terrain wire nrows, legend mask bits, from-year, camera pitch)
//   cd   camera distance, for distance fades

// [hex, label, palette slot]: slots 0-7 index the shader palette; materials
// without a slot draw grey (unknown) or via the magenta highlight pass
// (m.p. ductile iron). legend mask bits: slot for palette materials, then
// 8 unknown, 9 mpdi, 10 emergency works, 11 urgent works.
export const MATERIALS = [
  ['ef4b3d', 'cast iron', 0],
  ['f28e2b', 'spun iron', 1],
  ['edc948', 'ductile iron', 4],
  ['ff2d9b', 'm.p. ductile iron'],
  ['76b7b2', 'steel', 5],
  ['b07aa1', 'asbestos cement', 2],
  ['59a14f', 'pvc', 6],
  ['9c755f', 'lead', 3],
  ['4e93d0', 'polyethylene', 7],
  ['4d5261', 'unknown'],
]

// vertical exaggeration; heights are stored as u16 decimetres + 1000
export const EXAGGERATE = 1.5

const rgb = hex => `vec3f(${[0, 2, 4].map(i => (parseInt(hex.substr(i, 2), 16) / 255).toFixed(3))})`
const palette = MATERIALS.filter(m => m[2] >= 0).sort((a, b) => a[2] - b[2]).map(m => rgb(m[0]))

export function shaders(M) {
  const { minx: MX, miny: MY, cell: CELL, ncols: NC, nrows: NR } = M
  const { n: T0N, step: T0S } = M.t0, T1P = M.t1.p, [, Y1] = M.yr

  // the 2x2 texel quad around integer post i, as f32
  const quad = (tex, layer) =>
    `vec4f(vec4u(${[[0, 0], [1, 0], [0, 1], [1, 1]].map(o => `textureLoad(${tex},i+vec2u(${o}),${layer}0).r`)}))`

  const common = `
struct V{vp:mat4x4f,ms:vec4f,tw:vec4f,tw2:vec4f,cd:f32};
@group(0)@binding(0)var<uniform> v:V;
@group(0)@binding(1)var coarseTex:texture_2d<u32>;
@group(0)@binding(2)var fineTex:texture_2d_array<u32>;
@group(0)@binding(3)var<storage> fineSlot:array<i32>;

// culled vertices go here (well off-screen, w=1)
const OFF=vec4f(2e9,0.,0.,1.);

fn fineQuad(i:vec2u,l:u32)->vec4f{return ${quad('fineTex', 'l,')};}
fn coarseQuad(i:vec2u)->vec4f{return ${quad('coarseTex', '')};}
fn bilerp(q:vec4f,r:vec2f)->f32{return (mix(mix(q.x,q.y,r.x),mix(q.z,q.w,r.x),r.y)-1000.)*${EXAGGERATE * 1e-4};}

// terrain height at world point p: the cell's lidar detail texture when
// resident (fineSlot lut), else the resident coarse national grid
fn terrainHeight(p:vec2f)->f32{
  let c=(p-vec2f(${MX},${MY}))/${CELL};
  let i=vec2u(clamp(vec2i(floor(c)),vec2i(0),vec2i(${NC - 1},${NR - 1})));
  let l=fineSlot[i.x+${NC}u*i.y];
  if(l>=0){
    let f=clamp((c-vec2f(i))*${T1P - 1}.,vec2f(0.),vec2f(${T1P - 1 - .001}));
    return bilerp(fineQuad(vec2u(f),u32(l)),fract(f));
  }
  let g=clamp((p-vec2f(${MX},${MY}))/${T0S},vec2f(0.),vec2f(${T0N - 1 - .001}));
  return bilerp(coarseQuad(vec2u(g)),fract(g));
}

// cell-local u16 coords -> bng km (q spans the 2 km cell in 1/65535 steps)
fn cellWorld(c:u32,q:vec2u)->vec2f{
  return vec2f(${MX},${MY})+vec2f(f32(c%${NC}u),f32(c/${NC}u))*${CELL}+vec2f(q)*${CELL / 65535};
}`

  // pipes: 12 B instance = endpoint pair (u16 cell-local x4) + cell + packed
  // year<<8|mpdi<<7|tone. drawn twice: PASS 0 the palette, PASS 1 the magenta
  // m.p. ductile iron highlight. screen-space quad extrusion along the segment.
  const pipes = common + `
override PASS:f32=0.;
struct O{@builtin(position)p:vec4f,@location(0)@interpolate(flat)t:u32};

@vertex fn vs(@builtin(vertex_index)i:u32,@location(0)q:vec4u,@location(1)m:vec2u)->O{
  let yr=m.y>>8u;
  let bit=select(min(m.y&0x7fu,8u),9u,PASS>.5);
  let hide=select(v.ms.w<=${Y1}.,f32(yr)+${M.yr0}.>v.ms.w,yr>0u)
    ||select(v.tw2.z>0.,f32(yr)+${M.yr0}.<v.tw2.z,yr>0u)
    ||((u32(v.tw2.y)>>bit)&1u)==0u;
  if((PASS>.5&&(m.y&0x80u)==0u)||hide){return O(OFF,0u);}
  let A=cellWorld(m.x,q.xy);let B=cellWorld(m.x,q.zw);
  let ca=v.vp*vec4f(A,terrainHeight(A)+.002,1.);
  let cb=v.vp*vec4f(B,terrainHeight(B)+.002,1.);
  let c=select(ca,cb,i>1u);
  let d=normalize(cb.xy/cb.w-ca.xy/ca.w);
  return O(vec4f(c.xy+vec2f(-d.y,d.x)*(f32(i&1u)*2.-1.)*v.ms.xy*${(1 / 4.5).toFixed(4)}*c.w,c.zw),m.y&0xffu);
}
const PAL=array<vec3f,8>(${palette});
@fragment fn fs(@location(0)@interpolate(flat)t:u32)->@location(0)vec4f{
  if(PASS>.5){return vec4f(${rgb('ff2d9b')},1.);}
  let n=t&0x7fu;
  if(n>7u){return vec4f(${rgb('4d5261')},.55);}
  return vec4f(PAL[n],.82);
}`

  // buildings: 12 B instance = footprint edge (u16 pair) + cell + heights
  // (m.y = base<<8 | top, half-metres). vs: 6-vertex wireframe (base edge,
  // roof edge, riser); vsf: 4-vertex wall quad, depth-written so walls
  // occlude; vsr: roof triangle from the 16 B earcut record.
  const buildings = common + `
@vertex fn vs(@builtin(vertex_index)i:u32,@location(0)q:vec4u,@location(1)m:vec2u)->@builtin(position)vec4f{
  let p=cellWorld(m.x,select(q.xy,q.zw,i==1u||i==3u));
  return v.vp*vec4f(p,terrainHeight(p)+select(f32(m.y>>8u),f32(m.y&0xffu),i>=2u&&i!=4u)*5e-4,1.);
}
@fragment fn fs()->@location(0)vec4f{return vec4f(.45,.47,.52,.5);}

@vertex fn vsf(@builtin(vertex_index)i:u32,@location(0)q:vec4u,@location(1)m:vec2u)->@builtin(position)vec4f{
  let p=cellWorld(m.x,select(q.xy,q.zw,(i&1u)==1u));
  return v.vp*vec4f(p,terrainHeight(p)+select(f32(m.y&0xffu),f32(m.y>>8u),i<2u)*5e-4,1.);
}
@fragment fn fsf()->@location(0)vec4f{return vec4f(.855,.865,.885,1.);}

@vertex fn vsr(@builtin(vertex_index)i:u32,@location(0)a:vec4u,@location(1)b:vec4u)->@builtin(position)vec4f{
  let p=cellWorld(b.z,select(select(a.xy,a.zw,i==1u),b.xy,i==2u));
  return v.vp*vec4f(p,terrainHeight(p)+f32(b.w&0xffu)*5e-4,1.);
}
@fragment fn fsr()->@location(0)vec4f{return vec4f(.855,.865,.885,1.);}`

  // works + sites: 16 B instance = (x, y, day, flag) f32. screen-space ring
  // billboard; flag>.5 emergency (dark), selected instance enlarged blue;
  // distance-faded so far views aren't freckled.
  const works = common + `
struct O{@builtin(position)p:vec4f,@location(0)q:vec2f,@location(1)@interpolate(flat)hot:f32,@location(2)@interpolate(flat)em:f32,@location(3)@interpolate(flat)fd:f32};

@vertex fn vs(@builtin(vertex_index)i:u32,@builtin(instance_index)inst:u32,@location(0)pt:vec2f,@location(1)df:vec2f)->O{
  if((df.x>0.&&1970.+df.x/365.2425>v.ms.w)
    ||select(v.tw2.z>0.,1970.+df.x/365.2425<v.tw2.z,df.x>0.)
    ||((u32(v.tw2.y)>>select(11u,10u,df.y>.5))&1u)==0u){return O(OFF,vec2f(0.),0.,0.,0.);}
  // distance-cull on a flat-ground position before paying for the terrain
  // sample — most of the 144k instances are far outside the fade radius
  let ground=v.vp*vec4f(pt,0.,1.);
  let fd=1.-smoothstep(2.5,4.,ground.w/v.cd);
  if(fd<.01){return O(OFF,vec2f(0.),0.,0.,0.);}
  let q=vec2f(vec2u(i&1u,i>>1u))*2.-1.;
  let hot=select(0.,1.,f32(inst)==v.ms.z);
  let c=v.vp*vec4f(pt,terrainHeight(pt)+.004,1.);
  let r=v.ms.xy*select(1.,1.8,hot>.5);
  return O(vec4f(c.xy+q*r*c.w,c.zw),q,hot,df.y,fd);
}
@fragment fn fs(@location(0)q:vec2f,@location(1)@interpolate(flat)hot:f32,@location(2)@interpolate(flat)em:f32,@location(3)@interpolate(flat)fd:f32)->@location(0)vec4f{
  let a=smoothstep(.13,.07,abs(length(q)-.89));
  if(a<.01){discard;}
  return vec4f(select(select(vec3f(.55,.58,.64),vec3f(.07,.07,.07),em>.5),vec3f(0.,.55,.9),hot>.5),a*fd*.95);
}`

  // coastline: 8 B instance = segment (u16 x4, 20 m units). vs: distance-faded
  // hairline; vsf: stencil fan (centre + segment triangles, invert winding
  // rule) marking land; vss: full-extent sea quad drawn where stencil is even.
  const coast = common + `
struct O{@builtin(position)p:vec4f,@location(0)f:f32};

@vertex fn vs(@builtin(vertex_index)i:u32,@location(0)q:vec4u)->O{
  let c=v.vp*vec4f(vec2f(select(q.xy,q.zw,i==1u))*.02,.001,1.);
  return O(c,1.-smoothstep(6.,12.,c.w/v.cd));
}
@fragment fn fs(@location(0)f:f32)->@location(0)vec4f{return vec4f(0.,0.,0.,f);}

@vertex fn vsf(@builtin(vertex_index)i:u32,@location(0)q:vec4u)->@builtin(position)vec4f{
  let p=select(select(vec2f(q.xy),vec2f(q.zw),i==2u)*.02,vec2f(${MX + NC * CELL / 2},${MY + NR * CELL / 2}),i==0u);
  return v.vp*vec4f(p,.001,1.);
}
@fragment fn fsf()->@location(0)vec4f{return vec4f(0.);}

@vertex fn vss(@builtin(vertex_index)i:u32)->@builtin(position)vec4f{
  let p=vec2f(${MX},${MY})+vec2f(${NC * CELL}.,${NR * CELL}.)*(vec2f(vec2u(i&1u,i>>1u))*41.-20.);
  return v.vp*vec4f(p,0.,1.);
}
@fragment fn fss()->@location(0)vec4f{return vec4f(.905,.912,.92,1.);}`

  // terrain wire: no vertex data at all — instance li picks a grid line
  // (first nrows horizontal, then columns), vertex index walks along it,
  // terrainHeight supplies z. grid placement comes from tw/tw2.
  const terrain = common + `
@vertex fn vs(@builtin(vertex_index)vi:u32,@builtin(instance_index)li:u32)->@builtin(position)vec4f{
  let k=vi>>1u;let st=v.tw.z;let nw=u32(v.tw.w);let nh=u32(v.tw2.x);var g:vec2f;
  if(li<nh){
    if(k+1u>=nw){return OFF;}
    g=v.tw.xy+st*vec2f(f32(k+(vi&1u)),f32(li));
  }else{
    if(k+1u>=nh){return OFF;}
    g=v.tw.xy+st*vec2f(f32(li-nh),f32(k+(vi&1u)));
  }
  let p=vec2f(${MX},${MY})+g*${T0S};
  return v.vp*vec4f(p,terrainHeight(p),1.);
}
@fragment fn fs()->@location(0)vec4f{return vec4f(.30,.42,.50,.12+.26*smoothstep(.02,.5,v.tw2.w));}`

  return { pipes, buildings, works, coast, terrain }
}

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
//   fe   terrain wire far fade-out distance (in cd units)
//   fi   terrain wire near fade-in distance
//   ma   minor-line alpha: odd grid lines fade in as the step subdivides

// [hex, label, palette slot]: slots 0-7 index the shader palette; materials
// without a slot draw grey (unknown) or via the magenta highlight pass
// (m.p. ductile iron). legend mask bits: slot for palette materials, then
// 8 unknown, 9 mpdi, 10 emergency works, 11 urgent works, 12 nts pipelines,
// 13 nts sites.
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
  const { nx: T0X, ny: T0Y, step: T0S } = M.t0, T1P = M.t1.p, [, Y1] = M.yr

  // the 2x2 texel quad around integer post i, as f32
  const quad = (tex, layer) =>
    `vec4f(vec4u(${[[0, 0], [1, 0], [0, 1], [1, 1]].map(o => `textureLoad(${tex},i+vec2u(${o}),${layer}0).r`)}))`

  const common = `
struct V{vp:mat4x4f,ms:vec4f,tw:vec4f,tw2:vec4f,cd:f32,fe:f32,fi:f32,ma:f32};
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
  let g=clamp((p-vec2f(${MX},${MY}))/${T0S},vec2f(0.),vec2f(${T0X - 1 - .001},${T0Y - 1 - .001}));
  return bilerp(coarseQuad(vec2u(g)),fract(g));
}

// cell-local u16 coords -> bng km (q spans the 2 km cell in 1/65535 steps)
fn cellWorld(c:u32,q:vec2u)->vec2f{
  return vec2f(${MX},${MY})+vec2f(f32(c%${NC}u),f32(c/${NC}u))*${CELL}+vec2f(q)*${CELL / 65535};
}`

  // pipes: 12 B instance = endpoint pair (u16 cell-local x4) + packed word
  // year<<23|mpdi<<22|tone<<18|cell (tone 9 = nts transmission). drawn twice:
  // PASS 0 the palette, PASS 1 the magenta m.p. ductile iron highlight.
  // screen-space quad extrusion along the segment.
  const pipes = common + `
override PASS:f32=0.;
struct O{@builtin(position)p:vec4f,@location(0)@interpolate(flat)t:u32};

@vertex fn vs(@builtin(vertex_index)i:u32,@location(0)q:vec4u,@location(1)m:u32)->O{
  let cell=m&0x3ffffu;let tn=(m>>18u)&0xfu;let yr=m>>23u;
  let bit=select(select(min(tn,8u),12u,tn==9u),9u,PASS>.5);
  let hide=select(v.ms.w<=${Y1}.,f32(yr)+${M.yr0}.>v.ms.w,yr>0u)
    ||select(v.tw2.z>0.,f32(yr)+${M.yr0}.<v.tw2.z,yr>0u)
    ||((u32(v.tw2.y)>>bit)&1u)==0u;
  if((PASS>.5&&(m&0x400000u)==0u)||hide){return O(OFF,0u);}
  let A=cellWorld(cell,q.xy);let B=cellWorld(cell,q.zw);
  let ca=v.vp*vec4f(A,terrainHeight(A)+.002,1.);
  let cb=v.vp*vec4f(B,terrainHeight(B)+.002,1.);
  let c=select(ca,cb,i>1u);
  let d=normalize(cb.xy/cb.w-ca.xy/ca.w);
  return O(vec4f(c.xy+vec2f(-d.y,d.x)*(f32(i&1u)*2.-1.)*v.ms.xy*${(1 / 4.5).toFixed(4)}*c.w,c.zw),tn);
}
const PAL=array<vec3f,10>(${palette},${rgb('4d5261')},${rgb('9096a0')});
@fragment fn fs(@location(0)@interpolate(flat)t:u32)->@location(0)vec4f{
  if(PASS>.5){return vec4f(${rgb('ff2d9b')},1.);}
  let n=min(t,9u);
  return vec4f(PAL[n],select(select(.82,.55,n==8u),.95,n==9u));
}`

  // buildings: 12 B instance = footprint edge (u16 pair) + packed word
  // min-height<<26 | height<<18 | cell (half-metres). vs: 6-vertex wireframe
  // (base edge, roof edge, riser); vsf: 4-vertex wall quad, depth-written so
  // walls occlude; vsr: roof triangle from the 16 B earcut record (third
  // vertex + word height<<18|cell).
  const buildings = common + `
@vertex fn vs(@builtin(vertex_index)i:u32,@location(0)q:vec4u,@location(1)m:u32)->@builtin(position)vec4f{
  let p=cellWorld(m&0x3ffffu,select(q.xy,q.zw,i==1u||i==3u));
  return v.vp*vec4f(p,terrainHeight(p)+select(f32(m>>26u),f32((m>>18u)&0xffu),i>=2u&&i!=4u)*5e-4,1.);
}
@fragment fn fs()->@location(0)vec4f{return vec4f(.45,.47,.52,.5);}

@vertex fn vsf(@builtin(vertex_index)i:u32,@location(0)q:vec4u,@location(1)m:u32)->@builtin(position)vec4f{
  let p=cellWorld(m&0x3ffffu,select(q.xy,q.zw,(i&1u)==1u));
  return v.vp*vec4f(p,terrainHeight(p)+select(f32((m>>18u)&0xffu),f32(m>>26u),i<2u)*5e-4,1.);
}
@fragment fn fsf()->@location(0)vec4f{return vec4f(.855,.865,.885,1.);}

@vertex fn vsr(@builtin(vertex_index)i:u32,@location(0)a:vec4u,@location(1)b:vec2u,@location(2)m:u32)->@builtin(position)vec4f{
  let p=cellWorld(m&0x3ffffu,select(select(a.xy,a.zw,i==1u),b,i==2u));
  return v.vp*vec4f(p,terrainHeight(p)+f32((m>>18u)&0xffu)*5e-4,1.);
}
@fragment fn fsr()->@location(0)vec4f{return vec4f(.855,.865,.885,1.);}`

  // works + sites: 16 B instance = (x, y, day, flag) f32. flag 0 urgent /
  // 1 emergency: screen-space ring billboard, laid-year windowed, distance-
  // faded so far views aren't freckled. flag 2 nts site: filled diamond,
  // timeless and visible at every distance. flag 3 fatal incident: red ring,
  // ditto. selected instance enlarged blue.
  const works = common + `
struct O{@builtin(position)p:vec4f,@location(0)q:vec2f,@location(1)@interpolate(flat)hot:f32,@location(2)@interpolate(flat)em:f32,@location(3)@interpolate(flat)fd:f32};

@vertex fn vs(@builtin(vertex_index)i:u32,@builtin(instance_index)inst:u32,@location(0)pt:vec2f,@location(1)df:vec2f)->O{
  let site=df.y>1.5;
  let hide=(!site)&&((df.x>0.&&1970.+df.x/365.2425>v.ms.w)
    ||select(v.tw2.z>0.,1970.+df.x/365.2425<v.tw2.z,df.x>0.));
  if(hide||((u32(v.tw2.y)>>select(select(select(11u,10u,df.y>.5),13u,site),14u,df.y>2.5))&1u)==0u){return O(OFF,vec2f(0.),0.,0.,0.);}
  // distance-cull on a flat-ground position before paying for the terrain
  // sample — most of the 144k instances are far outside the fade radius
  let ground=v.vp*vec4f(pt,0.,1.);
  let fd=select(1.-smoothstep(2.5,4.,ground.w/v.cd),1.,site);
  if(fd<.01){return O(OFF,vec2f(0.),0.,0.,0.);}
  let q=vec2f(vec2u(i&1u,i>>1u))*2.-1.;
  let hot=select(0.,1.,f32(inst)==v.ms.z);
  let c=v.vp*vec4f(pt,terrainHeight(pt)+.004,1.);
  let r=v.ms.xy*select(select(1.,1.8,hot>.5),1.3,site&&hot<.5);
  return O(vec4f(c.xy+q*r*c.w,c.zw),q,hot,df.y,fd);
}
@fragment fn fs(@location(0)q:vec2f,@location(1)@interpolate(flat)hot:f32,@location(2)@interpolate(flat)em:f32,@location(3)@interpolate(flat)fd:f32)->@location(0)vec4f{
  let site=em>1.5&&em<2.5;
  let a=select(smoothstep(.13,.07,abs(length(q)-.89)),smoothstep(1.,.85,abs(q.x)+abs(q.y)),site);
  if(a<.01){discard;}
  var col=select(select(vec3f(.55,.58,.64),vec3f(.07,.07,.07),em>.5),vec3f(.565,.588,.627),site);
  if(em>2.5){col=vec3f(.88,.09,.06);}
  if(hot>.5){col=vec3f(0.,.55,.9);}
  return vec4f(col,a*fd*.95);
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
  // terrainHeight supplies z. grid placement comes from tw/tw2. the step is
  // snapped to a pow2 of the base spacing so lines sit at fixed world
  // positions; odd (minor) lines crossfade in via ma, and each grid fades
  // between its fi/fe distance band so the two tiers hand over invisibly.
  const terrain = common + `
struct O{@builtin(position)p:vec4f,@location(0)w:f32,@location(1)@interpolate(flat)m:f32};

@vertex fn vs(@builtin(vertex_index)vi:u32,@builtin(instance_index)li:u32)->O{
  let k=vi>>1u;let st=v.tw.z;let nw=u32(v.tw.w);let nh=u32(v.tw2.x);var g:vec2f;
  if(li<nh){
    if(k+1u>=nw){return O(OFF,0.,0.);}
    g=v.tw.xy+st*vec2f(f32(k+(vi&1u)),f32(li));
  }else{
    if(k+1u>=nh){return O(OFF,0.,0.);}
    g=v.tw.xy+st*vec2f(f32(li-nh),f32(k+(vi&1u)));
  }
  let x=li-select(0u,nh,li>=nh)+u32(round(select(v.tw.x,v.tw.y,li<nh)/st));
  let p=vec2f(${MX},${MY})+g*${T0S};
  let c=v.vp*vec4f(p,terrainHeight(p),1.);
  return O(c,c.w/v.cd,select(1.,v.ma,(x&1u)==1u));
}
@fragment fn fs(@location(0)w:f32,@location(1)@interpolate(flat)m:f32)->@location(0)vec4f{
  return vec4f(.30,.42,.50,(.12+.26*smoothstep(.02,.5,v.tw2.w))*m*(1.-smoothstep(.7*v.fe,v.fe,w))*smoothstep(v.fi,v.fi+1.8,w));
}`

  return { pipes, buildings, works, coast, terrain }
}

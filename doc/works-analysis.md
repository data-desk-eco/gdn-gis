# emergency works × pipe material: what the permit record says about the iron mains endgame

duckdb analysis of `dist/works.parquet` (143,984 street manager immediate
permits, 2020 – mid-2026) against `dist/pipes.parquet` (2.27 m cadent main
segments, 128,000 km). each work point was snapped to the nearest main within
20 m (99.4 % match ≤30 m); rates below are emergency permits per 1,000 km of
main per year over the six-year window. note the works file stores bng points
but the pipes file stores lon/lat despite bng crs metadata — pipes were
reprojected 4326 → 27700 (always_xy) before joining.

## headlines

1. **cast iron fails ~9× as often as polyethylene.** emergency permits per
   1,000 km-yr: asbestos cement 607, cast iron 587, spun iron 432, steel 261,
   ductile iron 250, pe ~65. iron is 12.8 % of the network by length but sits
   under roughly half of all emergency digs.

2. **age is destiny within material.** 1880s cast iron runs at 941/1,000 km-yr
   vs 457 for 1950s pipe; 1970s pe at 111 vs 40 for 2010s pe —
   first-generation pe is ~3× worse than modern pe.

3. **the failure record drives the replacement queue.** of works whose nearest
   main was installed *after* the dig, 94 % of the replaced hosts were iron —
   ~12,000 emergency digs were followed by insertion of new pe into an iron
   host on the same alignment. naive snapshot attribution therefore inverts the trend: raw iron
   share of emergency works "rises" 34 → 45 % (2020→25), but after
   reattributing works that predate their nearest pipe it *falls* 56 → 49 % —
   the replacement programme is slowly winning on share. per km of
   *remaining* iron, though, the rate is rising: iron-attributed works fell
   only ~10 % 2021→25 while the iron stock shrank ~9 %/yr — matching hse
   rr1216's finding that failures per km of surviving iron main are climbing.

4. **repeat digs are common and concentrated on victorian iron.** 19.7 % of
   emergency works fall within 50 m and 12 months of a previous emergency
   work. 1,587 hundred-metre cells saw ≥5 emergency digs (9,754 works); median
   gap between successive digs in those cells is 112 days. clustered works are
   on bare cast iron 41 % of the time vs 23 % elsewhere. this is the failure
   mode behind the fatal summerseat (2021) and mirfield (2019) explosions:
   fractured cast iron mains that had not been prioritised for replacement.

5. **the worst cluster sits on tier 3 trunk iron.** gill street, poplar e14:
   23 emergency digs in one 100 m cell since 2020, 17 of them on 457–610 mm
   low-pressure cast iron laid ~1900 (plus a 1219 mm 1880s medium-pressure
   main nearby). large-diameter (>18 in) iron shows the *highest* emergency
   rate of the three hse tiers — t1 (≤8 in) 537, t2 (8–18 in) 502, t3 (≥18 in)
   580 — yet tier 3 is the only tier without a blanket decommissioning
   mandate: hse's 30/30 programme requires all ≤8 in iron within 30 m of
   buildings gone by 2032, risk-scored replacement for 8–18 in, and only
   cba-justified case-by-case decommissioning at 18 in+. hse's stated
   rationale is that 18 in+ mains are "the least likely to fail of all those
   within 30 metres of buildings" — the opposite of what this permit record
   shows, and hse's own 2024 review (rr1216) now flags "concerns with the
   methodology used to prioritise larger diameter pipes for decommissioning".

6. **winter is iron season.** dec–feb emergency works run 1.9× jun–aug for
   cast iron and 3.0× for spun iron (brittle circumferential fracture under
   frost-driven ground movement), vs 1.7× for pe and 1.5× for steel. january
   is the peak month network-wide (12.1 k emergency permits vs 5.8 k in june,
   2021–25 pooled — a 2.1× swing).

7. **iron digs also cost more disruption.** mean permit duration 8.9 days on
   cast iron vs 6.9 on pe; cast + spun iron account for 3,029 of 7,871
   emergency road closures.

## caveats

- attribution is nearest-main-within-20 m: some escapes are service pipes, not
  mains, and dense streets can mis-assign between parallel mains. signals here
  are ecological, not per-asset failure records.
- the pipe table is a 2026 snapshot; rates for materials being actively
  removed (iron) are computed against end-of-window length, so true iron
  rates early in the window are modestly understated.
- inserted pe initially looked 75 % worse than direct-buried pe; after
  dropping works that predate the pipe the gap shrinks to ~13 %
  (70.8 vs 62.9) — mostly reverse causation, the residual plausibly the older
  urban ground it lies in.
- street manager covers england only and went live april 2020, hence the
  window; 2026 is a part year.

## replacement pace

pe insertion into old hosts has been flat at ~1,500 km/yr since 2015
(1,405 km in 2025) — in line with cadent's riio-gd2 plan of ~1,700 km/yr of
iron renewal (some is open-cut, not insertion). with ~16,000 km of iron still
in the ground (7,630 km bare cast, 4,859 spun, 3,498 ductile; plus 4,300 km
steel), the remaining ~5,600 km of tier 1 cast/spun stock is consistent with
the 2032 deadline, but the 1,347 km of ≥18 in trunk iron (the highest-rate
tier, per §5) mostly sits outside the mandate.

## sources

- hse iron mains risk reduction programme overview (30/30 policy since 2002):
  <https://www.hse.gov.uk/gas/supply/mainsreplacement/index.htm>
- hse enforcement policy 2026–31: tier boundaries (t1 ≤8 in ≈ 80 % of at-risk
  iron, t2 8–18 in risk-threshold, t3 ≥18 in cba-only), tier 1 decommissioned
  by end 2032:
  <https://www.hse.gov.uk/gas/supply/mainsreplacement/enforcement-policy-2026-2031.htm>
- hse enforcement policy 2013–21, tier 3 rationale ("least likely to fail"):
  <https://www.hse.gov.uk/gas/supply/mainsreplacement/enforcement-policy-2013-2021.htm>
- hse rr1216 (2024) review of imrrp 2013–23: failures per km of remaining
  iron rising; concerns over large-diameter prioritisation methodology:
  <https://www.hse.gov.uk/research/rrhtm/rr1216.htm>
- cadent riio-gd2 repex ~1,700 km/yr, network >131,000 km:
  <https://utilityweek.co.uk/boosting-long-term-efficiency-in-iron-mains-replacement/>
- ofgem gd2 annual report (north london under-delivered repex three years
  running before +15.3 % in 2024/25):
  <https://www.ofgem.gov.uk/sites/default/files/2026-01/Annual_Report_GD_Strategy.pdf>
- riio-gd3 final determinations (dec 2025), cadent 231 km non-mandatory repex
  reprioritisable to highest-risk assets:
  <https://www.ofgem.gov.uk/sites/default/files/2025-12/RIIO-3-Final-Determinations-Cadent.pdf>
- summerseat, bury (feb 2021): fatal explosion, likeliest source a fractured
  cast iron main 35 m away that had not scored high priority for replacement:
  <https://www.dailypost.co.uk/news/uk-world-news/explosion-killed-bereavement-councillor-caused-21358295>
- mirfield (2019): fatal explosion from fractured 6 in cast iron main missing
  from records; ngn fined £5 m:
  <https://www.hazardexonthenet.net/article/189399/UK-utility-fined--5-million-after-fatal-gas-explosion.aspx>
- cadent attended 393,620 reported gas escapes in 2018/19 (~21 % traced to
  its network):
  <https://cadentgas.com/getmedia/86e176d6-05d9-494b-9779-ce33dc80ab26/Cadent_AR1819.pdf>
- street manager live 1 apr 2020 (mandatory 1 jul 2020), immediate
  emergency/urgent categories:
  <https://department-for-transport-streetmanager.github.io/street-manager-docs/open-data/>

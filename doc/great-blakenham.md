# great blakenham: the "pe next to cast iron" pair

investigation note, 2026-07-07. prompted by the map view east of great
blakenham village centre (b1113 corridor, ~e 612 500 n 250 200), where a
polyethylene line and a cast iron line run visibly side by side along the
same road.

## verdict: two pressure tiers, not a duplicate and not a replacement

the pair is real and both mains are live — they are different assets doing
different jobs:

| | crawl id | gpi id | spec | tier | laid |
|---|---|---|---|---|---|
| red | 410965317 | CDT821930639 | 6″ (152 mm) cast iron | **low pressure** | 1930 |
| blue | 410965315 | CDT821930635 | 180 mm polyethylene | **medium pressure** | 1990 |

midpoints sit ~25 m apart — opposite sides of the carriageway, genuinely
separate trenches. the lp cast iron main is the village's original 1930
distribution main, serving the frontages directly; the mp pe main is a 1990
feeder running through on the same road. the map colours by material only
(pressure isn't drawn, apart from the magenta m.p. ductile iron flag), so a
mp feeder sharing a street with an lp main reads as "why didn't the pe
replace the iron?" — it was never meant to.

a useful tell: where pe *does* replace iron here it is mostly by insertion
(screentips like `125MM PE (IN 6" CI)` — 20+ features in the village), and an
inserted main is a single line on the map carrying the pe colour. two
parallel lines are never an insertion record.

## the village in layers (all from the open gpi install dates)

- **1930** — 6″/4″ lp cast iron: the original gasification of the village core.
- **1939** — 10″ (254 mm) **medium-pressure cast iron** north–south through
  the crossroads (CDT440002994733).
- **1965** — lp spun iron infill on the east side (~3.1 km, the largest iron
  tranche here).
- **1973–75** — the junction at the east end of the pair (e 612 690
  n 250 100): a 150 mm mp steel stub (1973) and 150 mm lp ductile iron
  (1975) — the multicolour convergence at the right edge of the view,
  almost certainly a district governor where mp drops to lp.
- **1987–98** — lp pe estate mains and services, plus the 1989/1990 mp pe
  feeders (250/180 mm).
- **2003** — 315 mm mp pe backbone laid north–south *parallel to the 1939 mp
  cast iron*, which is still in cadent's current open asset release.

## the actual story, if there is one

the screenshot pair is unremarkable, but the corridor holds a sharper
version of the same picture: **medium-pressure cast iron from 1939 still
live** alongside its 2003 pe successor, plus ~5.9 km of lp iron (ci + si)
and 1.7 km of mp ci in this one village per the current release. mp iron is
the highest-consequence legacy class, and everything ferrous within 30 m of
buildings is supposed to be gone by the hse iron-mains deadline of 2032.
the map's laid-year timeline now makes exactly this readable in place:
click the pipe (tooltip gives material + laid year) and scrub the window.

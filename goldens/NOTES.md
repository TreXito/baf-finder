# Phase 0 goldens — surprises, replicated quirks, and gaps

Everything here was **replicated verbatim** (bug-compatible). Nothing was
"fixed". Items the Rust port must reproduce exactly.

## Replicated quirks / things worth knowing for the port

1. **NaN/Infinity leak into stats.** `median([])` returns `NaN` and `minMax([])`
   returns `{min: Infinity, max: -Infinity}`. These can surface in outputs (e.g.
   an empty/degenerate pool). They are encoded as sentinel strings (`"NaN"`,
   `"Infinity"`, `"-Infinity"`) in the fixtures — the Rust port must produce the
   same values, not silently coerce them.

2. **`filter.fieldValue` cannot resolve `winning_bid` (or any variant field).**
   The `MIDAS_SWORD` / `MIDAS_STAFF` per-item tiers in `binmaster-filter.json`
   match on `winning_bid`, but `fieldValue` only knows enchantments/attributes/
   extras/`gem_slots`/`stars`/`upgrade_level`/`recombobulated`. So the matcher
   always fails and those tiers **never fire** — a Midas flip falls through to the
   GLOBAL ladder instead. Captured in `filter/decisions.json` (MIDAS_SWORD →
   priority 8 global tier). This looks like a latent config/impl mismatch; left
   as-is.

3. **Empty `seller` never dedups.** In the snapshot ~2 % of rows (and most of the
   oldest rows) have `seller = ''`; the code assigns each an `anon:N` key so they
   never merge in the per-seller dedup (`priceFor`, `cleanSnipe`, `dominanceFloor`,
   modifier model). The Rust port must keep empty-seller rows distinct, in the
   same iteration order (fixture ref order), or dedup counts drift.

4. **Reduced-schema legacy rows exist but fall OUTSIDE the window.** The very
   oldest sold rows (near `MIN(sold_at)`) use an older attrs schema: `pet` has a
   `level` field instead of `exp/candied/heldItem/skin`, and the value arrays
   (`scrolls/gems/parts/extras/itemUuid`) are absent. All such rows predate the
   7-day cutoff (`1783336949`), so they never enter any fixture. Within the
   window the attrs are the full modern shape, though some rows still lack the
   newest arrays (`gems/parts` ~96 %, `itemUuid` ~73 % present) — which is exactly
   why the money core is null-safe (`?? []`, `?? {}`). If a future slice reaches
   back past the cutoff, `petLevelBand` would see `exp = undefined → NaN`, and
   `petLevel`'s loop (`rest < c` with `rest = NaN` always false) would climb to
   `maxLevel`, banding every such pet as `max`. Not triggered here; noted so the
   Rust port matches if the window ever widens.

5. **Bazaar readiness is a fixture parameter.** Fixtures are generated with the
   fixed bazaar map installed and `bazaarReady() = true`. With a cold/empty
   bazaar (`bazaarReady() = false`) `featureBazaarValue` returns `null`
   throughout and the significance decision falls entirely to learned statistics,
   and `craftCost` uses its static master-star fallbacks. The Rust port replays
   the WITH-bazaar state (map serialized in each fixture). A separate
   cold-bazaar fixture set is not generated (see gaps).

6. **`petLevelBand` with an unknown tier** falls back to the LEGENDARY offset
   (20). Covered explicitly in `petLevels/matrix.json` (tiers include `DIVINE`
   and `''`).

7. **Real market anchor is authentic.** The snapshot's 20th-percentile base
   Hyperion value is `528,000,000`, so `craftCeiling(HYPERION*10)` = `(528M +
   313M) × 1.1 = 925,100,000` — matching `craftCost.test.ts` exactly. Confirms
   the slice is anchored to genuine sold data, not a toy.

## Gaps / fixtures owed

- **Deep-page `item_bytes` capture is still OWED.** PORT.md Phase 0 asks for
  nbt fixtures from `fixture-page0.json` *and a deep-page capture*. Only the
  page-0 capture exists locally, so `nbt/page0.json` covers page-0 item shapes
  only (929 unique). A deep-page capture (rarer/higher-value items —
  god-rolls, high-star, exotic variants) should be added when available to widen
  NBT decode coverage.

- **Cold-bazaar variants not generated.** priceIndex/modifierModel/craftCost/
  sniper are generated only in the bazaar-ready state. If the port needs to prove
  parity in the bazaar-gap path (`featureBazaarValue → null`, craft fallbacks),
  regenerate with the bazaar uninstalled and commit a parallel set.

- **`prevByKey` is same-dump.** The sniper fixture uses this page-0 dump's own
  byKey as the previous-dump live-BIN/undercut map (there is no prior dump
  locally). Cross-dump undercut behavior (a flip suppressed by *last* dump's
  cheaper listings) is therefore not exercised. A two-dump capture would close
  this.

- **Slice is a curated 22-id + PET subset**, not the full 2712-id universe. It
  was chosen for coverage (clean high-volume, attribute/starred armor,
  scroll-gated meta weapons + thin Necron swords, craft component, model items,
  pets). Item families outside the slice (books — 0 fresh rows anyway; runes;
  potions; drills; midas) are not priced by these fixtures.

- **No worker-path fixture.** The sniper fixture replays the single-box (no
  worker) sequencing. The worker screen (`screenAuction`) verdict is recorded per
  candidate, but the worker/main handoff, `evalStateVersion` key reuse, and
  direct-slice modes (index.ts / detectWorker.ts / pageWorker.ts) are Phase 3
  transport concerns and out of scope for these domain-core goldens.

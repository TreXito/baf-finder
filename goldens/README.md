# Phase 0 goldens — oracle fixtures for the Rust port

Deterministic `input → output` fixtures produced by **running the real TypeScript
finder** (`ts-oracle/`, a disposable copy of `baf-flip-finder`). These are the
ground truth the Rust port must reproduce exactly in Phase 2. Every output value
was produced by executing the real TS functions/classes — nothing is
hand-written.

## The pinned clock (determinism)

`pinnedNowMs = 1783941749000` — chosen as `MAX(sold_at) * 1000` from the snapshot
(`SELECT MAX(sold_at) FROM sold` = `1783941749`, unix seconds). Every generator
imports `tools/goldens/clock.ts` FIRST, which overrides the global `Date.now` to
return this constant before any money-core code (PriceIndex age cutoff,
modifier-model rebuild, trend window, sniper `foundAtMs`, relist/bazaar
readiness) reads it. `pinnedNowMs` is recorded at the top of every fixture; the
Rust replay MUST use the identical value.

The reference window cutoff that flows from this is
`cutoff = pinnedNowMs/1000 − refMaxAgeDays(7)·86400 = 1783336949`.

## Fixed bazaar snapshot

Fixtures that touch bazaar-priced significance / craft cost install ONE fixed
price map (`tools/goldens/bazaarFixture.ts`), with `lastRefresh = pinnedNowMs` so
`bazaarReady()` is true. It is a synthetic-but-fixed **input** (master-star and
Necron-craft prices match `craftCost.test.ts`); the full map is serialized into
each such fixture under `bazaar`. The Rust port MUST install the identical map.

## Number encoding

Non-finite numbers are serialized as the sentinel strings `"NaN"`, `"Infinity"`,
`"-Infinity"` (plain JSON would lose them to `null`). Everything else is standard
JSON, 2-space indent, object key order = the real code's insertion order.

## The reference slice (priceIndex / modifierModel / craftCost / sniper)

A fixed item-id slice of `sold`, loaded read-only in a deterministic order
(non-pet `ORDER BY auction_id`, then `PET` capped `ORDER BY auction_id LIMIT
1500`). Recorded in each fixture's `source`. The full loaded slice is emitted to
`priceIndex/refs.jsonl` (one `Reference` JSON per line) so Rust can replay
without the 1.1 GB DB; `source.refsSha256` (`4137a29b…`) proves an identical load.

- **cutoff**: `1783336949`  •  **refMaxAgeDays**: 7
- **non-pet ids (22)**: SUPERIOR_DRAGON_CHESTPLATE, TERROR_{HELMET,CHESTPLATE,LEGGINGS,BOOTS},
  HYPERION, ASTRAEA, SCYLLA, VALKYRIE, TERMINATOR, NECRON_HANDLE, DIVAN_CHESTPLATE,
  LIVID_DAGGER, SHADOW_ASSASSIN_CHESTPLATE, AURORA_CHESTPLATE, POWER_WITHER_CHESTPLATE,
  DAEDALUS_AXE, JUJU_SHORTBOW, MAGMA_LORD_CHESTPLATE, FERMENTO_CHESTPLATE,
  GLACITE_HELMET, DWARF_TURTLE_SHELMET
- **PET**: first 1500 fresh rows by auction_id
- **loaded**: 30 942 refs (29 442 non-pet + 1500 pet), 1998 priced keys

Exact SQL (in `source.nonPetSql` / `source.petSql`):
```
SELECT price, sold_at, seller, attrs FROM sold
  WHERE item_id IN (<22 ids>) AND sold_at >= 1783336949 ORDER BY auction_id
SELECT price, sold_at, seller, attrs FROM sold
  WHERE item_id = 'PET' AND sold_at >= 1783336949 ORDER BY auction_id LIMIT 1500
```

## Fixtures

| file | entries | bytes | what |
|---|---|---|---|
| `nbt/page0.json` | 929 | 1,798,131 | unique page-0 `item_bytes` → full `ItemAttributes` (`decodeItemBytes`) |
| `petLevels/matrix.json` | 1152 | 226,796 | `petLevel` + `petLevelBand` over types × tiers × exp breakpoints |
| `inventoryPricing/matrix.json` | 3841 | 1,591,461 | `priceInventory` over target/lbin/basis/paid/age/volume matrix |
| `priceIndex/queries.json` | 267 | 619,285 | full public money-core surface per query (see below) |
| `priceIndex/refs.jsonl` | 30,942 | 12,049,852 | the loaded reference slice (replay input) |
| `craftCost/matrix.json` | 84 | 76,954 | `craftCeiling` over star sweeps + `starMaterialCost(0..12)` |
| `modifierModel/estimates.json` | 261 | 287,113 | `rebuildModifierModel` + `estimateFor` per base key (193 models) |
| `sniper/dump.json` | 1000 | 950,402 | one full page-0 dump through the real finder pipeline |
| `filter/decisions.json` | 1544 | 1,184,682 | `evaluateFlip` over a Flip matrix hitting every branch |

### priceIndex query fields
Per query (`set` ∈ ref | live | starSweep) the fixture dumps every public method:
`baseKey`, `finalKey`, `candidateFeatures`, `sigFeatures`, `sigSignature`,
`minorFeatureValue`, `adjustedPrice`(@100M probe), `baseValueFor`(baseKey & id),
`highForBase`, `soldCountForBase`, `cheapMedian`, `thinKeyEvidence`,
`baseTrendPct`, `zeroStarBaseline`, `cleanSnipe`, `priceFor` (full `KeyStats`),
`dominanceFloor`. (`betterStarCap` is private; it is exercised transitively via
`priceFor`.)

### sniper pipeline
Single-box (no worker) sequencing exactly as `index.ts` runs it: per candidate
in fixture order `evalCleanSnipe → evalMedianFlip`; non-priceable candidates
collect into `lbinCandidates`; then `evalDominanceFlips` + `evalLbinFlips` run
over them. Shared `seen`/relist state accumulates (order-dependent — the order is
`auctions[]` order). The live-BIN map is built from this same dump (minor-adjusted
like `index.ts`) and stands in for `prevByKey` (no previous dump locally). Each
candidate records its `screen` verdict (`screenAuction`) and `decision`.

## Regenerate

Build once, then run any generator (writes to its canonical path; pass an
explicit path arg to write elsewhere):

```
cd ts-oracle && npm run build
node dist/tools/goldens/genNbt.js
node dist/tools/goldens/genPetLevels.js
node dist/tools/goldens/genInventoryPricing.js
node dist/tools/goldens/genPriceIndex.js        # also writes priceIndex/refs.jsonl
node dist/tools/goldens/genCraftCost.js
node dist/tools/goldens/genModifierModel.js
node dist/tools/goldens/genSniper.js
node dist/tools/goldens/genFilter.js
```

## Determinism proof

Each generator was run twice into temp files and `cmp`-compared — all 8 are
byte-identical across runs (sha256 of run A shown):

```
nbt               a1d562830f7ac9c6
petLevels         1e65a49886a4f3fb
inventoryPricing  f136f38db0ecc9a8
priceIndex        444ba2519197fa64
craftCost         d75f88d5d796d5ac
modifierModel     42e5c1e7bca9044c
sniper            836e9ac79c351e84
filter            69b93b76789f557b
```

# baf-finder

A self-hosted auction-house flip finder for Hypixel SkyBlock, written in Rust.

It watches the public Hypixel auction API, prices every active BIN against a
reference index built from real sold auctions, and pushes the flips it finds
to a websocket feed that bots or dashboards can consume. Optional extras: a
Discord webhook, a public key-gated websocket feed, an HTTP flip API / filter
editor, and a bazaar order book recorder.

The pricing model is *attribute-keyed*: a god-rolled item is never priced
against a bare one. See "How it prices" below.

> **Read the disclaimer before running this.** This project talks to the
> Hypixel API and is not affiliated with or endorsed by Hypixel, Mojang,
> Microsoft or Coflnet. Automating gameplay on Hypixel is bannable, this
> repository contains no bot, and what you connect to the flip feed is your
> responsibility. [Credits and legal notes](#credits--legal).

## Quickstart

```bash
rustup toolchain install nightly   # SIMD NBT decode needs nightly
git clone https://github.com/TreXito/baf-finder
cd baf-finder
SERVE=1 cargo run --release
```

First run creates `./data/auctions.sqlite`, collects the last ~60 seconds of
ended BIN auctions, and starts the loop. A key becomes priceable once it has
at least `MIN_REFS` (default 5) observed sales, so on a fresh database flips
start appearing after a few hours once the reference stock builds up.
Everything lands in `./data/`; nothing outside it is written.

Expect roughly 2-4 GB of RAM at the default `RETENTION_DAYS=14` window.
Lower `RETENTION_DAYS` to trade reference depth for memory.

What you get by default:

- `127.0.0.1:15101` websocket flip feed (clients: welcome, flips, estimates)
- `127.0.0.1:15100` HTTP filter editor API (set `ADMIN_PASSWORD` if you
  expose it)
- flips also logged as `FLIP` lines to stderr
- ended auctions collected every ~55 s, price index rebuilt every 10 min

Try a one-shot against the live auction house without the ws feed:

```bash
SWEEP=1 cargo run --release
```

(The sweep needs enough references to price anything; on a fresh DB it is a
pipeline smoke test, not a flip source.)

## How it prices

Items are compared by a **key** derived from their attributes, so a god-roll
is never priced against a base item:

1. **Base value** = the low-percentile sold price of an item's *base key*
   (internal id + stars + pet tier/level band + book enchant), ignoring
   Kuudra attributes.
2. **Attribute significance** - an attribute at a given tier joins the key
   only when its marginal value (median sold price of items carrying it
   minus base value) is at least `ATTR_MIN_SHARE` (default 15%) of the base
   item's value. This is the "is this attribute worth >= X% of the item
   itself" test.
3. **Comparison key** = base key + the significant attribute tiers. Each key
   is priced by the **median** of its own pool and is only usable once it has
   at least `MIN_REFS` (default 5) references.

Five detector lanes run over every auction: `snipe` (median far below),
`median` (plain median margin), `model` (regression over attribute
significance), `lbin` (lowest BIN reference) and `dominance` (a seller pool
dominates the price band). Each candidate is passed through confidence,
volume, craft-cost ceiling, volatility and manipulation guards before it is
emitted.

## Modes

| Mode | Env | What it does |
|------|-----|--------------|
| Serve | `SERVE=1` | Full loop: collect ended auctions, poll BIN pages, price, push flips. |
| Sweep | `SWEEP=1` | One pass over the live AH, log flips, exit. |
| Compare | `COMPARE=1` | Dev only: Rust vs TS head-to-head (needs the TS oracle, not bundled). |
| Bazaar collector | `BAZAAR_COLLECT_ONLY=1` | Only records bazaar snapshots into their own sqlite. |
| Bazaar report | `BZ_REPORT=1` | Prints what the bazaar finder would do right now. |

## Configuration

No keys are required: the auction pages, `auctions_ended`, bazaar and item
resources endpoints all work keyless. Set `API_KEY` only if you want
per-player auction lookups (and respect Hypixel's rate limits with it).

Main env knobs (defaults in parentheses):

| Env | Purpose |
|-----|---------|
| `DB_PATH` | sqlite for sold references and listings (`./data/auctions.sqlite`) |
| `REBUILD_MS` | price index rebuild interval (`600000`) |
| `ENDED_INTERVAL_MS` | ended-auction collection interval (`55000`) |
| `LANES` / `POLL_DELAY_MS` | AH polling lanes and delay between polls |
| `WS_HOST` / `WS_PORT` | flip feed bind (`127.0.0.1:15101`) |
| `DISCORD_WEBHOOK_URL` | optional webhook for found flips |
| `API_KEY` | optional Hypixel API key for player lookups |
| `RECENT_FLIPS_PATH` | own-flip memory (`./data/recent-flips.json`) |
| `COST_BASIS_PATH` | cost basis ledger (`./data/cost-basis.json`) |
| `MAX_HELD_PER_BASE` | flips pushed per base item while holding (3) |

Every model constant in `finder-core/src/config.rs` (`MIN_MARGIN`,
`MIN_PROFIT`, `MIN_REFS`, `ATTR_MIN_SHARE`, `RETENTION_DAYS`, `LIQ_DISCOUNT`
and many more) is also env-overridable; the doc comments there explain each
one. The finder deliberately defaults to a *conservative* configuration.

Sold-through, time-to-sell and other survival statistics are estimated with
Kaplan-Meier so censored (expired unsold) listings are not misread as sales.

### Extra surfaces

- **HTTP flip API / filter editor**: starts with SERVE on
  `127.0.0.1:15100` (`FLIP_API_HOST` / `FLIP_API_PORT`). If you expose it
  (`FLIP_API_HOST=0.0.0.0`), set `ADMIN_PASSWORD` or `/filter` is an open
  control surface.
- **Public key-gated ws feed**: off unless `PUBLIC_WS=1`; `PUBLIC_WS_PORT`
  (15102), `PUBLIC_WS_KEYS_PATH` (`./data/public-keys.json`). Keys are
  SHA-256 hashed on load; a `finder-rs/src/bin/public_key.rs` helper
  generates them.
- **Seller follow** and related collectors: see `finder-rs/src/*.rs` headers.

The flip feed is the same protocol the
[frikadellen-baf](https://github.com/TreXito/frikadellen-baf-121) mod family
consumes, so existing feed clients work against a self-hosted finder.

## Testing

```bash
cargo test
```

The suite replays golden fixtures (NBT decode, price index, craft cost,
modifier model, sniper decisions, filter decisions, survival estimator)
captured from live data. It runs fully offline.

## Credits / legal

- **[Coflnet](https://coflnet.com)**: three concrete things this repo takes
  from them: the flip-feed websocket protocol the feed server speaks
  (welcome / flip / estimate / purse / listed), the optional Hypixel
  auction-fee model behind `AH_FEE_COFL=1` (a port of their fee math), and
  the `sky.coflnet.com` links flip payloads carry for auction context. Not
  affiliated; no Coflnet API, data or paid feed is used anywhere.
- **Hypixel / Mojang**: all auction and bazaar data comes from the public
  Hypixel API endpoints. Hypixel SkyBlock © Hypixel. This project is not
  affiliated with or endorsed by Hypixel, Mojang or Microsoft, and it is not
  connected to NetherAPI beyond optional user-configured seller lookups.
- Test fixtures contain captured public-API responses for regression testing;
  they belong to their respective owners and are not a dataset.
- Thanks to the authors of `simdnbt`, `mimalloc`, `tokio`, `reqwest`,
  `rusqlite` and the Rust team (SIMD decode needs nightly).

**Use at your own risk.** Respecting rate limits is mandatory; hammering the
API from many IPs will get you blocked (and ruins it for everyone). Hooking
the feed up to account automation is against Hypixel's rules and will get
those accounts banned; that choice and those consequences are yours. No
warranty; MIT licensed, see `LICENSE`. Data attribution details in `NOTICE`.

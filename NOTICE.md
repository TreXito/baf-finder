# NOTICE

baf-finder is an independent, self-hosted auction analysis tool. It is not
affiliated with, endorsed by, or connected to Hypixel Inc., Mojang AB,
Microsoft Corporation, Coflnet UG or NetherAPI.

## Data

All auction and bazaar market data consumed by this software comes from
public, unauthenticated Hypixel API endpoints (`/v2/skyblock/auctions`,
`/v2/skyblock/auctions_ended`, `/v2/skyblock/bazaar`,
`/v2/resources/skyblock/items`). Hypixel, SkyBlock and all related marks and
game content are the property of Hypixel Inc. and Mojang AB.

The repository ships small captured fixtures under `goldens/` and
`finder-rs/fixture-page0.json`. They are public API responses, kept for
offline regression testing only, and are not themselves a dataset or a
product.

No Coflnet proprietary data, subscription feeds or paid APIs are consumed or
bundled. The websocket flip-feed protocol the feed server speaks is a
behavioral port of Coflnet's flip feed protocol, for client compatibility;
the optional auction-fee model (`AH_FEE_COFL=1`) is a port of their fee
math; auction links in flip payloads point at `sky.coflnet.com` as a
courtesy to their price-tracking site. Not affiliated with Coflnet.

## Usage

Respect the rate limits and terms of the APIs you call. Automating gameplay
on Hypixel is against their rules; this repository contains no bot or game
client, and the authors accept no responsibility for what consumers of the
flip feed do with it.

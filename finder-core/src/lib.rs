//! finder-core — Phase 2 domain core, a behavioral clone of the TypeScript
//! finder's money core (`baf-flip-finder/src/`). Every module is pinned to the
//! Phase 0 goldens in `../goldens/`; behavior ports VERBATIM (bug-compatible),
//! never "improved" (see PORT.md / STATUS.md Deviations).

pub mod bazaar;
pub mod config;
pub mod craft_cost;
pub mod filter;
pub mod inventory_pricing;
pub mod math;
pub mod modifier_model;
pub mod nbt;
pub mod pet_levels;
pub mod price_index;
pub mod sniper;
pub mod survival;

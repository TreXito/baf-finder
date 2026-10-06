//! Golden-replay parity for petLevels: petLevel + petLevelBand over the
//! type × tier × exp matrix in `goldens/petLevels/matrix.json`.

use finder_core::pet_levels::{pet_level, pet_level_band};
use serde_json::Value;

#[test]
fn pet_levels_golden_parity() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../goldens/petLevels/matrix.json"
    );
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("golden present"))
        .expect("golden parses");
    let entries = doc["entries"].as_array().expect("entries array");
    assert!(entries.len() >= 1000, "expected the full petLevels matrix");

    let mut mismatches = 0usize;
    let mut examples: Vec<String> = Vec::new();
    for e in entries {
        let inp = &e["input"];
        let ty = inp["type"].as_str().unwrap();
        let tier = inp["tier"].as_str().unwrap();
        let exp = inp["exp"].as_f64().unwrap();
        let want_level = e["output"]["petLevel"].as_i64().unwrap();
        let want_band = e["output"]["petLevelBand"].as_str().unwrap();

        let got_level = pet_level(ty, tier, exp);
        let got_band = pet_level_band(ty, tier, exp);
        if got_level != want_level || got_band != want_band {
            mismatches += 1;
            if examples.len() < 6 {
                examples.push(format!(
                    "type={ty} tier={tier} exp={exp}: want ({want_level},{want_band}) got ({got_level},{got_band})"
                ));
            }
        }
    }
    for ex in &examples {
        eprintln!("--- MISMATCH {ex}");
    }
    assert_eq!(
        mismatches,
        0,
        "{mismatches}/{} petLevels entries mismatched",
        entries.len()
    );
}

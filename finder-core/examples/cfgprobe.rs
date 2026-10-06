fn main() {
    use finder_core::config::*;
    println!("MIN_PROFIT          = {}", *MIN_PROFIT);
    println!("MIN_CONFIDENCE      = {}", *MIN_CONFIDENCE);
    println!("MIN_VOLUME_PER_DAY  = {}", *MIN_VOLUME_PER_DAY);
    println!("CRAFT_CEILING_MULT  = {}", *CRAFT_CEILING_MULT);
    println!("RETENTION_DAYS      = {}", *RETENTION_DAYS);
    println!("REF_MAX_RAM         = {}", *REF_MAX_RAM);
    println!("ATTR_MIN_SHARE      = {}", *ATTR_MIN_SHARE);
}

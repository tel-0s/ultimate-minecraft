//! Biomes.
//!
//! Stage 4b starter set: 8 biomes that map cleanly onto the temperate +
//! coastal + alpine spread you'd expect to walk through on the overworld.
//! Each one's `registry_id` is its 0-indexed position in the alphabetical
//! `worldgen/biome` registry sent during configuration (see
//! `connection.rs::registry_data`), which is what the chunk packet's
//! biome paletted container references.
//!
//! Adding a biome here means: (a) bumping the enum, (b) wiring its
//! registry index, (c) making the registry list still include its name.
//! The registry list itself isn't pruned — vanilla shipping all ~65 names
//! means the client expects them all, so we keep all 65 registered and
//! just pick a small subset to actually *assign* during worldgen.

use serde::{Deserialize, Serialize};

/// A biome assigned to a chunk (Stage 4b) or, later, to a 4×4×4 cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Biome {
    Plains,
    Forest,
    Desert,
    SnowyPlains,
    StonyPeaks,
    Beach,
    Ocean,
    River,
}

impl Biome {
    /// Wire ID for the worldgen/biome registry sent during configuration.
    /// MUST stay in sync with the alphabetical list in
    /// `connection.rs::registry_data` — changing the order there breaks
    /// every chunk packet.
    pub const fn registry_id(self) -> u32 {
        match self {
            Self::Beach => 3,
            Self::Desert => 14,
            Self::Forest => 21,
            Self::Ocean => 35,
            Self::Plains => 40,
            Self::River => 41,
            Self::SnowyPlains => 46,
            Self::StonyPeaks => 51,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Beach => "minecraft:beach",
            Self::Desert => "minecraft:desert",
            Self::Forest => "minecraft:forest",
            Self::Ocean => "minecraft:ocean",
            Self::Plains => "minecraft:plains",
            Self::River => "minecraft:river",
            Self::SnowyPlains => "minecraft:snowy_plains",
            Self::StonyPeaks => "minecraft:stony_peaks",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids_unique() {
        let all = [
            Biome::Plains, Biome::Forest, Biome::Desert, Biome::SnowyPlains,
            Biome::StonyPeaks, Biome::Beach, Biome::Ocean, Biome::River,
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(
                    all[i].registry_id(), all[j].registry_id(),
                    "{:?} and {:?} share registry ID", all[i], all[j],
                );
            }
        }
    }

    #[test]
    fn names_are_namespaced() {
        for b in [Biome::Plains, Biome::Desert, Biome::Ocean] {
            assert!(b.name().starts_with("minecraft:"), "biome name {:?}", b.name());
        }
    }

    #[test]
    fn roundtrips_through_json() {
        let json = serde_json::to_string(&Biome::SnowyPlains).unwrap();
        assert_eq!(json, "\"snowy_plains\"");
        let parsed: Biome = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Biome::SnowyPlains);
    }
}

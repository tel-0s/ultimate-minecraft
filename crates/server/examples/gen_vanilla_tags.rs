//! Regenerate `src/net/vanilla_tags.json` from a vanilla client jar's
//! embedded datapack.
//!
//! ```sh
//! cargo run --example gen_vanilla_tags -- /path/to/minecraft-<ver>-client.jar
//! ```
//!
//! Vanilla clients receive ALL tag data from the server (static
//! registries have no built-in fallback on network play), and MC 26.x
//! eagerly resolves several tags while building item data-components at
//! configuration finish — a missing one refuses the connection
//! ("Missing tag TagKey[minecraft:damage_type / minecraft:is_fire]").
//! So we ship the complete vanilla tag networks for every registry we
//! can resolve ids for: the static registries azalea knows, plus the
//! dynamic registries `registry_entries()` declares.
//!
//! Member names (and `#tag` references) are kept as NAMES in the JSON;
//! `net::tags` resolves them to wire ids at startup through azalea /
//! the declared registry lists — names are forever, ids are
//! per-version, so this file survives version bumps unchanged unless
//! vanilla's tag data itself changes.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

/// (registry id, path prefix inside `data/minecraft/tags/`)
const REGISTRIES: &[(&str, &str)] = &[
    // Static registries (ids resolved through azalea's builtin tables).
    ("minecraft:block", "block/"),
    ("minecraft:item", "item/"),
    ("minecraft:entity_type", "entity_type/"),
    ("minecraft:fluid", "fluid/"),
    ("minecraft:game_event", "game_event/"),
    ("minecraft:point_of_interest_type", "point_of_interest_type/"),
    ("minecraft:potion", "potion/"),
    // Dynamic registries we declare during configuration (ids = index
    // into the declared entry list).
    ("minecraft:damage_type", "damage_type/"),
    ("minecraft:timeline", "timeline/"),
    ("minecraft:painting_variant", "painting_variant/"),
    ("minecraft:worldgen/biome", "worldgen/biome/"),
    ("minecraft:banner_pattern", "banner_pattern/"),
    ("minecraft:enchantment", "enchantment/"),
    ("minecraft:dialog", "dialog/"),
    ("minecraft:instrument", "instrument/"),
];

/// Synchronized dynamic registries whose ELEMENT lists we also derive
/// from the jar (the ones `registry_entries()` doesn't hand-declare for
/// order-sensitivity or azalea-derivation reasons). MC 26.x resolves
/// delayed holder components against these at configuration finish
/// ("Missing element minecraft:trim_material / minecraft:redstone"), so
/// every synchronized registry with data must be declared.
const ELEMENT_REGISTRIES: &[(&str, &str)] = &[
    ("minecraft:chat_type", "chat_type/"),
    ("minecraft:trim_pattern", "trim_pattern/"),
    ("minecraft:trim_material", "trim_material/"),
    ("minecraft:banner_pattern", "banner_pattern/"),
    ("minecraft:enchantment", "enchantment/"),
    ("minecraft:jukebox_song", "jukebox_song/"),
    ("minecraft:instrument", "instrument/"),
    ("minecraft:dialog", "dialog/"),
    ("minecraft:sulfur_cube_archetype", "sulfur_cube_archetype/"),
    ("minecraft:test_environment", "test_environment/"),
    ("minecraft:test_instance", "test_instance/"),
];

fn main() {
    let jar_path = std::env::args()
        .nth(1)
        .expect("usage: gen_vanilla_tags <client jar path>");
    let file = std::fs::File::open(&jar_path).expect("open client jar");
    let mut zip = zip_read(file);

    let mut out: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    let names: Vec<String> = zip.file_names().map(|s| s.to_string()).collect();
    for entry_name in names {
        let Some(rel) = entry_name.strip_prefix("data/minecraft/tags/") else {
            continue;
        };
        let Some(rel) = rel.strip_suffix(".json") else {
            continue;
        };
        let Some((registry, tag_name)) = REGISTRIES.iter().find_map(|(reg, prefix)| {
            rel.strip_prefix(prefix).map(|t| (reg.to_string(), t.to_string()))
        }) else {
            continue;
        };

        let mut buf = String::new();
        zip.by_name(&entry_name)
            .expect("entry")
            .read_to_string(&mut buf)
            .expect("read");
        let parsed: serde_json::Value = serde_json::from_str(&buf).expect("tag json");
        let members: Vec<String> = parsed["values"]
            .as_array()
            .expect("values array")
            .iter()
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                // {"id": "...", "required": false} form.
                other => other["id"].as_str().expect("id").to_string(),
            })
            .collect();
        out.entry(registry).or_default().insert(tag_name, members);
    }

    let dest = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/net/vanilla_tags.json");
    std::fs::write(&dest, serde_json::to_string_pretty(&out).unwrap()).expect("write");
    let total: usize = out.values().map(|m| m.len()).sum();
    println!(
        "wrote {} ({} registries, {} tags) from {}",
        dest.display(),
        out.len(),
        total,
        jar_path,
    );

    // Element lists for the jar-derived registries.
    let mut elements: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let names: Vec<String> = zip.file_names().map(|s| s.to_string()).collect();
    for entry_name in names {
        let Some(rel) = entry_name.strip_prefix("data/minecraft/") else {
            continue;
        };
        let Some(rel) = rel.strip_suffix(".json") else {
            continue;
        };
        if rel.contains("tags/") {
            continue;
        }
        let Some((registry, element)) = ELEMENT_REGISTRIES.iter().find_map(|(reg, prefix)| {
            rel.strip_prefix(prefix)
                .filter(|e| !e.contains('/'))
                .map(|e| (reg.to_string(), format!("minecraft:{e}")))
        }) else {
            continue;
        };
        elements.entry(registry).or_default().push(element);
    }
    for list in elements.values_mut() {
        list.sort();
    }
    let dest = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/net/vanilla_registries.json");
    std::fs::write(&dest, serde_json::to_string_pretty(&elements).unwrap()).expect("write");
    let total: usize = elements.values().map(Vec::len).sum();
    println!(
        "wrote {} ({} registries, {} elements)",
        dest.display(),
        elements.len(),
        total,
    );
}

use zip::ZipArchive;

/// (`zip` is a dev-dependency: examples build with dev-deps, and the
/// generator is the only consumer.)
fn zip_read(file: std::fs::File) -> ZipArchive<std::fs::File> {
    ZipArchive::new(file).expect("parse jar as zip")
}

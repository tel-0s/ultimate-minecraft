//! The vanilla tag networks, resolved to wire ids at startup.
//!
//! Vanilla clients receive ALL tag data from the server — static
//! registries have no built-in fallback on network play — and MC 26.x
//! eagerly resolves several tags while building item data-components at
//! configuration finish (`minecraft:damage_type / minecraft:is_fire`
//! was the one that refused the first real-client connection). We ship
//! the complete vanilla tag set, regenerated from a client jar by
//! `cargo run --example gen_vanilla_tags -- <jar>`.
//!
//! `vanilla_tags.json` stores member NAMES (including `#tag`
//! references); this module resolves them per registry:
//! - static registries → azalea's builtin tables (`FromStr` + `to_u32`),
//! - dynamic registries → index into the entry lists the configuration
//!   phase declares (`registry_entries()`),
//! so the checked-in data survives version bumps unchanged unless
//! vanilla's tag content itself changes.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::LazyLock;

use azalea_protocol::common::tags::{TagMap, Tags};
use azalea_registry::Registry;
use azalea_registry::identifier::Identifier;
use indexmap::IndexMap;

type TagFile = BTreeMap<String, BTreeMap<String, Vec<String>>>;

static VANILLA_TAGS: LazyLock<TagFile> = LazyLock::new(|| {
    serde_json::from_str(include_str!("vanilla_tags.json"))
        .expect("vanilla_tags.json parses (regenerate with gen_vanilla_tags)")
});

fn bare(name: &str) -> &str {
    name.strip_prefix("minecraft:").unwrap_or(name)
}

/// Wire id of one member name in a STATIC registry, via azalea.
fn static_id<R: Registry + FromStr>(name: &str) -> Option<i32> {
    R::from_str(bare(name)).ok().map(|k| k.to_u32() as i32)
}

/// Resolve every tag of one registry to element ids, expanding `#tag`
/// references recursively (the wire format carries ids only).
fn resolve_registry(
    registry: &str,
    tags: &BTreeMap<String, Vec<String>>,
    resolve: &dyn Fn(&str) -> Option<i32>,
) -> Vec<Tags> {
    fn expand(
        registry: &str,
        tag: &str,
        tags: &BTreeMap<String, Vec<String>>,
        resolve: &dyn Fn(&str) -> Option<i32>,
        stack: &mut Vec<String>,
        out: &mut Vec<i32>,
    ) {
        if stack.iter().any(|s| s == tag) {
            tracing::warn!("tag reference cycle at {registry}/{tag}");
            return;
        }
        let Some(members) = tags.get(tag) else {
            tracing::warn!("unknown tag reference #{tag} in {registry}");
            return;
        };
        stack.push(tag.to_string());
        for m in members {
            if let Some(referenced) = m.strip_prefix('#') {
                expand(registry, bare(referenced), tags, resolve, stack, out);
            } else if let Some(id) = resolve(m) {
                out.push(id);
            } else {
                // A vanilla member azalea (or our declared list) doesn't
                // know would indicate version skew — surface it.
                tracing::warn!("unresolvable tag member {m} in {registry}/{tag}");
            }
        }
        stack.pop();
    }

    tags.keys()
        .map(|tag| {
            let mut elements = Vec::new();
            expand(registry, tag, tags, resolve, &mut Vec::new(), &mut elements);
            elements.sort_unstable();
            elements.dedup();
            Tags {
                name: Identifier::new(format!("minecraft:{tag}")),
                elements,
            }
        })
        .collect()
}

/// Build the complete UpdateTags payload. `declared` is the
/// configuration phase's registry list (`registry_entries()`), used to
/// resolve dynamic-registry member names to their declared indices.
/// Member-name → wire-id resolver for one registry: azalea's builtin
/// tables for the static registries, the declared entry list for the
/// dynamic ones (None when configuration doesn't declare it).
fn resolver_for<'a>(
    registry: &str,
    declared: &'a [(String, Vec<String>)],
) -> Option<Box<dyn Fn(&str) -> Option<i32> + 'a>> {
    use azalea_registry::builtin as b;
    Some(match registry {
        "minecraft:block" => Box::new(static_id::<b::BlockKind>),
        "minecraft:item" => Box::new(static_id::<b::ItemKind>),
        "minecraft:entity_type" => Box::new(static_id::<b::EntityKind>),
        "minecraft:fluid" => Box::new(static_id::<b::Fluid>),
        "minecraft:game_event" => Box::new(static_id::<b::GameEvent>),
        "minecraft:point_of_interest_type" => Box::new(static_id::<b::PointOfInterestKind>),
        "minecraft:potion" => Box::new(static_id::<b::Potion>),
        // Dynamic: index into the declared entry list.
        _ => {
            let (_, entries) = declared.iter().find(|(n, _)| n == registry)?;
            Box::new(move |name: &str| {
                let want = format!("minecraft:{}", bare(name));
                entries.iter().position(|e| *e == want).map(|i| i as i32)
            })
        }
    })
}

pub(crate) fn build_tag_map(declared: &[(String, Vec<String>)]) -> TagMap {
    let mut map: IndexMap<Identifier, Vec<Tags>> = IndexMap::new();
    for (registry, tags) in VANILLA_TAGS.iter() {
        let Some(resolve) = resolver_for(registry, declared) else {
            tracing::warn!(
                "tag data for {registry}, which configuration doesn't declare — skipped"
            );
            continue;
        };
        map.insert(
            Identifier::new(registry.clone()),
            resolve_registry(registry, tags, &*resolve),
        );
    }
    TagMap(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn built() -> TagMap {
        build_tag_map(&super::super::handshake::registry_entries())
    }

    fn tags_of<'a>(map: &'a TagMap, registry: &str) -> &'a Vec<Tags> {
        map.0
            .get(&Identifier::new(registry))
            .unwrap_or_else(|| panic!("{registry} tags present"))
    }

    fn tag<'a>(list: &'a [Tags], name: &str) -> &'a Tags {
        list.iter()
            .find(|t| t.name == Identifier::new(name))
            .unwrap_or_else(|| panic!("tag {name} present"))
    }

    /// The eager lookups MC 26.2 performs while building item
    /// data-components at configuration finish (extracted from
    /// Item$Properties / DataComponentInitializers bytecode). Any of
    /// these missing refuses a real client's connection.
    #[test]
    fn eagerly_resolved_tags_are_present_and_non_empty() {
        let map = built();
        for (registry, name, at_least) in [
            ("minecraft:damage_type", "minecraft:is_fire", 8),
            ("minecraft:block", "minecraft:mineable/pickaxe", 100),
            ("minecraft:block", "minecraft:mineable/axe", 50),
            ("minecraft:block", "minecraft:mineable/shovel", 20),
            ("minecraft:block", "minecraft:mineable/hoe", 20),
            ("minecraft:entity_type", "minecraft:can_wear_horse_armor", 2),
            ("minecraft:entity_type", "minecraft:can_wear_nautilus_armor", 1),
        ] {
            let t = tag(tags_of(&map, registry), name);
            assert!(
                t.elements.len() >= at_least,
                "{registry}/{name}: {} elements, expected >= {at_least}",
                t.elements.len()
            );
        }
    }

    /// EVERY member of every shipped tag must resolve: an unresolvable
    /// name means version skew between vanilla_tags.json and azalea /
    /// our declared lists (this is what caught the hand-written
    /// damage_type list missing 26.x's sulfur_cube_hot).
    #[test]
    fn every_tag_member_resolves() {
        let declared = super::super::handshake::registry_entries();
        let mut failures = Vec::new();
        for (registry, tags) in VANILLA_TAGS.iter() {
            let resolve = resolver_for(registry, &declared)
                .unwrap_or_else(|| panic!("no resolver for {registry}"));
            for (tag, members) in tags {
                for m in members {
                    if let Some(referenced) = m.strip_prefix('#') {
                        if !tags.contains_key(bare(referenced)) {
                            failures.push(format!("{registry}/{tag}: dangling ref {m}"));
                        }
                    } else if resolve(m).is_none() {
                        failures.push(format!("{registry}/{tag}: unresolvable {m}"));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn hash_references_expand_to_ids() {
        let map = built();
        // mineable/pickaxe includes #minecraft:walls etc. — after
        // expansion it must be pure ids and LARGER than its literal
        // member count would suggest is possible without expansion.
        let t = tag(tags_of(&map, "minecraft:block"), "minecraft:mineable/pickaxe");
        assert!(t.elements.len() > 400, "got {}", t.elements.len());
        // timeline/in_overworld = #universal + day + moon + early_game →
        // every declared timeline entry.
        let t = tag(tags_of(&map, "minecraft:timeline"), "minecraft:in_overworld");
        assert_eq!(t.elements, vec![0, 1, 2, 3]);
    }

    /// Round-trip: the resolved wire ids, mapped back through the
    /// declared damage_type list, must reproduce the JSON's member set
    /// exactly — every member resolved, none dropped, none misindexed.
    #[test]
    fn dynamic_tag_ids_index_the_declared_lists() {
        let map = built();
        let regs = super::super::handshake::registry_entries();
        let (_, damage_types) = regs
            .iter()
            .find(|(n, _)| n == "minecraft:damage_type")
            .unwrap();
        let t = tag(tags_of(&map, "minecraft:damage_type"), "minecraft:is_fire");
        let mut roundtripped: Vec<String> = t
            .elements
            .iter()
            .map(|&e| damage_types[e as usize].clone())
            .collect();
        roundtripped.sort();
        let mut expected: Vec<String> = VANILLA_TAGS["minecraft:damage_type"]["is_fire"]
            .iter()
            .map(|m| format!("minecraft:{}", bare(m)))
            .collect();
        expected.sort();
        assert_eq!(roundtripped, expected);
    }
}

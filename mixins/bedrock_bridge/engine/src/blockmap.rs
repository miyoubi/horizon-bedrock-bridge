//! Bedrock block identity → the Java block name and properties.
//!
//! Bedrock and Java spell both block names and state properties differently:
//! Bedrock's `pillar_axis` is Java's `axis`, its `facing_direction` packs a
//! direction and a face into one integer, and its booleans arrive as `0`/`1`.
//! The conversions here describe the properties that change how a block looks.
//!
//! The output is a list of *candidates*, not a final state: one Bedrock
//! property can correspond to several Java ones, and which applies depends on
//! the block. The caller keeps only the candidates the target block actually
//! declares, so a caller can offer both `facing` and `face` and let the game's
//! registry pick.

use crate::nbt::Tag;

/// A Java block name with candidate property assignments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JavaBlock {
    pub name: String,
    pub properties: Vec<(String, String)>,
}

/// Convert a Bedrock identifier and its state properties to the Java form.
pub fn to_java(bedrock_name: &str, states: Option<&Tag>) -> JavaBlock {
    let mut properties = Vec::new();
    if let Some(fields) = states.and_then(Tag::as_compound) {
        for (key, value) in fields {
            let Some(text) = tag_text(value) else {
                continue;
            };
            properties.extend(property(key, &text));
        }
    }
    JavaBlock {
        name: java_name(bedrock_name),
        properties,
    }
}

/// Render a state value as the text Bedrock would have written.
fn tag_text(tag: &Tag) -> Option<String> {
    Some(match tag {
        Tag::Byte(v) => v.to_string(),
        Tag::Short(v) => v.to_string(),
        Tag::Int(v) => v.to_string(),
        Tag::Long(v) => v.to_string(),
        Tag::Float(v) => v.to_string(),
        Tag::Double(v) => v.to_string(),
        Tag::String(v) => v.clone(),
        Tag::ByteArray(v) => v.iter().map(|b| if *b == 0 { '0' } else { '1' }).collect(),
        _ => return None,
    })
}

fn number(text: &str) -> Option<usize> {
    text.parse().ok()
}

fn truthy(text: &str) -> bool {
    text == "1" || text.eq_ignore_ascii_case("true")
}

fn flag(text: &str) -> String {
    if truthy(text) {
        "true".to_owned()
    } else {
        "false".to_owned()
    }
}

/// Bedrock's `facing_direction`: 0 down, 1 up, 2 north, 3 south, 4 west, 5 east.
const FACING_DIRECTION: [&str; 6] = ["down", "up", "north", "south", "west", "east"];

/// Bedrock's `direction` (stairs, doors): 0 south, 1 west, 2 north, 3 east.
const DIRECTION: [&str; 4] = ["south", "west", "north", "east"];

/// Bedrock's `weirdo_direction` (torches, ladders): 0 east, 1 west, 2 south, 3 north.
const WEIRDO: [&str; 4] = ["east", "west", "south", "north"];

/// The Java properties a Bedrock property can satisfy.
fn property(key: &str, value: &str) -> Vec<(String, String)> {
    let one = |k: &str, v: String| vec![(k.to_owned(), v)];
    match key {
        "pillar_axis" => one("axis", value.to_owned()),
        "upside_down_bit" => one(
            "half",
            if truthy(value) { "top" } else { "bottom" }.to_owned(),
        ),
        "open_bit" => one("open", flag(value)),
        "powered_bit" | "button_pressed_bit" | "toggle_bit" | "rail_data_bit" => {
            one("powered", flag(value))
        }
        "in_wall_bit" => one("in_wall", flag(value)),
        "hanging" => one("hanging", flag(value)),
        "dead_bit" => one("dead", flag(value)),
        "cracked_state" => one("cracked", flag(value)),
        "drag_down" => one("drag", flag(value)),
        "extinguished" => one("lit", (!truthy(value)).to_string()),
        // A vertical face becomes `face=wall` in Java, with `facing` alongside.
        "facing_direction" => {
            let direction = number(value).and_then(|n| FACING_DIRECTION.get(n).copied());
            let mut out = Vec::new();
            if let Some(direction) = direction {
                if let Some(face) = match direction {
                    "down" => Some("floor"),
                    "up" => Some("ceiling"),
                    _ => Some("wall"),
                } {
                    out.push(("face".to_owned(), face.to_owned()));
                }
                out.push(("facing".to_owned(), direction.to_owned()));
            }
            out
        }
        "direction" => one("facing", index(&DIRECTION, value)),
        "weirdo_direction" => one("facing", index(&WEIRDO, value)),
        "cardinal_direction" | "horizontal_facing_direction" => one("facing", value.to_owned()),
        "block_face" => one("face", value.to_owned()),
        "torch_facing_direction" => one(
            "facing",
            match value {
                "west" | "east" | "north" | "south" => value.to_owned(),
                _ => "up".to_owned(),
            },
        ),
        "redstone_signal" => one("power", value.to_owned()),
        "repeater_delay" => one("delay", value.to_owned()),
        "age" => one("age", value.to_owned()),
        "growth" => one("age", value.to_owned()),
        "bite_counter" => one("bites", value.to_owned()),
        "liquid_depth" => one("level", value.to_owned()),
        "composter_fill_level" => one("level", value.to_owned()),
        "respawn_anchor_charge" => one("charges", value.to_owned()),
        "rotation" => one("rotation", value.to_owned()),
        "ground_sign_direction" => one(
            "rotation",
            number(value)
                .map(|n| (16 - n.min(15)) % 16)
                .unwrap_or(0)
                .to_string(),
        ),
        _ => Vec::new(),
    }
}

fn index(table: &[&'static str], value: &str) -> String {
    number(value)
        .and_then(|n| table.get(n).copied())
        .unwrap_or(table[0])
        .to_owned()
}

/// Bedrock identifiers whose Java name differs, plus the families that differ
/// only by a suffix.
fn java_name(bedrock: &str) -> String {
    let key = bedrock.strip_prefix("minecraft:").unwrap_or(bedrock);
    // Wall and standing variants are regular: the wood prefix carries over.
    if let Some(rest) = key.strip_suffix("_standing_sign") {
        return qualified(&format!("{rest}_sign"));
    }
    if let Some(rest) = key.strip_suffix("_wall_sign") {
        return qualified(&format!("{rest}_wall_sign"));
    }
    let mapped = match key {
        "standing_sign" => "oak_sign",
        "wall_sign" => "oak_wall_sign",
        "grass" => "grass_block",
        "deadbush" => "dead_bush",
        "brick_block" => "bricks",
        "waterlily" => "lily_pad",
        "noteblock" => "note_block",
        "reeds" => "sugar_cane",
        "melon_block" => "melon",
        "mob_spawner" | "monster_spawner" => "spawner",
        "lit_redstone_ore" => "redstone_ore",
        "lit_deepslate_redstone_ore" => "deepslate_redstone_ore",
        "lit_redstone_lamp" => "redstone_lamp",
        "lit_furnace" => "furnace",
        "lit_blast_furnace" => "blast_furnace",
        "lit_smoker" => "smoker",
        "hardened_clay" => "terracotta",
        "grass_path" => "dirt_path",
        "lit_pumpkin" => "jack_o_lantern",
        "end_bricks" => "end_stone_bricks",
        "stone_stairs" => "cobblestone_stairs",
        "flowing_water" => "water",
        "flowing_lava" => "lava",
        "tallgrass" => "short_grass",
        "powered_repeater" | "unpowered_repeater" => "repeater",
        "powered_comparator" | "unpowered_comparator" => "comparator",
        "lava_cauldron" | "powder_snow_cauldron" => "cauldron",
        "wooden_door" => "oak_door",
        "wooden_trapdoor" => "oak_trapdoor",
        "fence_gate" => "oak_fence_gate",
        "wooden_button" => "oak_button",
        "wooden_pressure_plate" => "oak_pressure_plate",
        "wooden_slab" => "oak_slab",
        "wooden_stairs" => "oak_stairs",
        "double_wooden_slab" => "oak_slab",
        "planks" => "oak_planks",
        "sapling" => "oak_sapling",
        "leaves" => "oak_leaves",
        "log" | "log2" => "oak_log",
        "fence" => "oak_fence",
        "sign" => "oak_sign",
        "darkoak_sign" => "dark_oak_sign",
        "darkoak_wall_sign" => "dark_oak_wall_sign",
        "spruce_sign" | "birch_sign" | "jungle_sign" | "acacia_sign" => key,
        other => other,
    };
    qualified(mapped)
}

fn qualified(name: &str) -> String {
    if name.contains(':') {
        name.to_owned()
    } else {
        format!("minecraft:{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn states(fields: &[(&str, Tag)]) -> Tag {
        Tag::Compound(
            fields
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn pillar_axis_becomes_axis() {
        let out = to_java(
            "minecraft:oak_log",
            Some(&states(&[("pillar_axis", Tag::String("y".into()))])),
        );
        assert_eq!(out.name, "minecraft:oak_log");
        assert_eq!(out.properties, vec![("axis".to_owned(), "y".to_owned())]);
    }

    #[test]
    fn facing_direction_offers_face_and_facing() {
        // 2 = north, a wall direction.
        let out = to_java(
            "minecraft:oak_button",
            Some(&states(&[("facing_direction", Tag::Int(2))])),
        );
        assert_eq!(
            out.properties,
            vec![
                ("face".to_owned(), "wall".to_owned()),
                ("facing".to_owned(), "north".to_owned()),
            ]
        );
        // 0 = down, which Java calls a floor face.
        let out = to_java(
            "minecraft:lever",
            Some(&states(&[("facing_direction", Tag::Int(0))])),
        );
        assert_eq!(out.properties[0], ("face".to_owned(), "floor".to_owned()));
    }

    #[test]
    fn bedrock_booleans_become_java_text() {
        let out = to_java(
            "minecraft:oak_door",
            Some(&states(&[
                ("open_bit", Tag::Byte(1)),
                ("upside_down_bit", Tag::Byte(0)),
            ])),
        );
        assert_eq!(
            out.properties,
            vec![
                ("open".to_owned(), "true".to_owned()),
                ("half".to_owned(), "bottom".to_owned()),
            ]
        );
    }

    #[test]
    fn extinguished_inverts_and_growth_renames() {
        let out = to_java(
            "minecraft:campfire",
            Some(&states(&[("extinguished", Tag::Byte(0))])),
        );
        assert_eq!(out.properties, vec![("lit".to_owned(), "true".to_owned())]);
        let out = to_java("minecraft:wheat", Some(&states(&[("growth", Tag::Int(3))])));
        assert_eq!(out.properties, vec![("age".to_owned(), "3".to_owned())]);
    }

    #[test]
    fn renames_and_sign_families() {
        assert_eq!(
            to_java("minecraft:grass", None).name,
            "minecraft:grass_block"
        );
        assert_eq!(
            to_java("minecraft:spruce_standing_sign", None).name,
            "minecraft:spruce_sign"
        );
        assert_eq!(
            to_java("minecraft:acacia_wall_sign", None).name,
            "minecraft:acacia_wall_sign"
        );
        assert_eq!(to_java("minecraft:oak_log", None).name, "minecraft:oak_log");
    }

    #[test]
    fn unknown_properties_are_dropped() {
        let out = to_java(
            "minecraft:stone",
            Some(&states(&[("weird_bedrock_only_bit", Tag::Byte(1))])),
        );
        assert!(out.properties.is_empty());
    }
}

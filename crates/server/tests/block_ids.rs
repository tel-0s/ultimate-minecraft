use azalea_block::{blocks, BlockState, BlockTrait};
use azalea_registry::builtin::BlockKind;

#[test]
fn print_block_state_ids() {
    let ids: Vec<(&str, u32)> = vec![
        ("air", blocks::Air{}.as_block_state().into()),
        ("stone", blocks::Stone{}.as_block_state().into()),
        ("dirt", blocks::Dirt{}.as_block_state().into()),
        ("grass_block(snowy=false)", u32::from(blocks::GrassBlock{snowy:false}.as_block_state())),
        ("sand", blocks::Sand{}.as_block_state().into()),
        ("bedrock", blocks::Bedrock{}.as_block_state().into()),
        ("oak_log(y)", u32::from(blocks::OakLog{axis:azalea_block::properties::Axis::Y}.as_block_state())),
        // Water: check both ways of obtaining the ID
        ("water(level=0)", u32::from(blocks::Water{level:0.into()}.as_block_state())),
        ("water(default via BlockKind)", u32::from(BlockState::from(BlockKind::Water))),
        // Lava
        ("lava(level=0)", u32::from(blocks::Lava{level:0.into()}.as_block_state())),
        ("lava(default via BlockKind)", u32::from(BlockState::from(BlockKind::Lava))),
    ];
    for (name, id) in &ids {
        eprintln!("{}: {}", name, id);
    }

    // Verify our LAVA constant matches azalea
    let lava_id = u32::from(blocks::Lava{level:0.into()}.as_block_state());
    assert_eq!(lava_id, ultimate_server::block::LAVA.0 as u32,
        "LAVA constant doesn't match azalea BlockState");
}

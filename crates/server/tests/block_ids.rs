use azalea_block::{blocks, BlockTrait};

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
    ];
    for (name, id) in &ids {
        eprintln!("{}: {}", name, id);
    }
}

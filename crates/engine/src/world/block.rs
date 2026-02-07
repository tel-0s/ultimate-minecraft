/// Opaque block identifier. The engine stores these without interpreting them.
/// Game-specific layers assign meaning to specific IDs (e.g. 0 = air, 4 = sand).
///
/// The only semantic the engine enforces is that `BlockId::AIR` (0) is the
/// "empty" block: chunk sections filled entirely with AIR are deallocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct BlockId(pub u16);

impl BlockId {
    /// The universal "empty" block.
    pub const AIR: BlockId = BlockId(0);

    pub const fn new(id: u16) -> Self {
        Self(id)
    }
}

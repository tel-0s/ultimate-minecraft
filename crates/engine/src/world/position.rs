/// Absolute block position in the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockPos {
    pub x: i64,
    pub y: i64,
    pub z: i64,
}

impl BlockPos {
    pub const fn new(x: i64, y: i64, z: i64) -> Self {
        Self { x, y, z }
    }

    /// The chunk this block belongs to.
    pub const fn chunk(&self) -> ChunkPos {
        ChunkPos {
            x: (self.x >> 4) as i32,
            z: (self.z >> 4) as i32,
        }
    }

    /// Position within the chunk (0..16 each axis, 0..max_y for y).
    pub const fn local(&self) -> LocalBlockPos {
        LocalBlockPos {
            x: (self.x & 0xF) as u8,
            y: self.y,
            z: (self.z & 0xF) as u8,
        }
    }

    /// The six cardinal neighbors.
    pub const fn neighbors(&self) -> [BlockPos; 6] {
        [
            Self::new(self.x + 1, self.y, self.z),
            Self::new(self.x - 1, self.y, self.z),
            Self::new(self.x, self.y + 1, self.z),
            Self::new(self.x, self.y - 1, self.z),
            Self::new(self.x, self.y, self.z + 1),
            Self::new(self.x, self.y, self.z - 1),
        ]
    }
}

/// Chunk column position (each chunk is 16x16 blocks horizontally).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkPos {
    pub x: i32,
    pub z: i32,
}

impl ChunkPos {
    pub const fn new(x: i32, z: i32) -> Self {
        Self { x, z }
    }

    pub const fn block_origin(&self, y: i64) -> BlockPos {
        BlockPos::new((self.x as i64) << 4, y, (self.z as i64) << 4)
    }
}

/// Block position local to a chunk (x, z in 0..16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalBlockPos {
    pub x: u8,
    pub y: i64,
    pub z: u8,
}

impl LocalBlockPos {
    pub const fn section_index(&self) -> i32 {
        (self.y >> 4) as i32
    }

    pub const fn section_local_y(&self) -> u8 {
        (self.y.rem_euclid(16)) as u8
    }
}

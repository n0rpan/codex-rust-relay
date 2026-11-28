//! The source icons shared by the native sidebar and the phone session list.
#[derive(Clone, Copy)]
pub enum Cartridge {
    Codex,
    Claude,
    Antigravity,
    Api,
    Worker,
}
impl Cartridge {
    /// A seven-by-seven bitmap, with each row encoded most-significant pixel first.
    pub fn pixels(self) -> [u8; 7] {
        match self {
            Self::Codex => [
                0b0100010, 0b0010100, 0b0111110, 0b1101011, 0b1111111, 0b0101010, 0b1000001,
            ],
            Self::Claude => [
                0b0001000, 0b0101010, 0b0011100, 0b1111111, 0b0011100, 0b0101010, 0b0001000,
            ],
            Self::Antigravity => [
                0b0000110, 0b0011001, 0b0100010, 0b1010101, 0b0100010, 0b1001100, 0b0110000,
            ],
            Self::Api => [
                0b0001000, 0b0011100, 0b0110110, 0b1100011, 0b0110110, 0b0011100, 0b0001000,
            ],
            Self::Worker => [
                0b0001000, 0b0011100, 0b1111111, 0b1010101, 0b1111111, 0b0010100, 0b0100010,
            ],
        }
    }
}

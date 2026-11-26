//! Visible surfaces affected by one drain of a profile's inbox.
#[derive(Default, Debug)]
pub struct Changed {
    bits: u32,
    transcripts: Vec<String>,
    replaced: Vec<String>,
}
impl Changed {
    pub const THREADS: u32 = 1;
    pub const ASK: u32 = 1 << 1;
    pub const CONNECTION: u32 = 1 << 2;
    pub const NOTICE: u32 = 1 << 3;
    pub fn mark(&mut self, bits: u32) {
        self.bits |= bits;
    }
    pub fn transcript(&mut self, id: &str) {
        if !self.transcripts.iter().any(|t| t == id) {
            self.transcripts.push(id.to_owned());
        }
    }
    /// All item revisions restart when a reader replaces an existing history.
    pub(crate) fn replace_transcript(&mut self, id: &str) {
        self.transcript(id);
        if !self.replaced.iter().any(|t| t == id) {
            self.replaced.push(id.to_owned());
        }
    }
    pub fn replaced_threads(&self) -> &[String] {
        &self.replaced
    }
    pub fn merge(&mut self, other: Changed) {
        self.bits |= other.bits;
        for id in other.replaced {
            self.replace_transcript(&id);
        }
        for id in other.transcripts {
            self.transcript(&id);
        }
    }
    pub fn dirty_threads(&self) -> &[String] {
        &self.transcripts
    }
    pub fn surfaces(&self) -> u32 {
        self.bits
    }
    pub fn affects_catalog(&self) -> bool {
        self.bits & (Self::THREADS | Self::ASK | Self::CONNECTION) != 0
    }
    pub fn affects_view(&self, selected: Option<&str>) -> bool {
        self.bits != 0 || selected.is_some_and(|id| self.transcripts.iter().any(|t| t == id))
    }
}

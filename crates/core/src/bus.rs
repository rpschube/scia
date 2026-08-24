//! The feature bus: a triple buffer carrying the latest [`FeatureSnapshot`]
//! from the DSP thread to consumers. The writer never blocks the reader and
//! the reader never blocks the writer; the reader always sees the freshest
//! fully-published snapshot.

use crate::features::FeatureSnapshot;

/// Publishing half of the feature bus, held by the DSP thread.
pub struct FeatureWriter(triple_buffer::Input<FeatureSnapshot>);

/// Reading half of the feature bus, held by a consumer (renderer, output
/// stream, test).
pub struct FeatureReader(triple_buffer::Output<FeatureSnapshot>);

impl FeatureWriter {
    /// Publish `snapshot` as the new latest value. Wait-free; overwrites any
    /// snapshot the reader has not yet observed.
    pub fn publish(&mut self, snapshot: FeatureSnapshot) {
        self.0.write(snapshot);
    }
}

impl FeatureReader {
    /// The freshest published snapshot. Never blocks; between publishes it
    /// returns the same snapshot.
    pub fn latest(&mut self) -> &FeatureSnapshot {
        self.0.read()
    }

    /// Generation counter of the freshest published snapshot.
    pub fn generation(&mut self) -> u64 {
        self.0.read().generation
    }
}

/// Create a connected [`FeatureWriter`] / [`FeatureReader`] pair, seeded with a
/// default (generation 0) snapshot.
#[must_use]
pub fn feature_bus() -> (FeatureWriter, FeatureReader) {
    let (input, output) = triple_buffer::TripleBuffer::new(&FeatureSnapshot::default()).split();
    (FeatureWriter(input), FeatureReader(output))
}

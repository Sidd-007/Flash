/// A contiguous byte range owned by at most one worker.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Segment {
    /// Next byte to take from the server. Bytes below this have been handed
    /// to the writer.
    pub received: u64,
    /// Bytes below this (back to the segment's start) are on disk.
    pub written: u64,
    /// Exclusive end. Shrinks when another worker splits this segment.
    pub end: u64,
    owned: bool,
}

/// Which bytes still need downloading, and who is downloading them.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    segments: Vec<Segment>,
}

impl Plan {
    pub fn new(ranges: impl IntoIterator<Item = (u64, u64)>) -> Self {
        let segments = ranges
            .into_iter()
            .filter(|&(start, end)| start < end)
            .map(|(start, end)| Segment { received: start, written: start, end, owned: false })
            .collect();
        Self { segments }
    }

    /// `[0, total)` cut into at most `parts` equal ranges of at least `min` bytes.
    pub fn even(total: u64, parts: usize, min: u64) -> Self {
        let parts = (parts as u64).min(total / min.max(1)).max(1);
        let step = total / parts;
        Self::new((0..parts).map(|i| {
            let end = if i == parts - 1 { total } else { (i + 1) * step };
            (i * step, end)
        }))
    }

    pub fn segment(&self, id: usize) -> &Segment {
        &self.segments[id]
    }

    pub fn remaining(&self) -> Vec<(u64, u64)> {
        self.segments
            .iter()
            .filter(|s| s.written < s.end)
            .map(|s| (s.written, s.end))
            .collect()
    }

    pub fn remaining_bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.end - s.written).sum()
    }

    pub fn views(&self) -> Vec<crate::job::SegmentView> {
        self.segments
            .iter()
            .filter(|s| s.written < s.end)
            .map(|s| crate::job::SegmentView {
                start: s.written,
                end: s.end,
                active: s.owned && s.received < s.end,
            })
            .collect()
    }

    pub fn is_complete(&self) -> bool {
        self.segments.iter().all(|s| s.written >= s.end)
    }

    /// Gives a worker something to do: an unowned segment if there is one,
    /// otherwise the back half of the busiest segment (dynamic segmentation).
    /// Returns `None` when nothing is left that is worth splitting.
    pub fn assign(&mut self, min_split: u64) -> Option<usize> {
        if let Some(id) = self.segments.iter().position(|s| !s.owned && s.received < s.end) {
            self.segments[id].owned = true;
            return Some(id);
        }

        let (id, left) = self
            .segments
            .iter()
            .enumerate()
            .map(|(i, s)| (i, s.end - s.received))
            .max_by_key(|&(_, left)| left)?;
        if left / 2 < min_split {
            return None;
        }
        let mid = self.segments[id].received + left / 2;
        let end = std::mem::replace(&mut self.segments[id].end, mid);
        self.segments.push(Segment { received: mid, written: mid, end, owned: true });
        Some(self.segments.len() - 1)
    }

    /// Reserves up to `len` bytes of segment `id` for writing. Returns the
    /// file offset, how many bytes to keep, and whether the segment is done.
    pub fn claim(&mut self, id: usize, len: usize) -> (u64, usize, bool) {
        let s = &mut self.segments[id];
        let n = (len as u64).min(s.end.saturating_sub(s.received));
        let offset = s.received;
        s.received += n;
        (offset, n as usize, s.received >= s.end)
    }

    pub fn mark_written(&mut self, id: usize, n: u64) {
        self.segments[id].written += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    #[test]
    fn even_split_covers_everything() {
        let plan = Plan::even(10 * MIB + 3, 4, MIB);
        let r = plan.remaining();
        assert_eq!(r.len(), 4);
        assert_eq!(r[0].0, 0);
        assert_eq!(r[3].1, 10 * MIB + 3);
        assert!(r.windows(2).all(|w| w[0].1 == w[1].0));
    }

    #[test]
    fn small_files_get_fewer_segments() {
        assert_eq!(Plan::even(MIB / 2, 8, MIB).remaining(), vec![(0, MIB / 2)]);
        assert_eq!(Plan::even(3 * MIB, 8, MIB).remaining().len(), 3);
    }

    #[test]
    fn assign_then_split_the_busiest() {
        let mut plan = Plan::new([(0, 8 * MIB)]);
        assert_eq!(plan.assign(MIB), Some(0));
        plan.claim(0, MIB as usize);
        // 7 MiB left on segment 0 -> new segment takes the back half.
        assert_eq!(plan.assign(MIB), Some(1));
        let mid = MIB + 7 * MIB / 2;
        assert_eq!(plan.segment(0).end, mid);
        assert_eq!(plan.segment(1).received, mid);
        assert_eq!(plan.segment(1).end, 8 * MIB);
    }

    #[test]
    fn no_split_below_minimum() {
        let mut plan = Plan::new([(0, MIB)]);
        assert_eq!(plan.assign(MIB), Some(0));
        assert_eq!(plan.assign(MIB), None);
    }

    #[test]
    fn claim_stops_at_shrunk_end() {
        let mut plan = Plan::new([(0, 100)]);
        plan.assign(1);
        assert_eq!(plan.claim(0, 60), (0, 60, false));
        assert_eq!(plan.claim(0, 60), (60, 40, true));
        assert_eq!(plan.claim(0, 60), (100, 0, true));
        assert!(!plan.is_complete());
        plan.mark_written(0, 100);
        assert!(plan.is_complete());
        assert_eq!(plan.remaining_bytes(), 0);
    }
}

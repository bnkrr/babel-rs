/// Admission limits for learned protocol state. Existing entries may always
/// refresh or retract. Zero disables admission of new entries of that kind.
/// These limits do not cap origins, feasibility history, or process memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceLimits {
    pub max_neighbors: usize,
    pub max_candidates: usize,
    pub max_candidates_per_neighbor: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_neighbors: 256,
            max_candidates: 16_384,
            max_candidates_per_neighbor: 4_096,
        }
    }
}

/// Current occupancy and cumulative admission rejections. Candidate rejections
/// count finite Update TLVs; neighbor rejections count packets containing Hello.
/// If both candidate limits are full, the per-neighbor reason takes precedence.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResourceStatus {
    pub limits: ResourceLimits,
    pub candidates: usize,
    pub sources: usize,
    pub pending_requests: usize,
    pub unreachable: usize,
    pub rejected_neighbors: u64,
    pub rejected_candidates_global: u64,
    pub rejected_candidates_per_neighbor: u64,
}

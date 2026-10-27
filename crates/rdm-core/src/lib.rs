//! Segmented, resumable HTTP download engine.
//!
//! A [`Job`] probes the server, splits the file into byte ranges, downloads
//! them over parallel connections straight into a preallocated `.part` file,
//! and keeps a `.rdm.json` sidecar so an interrupted download can resume.

mod checksum;
mod filename;
pub mod hls;
mod job;
mod limit;
pub mod pattern;
mod plan;
mod probe;
mod state;
pub mod video;

use std::time::Duration;

pub use checksum::Checksum;
pub use hls::{HlsJob, HlsRequest};
pub use job::{Job, JobInfo, Outcome, Progress, Request, SegmentView, relink};
pub use limit::{RateLimiter, parse_rate};
pub use video::{VideoJob, VideoRequest};

/// An HTTP client tuned for segmented downloads.
pub fn default_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        // Separate TCP connections are the whole point; HTTP/2 would
        // multiplex every segment onto one connection.
        .http1_only()
        .user_agent(concat!("rdm/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .build()
}

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};
use rdm_core::{Checksum, HlsJob, HlsRequest, Job, JobInfo, Outcome, Progress, RateLimiter, Request};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use tokio_util::sync::CancellationToken;

/// Fast, resumable downloads over parallel connections.
///
/// Press Ctrl-C to pause; run the same command again to resume.
/// URLs may be batch patterns like "https://x.com/img[001-050].jpg".
/// HLS playlists (.m3u8) are downloaded as one video file.
#[derive(Parser)]
#[command(name = "flash", version)]
struct Args {
    /// URL to download
    url: String,
    /// Parallel connections (1-32)
    #[arg(short, long, default_value_t = 8)]
    connections: usize,
    /// Folder to save into
    #[arg(short, long, default_value = ".")]
    dir: PathBuf,
    /// File name (default: from the server or the URL)
    #[arg(short, long)]
    output: Option<String>,
    /// Extra request header, e.g. -H "Cookie: id=123" (repeatable)
    #[arg(short = 'H', long = "header", value_name = "HEADER")]
    headers: Vec<String>,
    /// Speed limit, e.g. 500K or 2M (bytes per second)
    #[arg(short, long, value_parser = parse_limit)]
    limit: Option<u64>,
    /// Verify the file afterwards: sha256:<hex> or md5:<hex>
    #[arg(long)]
    checksum: Option<Checksum>,
    /// Treat the URL as an HLS playlist (detected automatically for .m3u8)
    #[arg(long)]
    hls: bool,
}

fn parse_limit(s: &str) -> Result<u64, String> {
    rdm_core::parse_rate(s).ok_or_else(|| "use a rate like 500K or 2M".into())
}

/// A prepared file or HLS download.
enum AnyJob {
    File(Job),
    Hls(HlsJob),
}

impl AnyJob {
    fn info(&self) -> &JobInfo {
        match self {
            Self::File(j) => j.info(),
            Self::Hls(j) => j.info(),
        }
    }

    fn progress(&self) -> Arc<Progress> {
        match self {
            Self::File(j) => j.progress(),
            Self::Hls(j) => j.progress(),
        }
    }

    async fn run(self, cancel: CancellationToken) -> Result<Outcome> {
        match self {
            Self::File(j) => j.run(cancel).await,
            Self::Hls(j) => j.run(cancel).await,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Args::parse()).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<ExitCode> {
    let urls = rdm_core::pattern::expand(&args.url)?;
    if urls.len() > 1 {
        println!("Batch: {} files", urls.len());
    }

    let cancel = CancellationToken::new();
    tokio::spawn({
        let cancel = cancel.clone();
        async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        }
    });

    let limiters: Vec<_> = args.limit.map(|l| Arc::new(RateLimiter::new(l))).into_iter().collect();
    let headers = parse_headers(&args.headers)?;
    let client = rdm_core::default_client()?;
    let single = urls.len() == 1;
    for url in urls {
        let job = if args.hls || rdm_core::hls::looks_like_hls(&url) {
            AnyJob::Hls(
                HlsJob::prepare(client.clone(), HlsRequest {
                    url,
                    dir: args.dir.clone(),
                    file_name: args.output.clone().filter(|_| single),
                    connections: args.connections,
                    headers: headers.clone(),
                    limiters: limiters.clone(),
                })
                .await?,
            )
        } else {
            AnyJob::File(
                Job::prepare(client.clone(), Request {
                    url,
                    dir: args.dir.clone(),
                    file_name: args.output.clone().filter(|_| single),
                    connections: args.connections,
                    headers: headers.clone(),
                    limiters: limiters.clone(),
                    checksum: args.checksum.clone().filter(|_| single),
                })
                .await?,
            )
        };
        let code = download(job, cancel.clone()).await?;
        if code != ExitCode::SUCCESS {
            return Ok(code);
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn download(job: AnyJob, cancel: CancellationToken) -> Result<ExitCode> {
    let info = job.info().clone();
    print_summary(&info);

    let bar = progress_bar(&info);
    let progress = job.progress();
    let ticker = tokio::spawn({
        let (bar, progress) = (bar.clone(), progress.clone());
        async move {
            loop {
                bar.set_position(progress.downloaded());
                let conns = format!("{} conn", progress.active_connections());
                bar.set_message(match progress.parts() {
                    Some((done, total)) => format!("segment {done}/{total} · {conns}"),
                    None => conns,
                });
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    });

    let outcome = job.run(cancel).await;
    ticker.abort();
    bar.set_position(progress.downloaded());

    match outcome {
        Ok(Outcome::Completed(path)) => {
            bar.finish_and_clear();
            println!("✓ Saved {} ({})", path.display(), HumanBytes(progress.downloaded()));
            Ok(ExitCode::SUCCESS)
        }
        Ok(Outcome::Paused { resumable }) => {
            bar.abandon();
            if resumable {
                println!("⏸ Paused. Run the same command again to resume.");
            } else {
                println!("⏹ Stopped. This server can't resume, so the next run starts over.");
            }
            Ok(ExitCode::from(130))
        }
        Err(e) => {
            bar.abandon();
            if info.resumable {
                eprintln!("Progress is saved; run the same command again to resume.");
            }
            Err(e)
        }
    }
}

fn parse_headers(raw: &[String]) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for h in raw {
        let (name, value) = h
            .split_once(':')
            .with_context(|| format!("header must look like \"Name: value\", got {h:?}"))?;
        map.append(
            HeaderName::from_bytes(name.trim().as_bytes())
                .with_context(|| format!("invalid header name {name:?}"))?,
            HeaderValue::from_str(value.trim())
                .with_context(|| format!("invalid value for header {name:?}"))?,
        );
    }
    Ok(map)
}

fn print_summary(info: &JobInfo) {
    let size = info.total.map_or("unknown size".into(), |t| HumanBytes(t).to_string());
    println!("↓ {} ({size}) → {}", info.file_name, info.path.display());
    if info.resumed_from > 0 {
        println!("  resuming from {}", HumanBytes(info.resumed_from));
    }
    if info.resumable {
        println!("  up to {} connections", info.connections);
    } else {
        println!("  server doesn't support ranges: 1 connection, no resume");
    }
}

fn progress_bar(info: &JobInfo) -> ProgressBar {
    let (bar, template) = match info.total {
        Some(total) => (
            ProgressBar::new(total),
            "{spinner:.green} [{bar:40.cyan/blue}] {bytes}/{total_bytes} {binary_bytes_per_sec} ETA {eta} · {msg}",
        ),
        None => (
            ProgressBar::no_length(),
            "{spinner:.green} {bytes} {binary_bytes_per_sec} · {msg}",
        ),
    };
    bar.set_style(ProgressStyle::with_template(template).unwrap().progress_chars("█▉▊▋▌▍▎▏ "));
    // Start at the resumed position so the speed estimate ignores old bytes.
    bar.set_position(info.resumed_from);
    bar.reset_eta();
    bar
}

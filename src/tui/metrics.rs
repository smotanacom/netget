//! Per-instance throughput: one sample a second, a rate, a sparkline.
//!
//! The snapshot carries byte totals; this keeps the deltas. Sampled on the
//! 1s stats tick, so a sample is bytes per second and thirty samples are the
//! last half minute — enough to see whether an instance is alive at a glance.

use std::collections::VecDeque;

/// Samples kept per instance.
pub const SAMPLES: usize = 30;

const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

#[derive(Debug, Clone, Default)]
pub struct Throughput {
    last: Option<(u64, u64)>,
    /// `(received, sent)` per second, oldest first.
    samples: VecDeque<(u64, u64)>,
}

impl Throughput {
    /// Record the current totals. The first call only establishes a baseline.
    pub fn sample(&mut self, received_total: u64, sent_total: u64) {
        if let Some((rx, tx)) = self.last {
            self.samples.push_back((
                received_total.saturating_sub(rx),
                sent_total.saturating_sub(tx),
            ));
            while self.samples.len() > SAMPLES {
                self.samples.pop_front();
            }
        }
        self.last = Some((received_total, sent_total));
    }

    /// The most recent `(received, sent)` per-second sample.
    pub fn rate(&self) -> (u64, u64) {
        self.samples.back().copied().unwrap_or((0, 0))
    }

    pub fn samples(&self) -> impl Iterator<Item = u64> + '_ {
        self.samples.iter().map(|(rx, tx)| rx + tx)
    }

    /// Whether anything moved in the sampled window.
    pub fn is_idle(&self) -> bool {
        self.samples().all(|s| s == 0)
    }

    /// The last `width` samples as bar glyphs, scaled to the window's peak.
    /// Missing samples (a young instance) render as spaces so the line
    /// fills in from the right as history accrues.
    pub fn sparkline(&self, width: usize) -> String {
        if width == 0 {
            return String::new();
        }
        let values: Vec<u64> = self.samples().collect();
        let window: Vec<u64> = values.iter().rev().take(width).rev().copied().collect();
        let peak = window.iter().copied().max().unwrap_or(0);
        let mut out = String::with_capacity(width * 3);
        for _ in window.len()..width {
            out.push(' ');
        }
        for value in window {
            let glyph = if peak == 0 || value == 0 {
                BARS[0]
            } else {
                // 1..=7 for anything non-zero, so a trickle is visible.
                let level = ((value * 7) / peak).clamp(1, 7) as usize;
                BARS[level]
            };
            out.push(glyph);
        }
        out
    }
}

/// `340`, `1.2K`, `4.5M` — compact bytes for a 40-column pane.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["", "K", "M", "G", "T"];
    if n < 1000 {
        return n.to_string();
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

/// `1.2K/s`.
pub fn human_rate(n: u64) -> String {
    format!("{}/s", human_bytes(n))
}

/// `2m13s`, `1h04m`, `12s`.
pub fn human_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{:02}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}

/// `HH:MM:SS` local time for a unix-millisecond stamp.
pub fn clock(unix_ms: u64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(unix_ms as i64)
        .single()
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".to_string())
}

/// `HH:MM:SS` now.
pub fn clock_now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

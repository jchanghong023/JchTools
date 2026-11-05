use chrono::{DateTime, SecondsFormat, Utc};
use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::Duration;

const RETENTION: Duration = Duration::from_mins(20);
const HEADER: &str = "timestamp_utc,cpu_percent,memory_percent,physical_disk_number,disk_busy_percent,disk_read_MiB_per_s,disk_write_MiB_per_s\n";

#[derive(Default)]
pub(crate) struct Disk {
    pub(crate) number: u32,
    pub(crate) busy: Option<f64>,
    pub(crate) read: Option<f64>,
    pub(crate) write: Option<f64>,
}

pub(crate) struct Sample {
    pub(crate) at: DateTime<Utc>,
    pub(crate) cpu: Option<f64>,
    pub(crate) memory: Option<f64>,
    pub(crate) disks: Vec<Disk>,
}

struct Metric(Option<f64>);
impl fmt::Display for Metric {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.filter(|value| value.is_finite()) {
            Some(value) => write!(formatter, "{value:.3}"),
            None => formatter.write_str("N/A"),
        }
    }
}

impl Sample {
    fn csv(&self) -> Result<String, fmt::Error> {
        let timestamp = self.at.to_rfc3339_opts(SecondsFormat::Millis, true);
        let mut text = String::with_capacity(128 * self.disks.len().max(1));
        if self.disks.is_empty() {
            writeln!(
                text,
                "{timestamp},{},{},N/A,N/A,N/A,N/A",
                Metric(self.cpu),
                Metric(self.memory)
            )?;
        } else {
            for disk in &self.disks {
                writeln!(
                    text,
                    "{timestamp},{},{},{},{},{},{}",
                    Metric(self.cpu),
                    Metric(self.memory),
                    disk.number,
                    Metric(disk.busy),
                    Metric(disk.read),
                    Metric(disk.write),
                )?;
            }
        }
        Ok(text)
    }
}

struct Entry {
    elapsed: Duration,
    csv: String,
}

pub(crate) struct Window {
    entries: VecDeque<Entry>,
}

impl Window {
    pub(crate) fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(601),
        }
    }

    /// 使用单调时钟淘汰，不因系统校时导致历史无限积累；边界时刻仍在窗口中。
    pub(crate) fn record(
        &mut self,
        elapsed: Duration,
        sample: &Sample,
        path: &Path,
    ) -> io::Result<()> {
        let csv = sample.csv().map_err(io::Error::other)?;
        self.entries.push_back(Entry { elapsed, csv });
        let cutoff = elapsed.saturating_sub(RETENTION);
        while self
            .entries
            .front()
            .is_some_and(|entry| entry.elapsed < cutoff)
        {
            self.entries.pop_front();
        }
        // 同一文件原位更新，不产生历史分卷、临时输出或后备目录。
        let mut writer = BufWriter::new(File::create(path)?);
        writer.write_all(HEADER.as_bytes())?;
        for entry in &self.entries {
            writer.write_all(entry.csv.as_bytes())?;
        }
        writer.flush()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 夹具失败直接报告测试失败。
    use super::*;

    fn sample(seconds: i64) -> Sample {
        Sample {
            at: DateTime::from_timestamp(1_800_000_000 + seconds, 0).unwrap(),
            cpu: Some(12.5),
            memory: Some(60.0),
            disks: vec![Disk {
                number: 0,
                busy: Some(25.0),
                read: Some(1.5),
                write: None,
            }],
        }
    }

    // SPL-03 / SPL-04：缺失值输出、立即可读及20分钟循环淘汰边界。
    #[test]
    fn csv_is_visible_immediately_and_evicts_only_samples_older_than_twenty_minutes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pressure.csv");
        let mut window = Window::new();
        let first = sample(0);
        let first_timestamp = first.at.to_rfc3339_opts(SecondsFormat::Millis, true);
        window.record(Duration::ZERO, &first, &path).unwrap();
        let initial = std::fs::read_to_string(&path).unwrap();
        assert_eq!(initial.lines().count(), 2, "未满窗口已能读到样本");
        assert!(initial.contains("12.500,60.000,0,25.000,1.500,N/A"));
        window
            .record(Duration::from_secs(1200), &sample(1200), &path)
            .unwrap();
        let boundary = std::fs::read_to_string(&path).unwrap();
        assert!(boundary.contains(&first_timestamp), "恰好20分钟仍保留");
        window
            .record(Duration::from_secs(1202), &sample(1202), &path)
            .unwrap();
        let rolled = std::fs::read_to_string(&path).unwrap();
        assert!(!rolled.contains(&first_timestamp), "超出20分钟被淘汰");
        assert_eq!(rolled.lines().count(), 3, "持续写入而非到时停止");
        assert!(rolled.contains(&sample(1202).at.to_rfc3339_opts(SecondsFormat::Millis, true)));
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            1,
            "只有单个输出文件"
        );
    }
}

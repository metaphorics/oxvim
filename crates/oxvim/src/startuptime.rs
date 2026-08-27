//! `--startuptime` timing log.
//!
//! `profile.c`/`time_msg` records one milestone per startup step and
//! `time_finish` writes them out when the process leaves. Each line carries the
//! elapsed time since process start and the time since the previous mark, both
//! in `SSS.mmm` milliseconds, so the log reads like upstream's.
//!
//! Marks are taken unconditionally, because whether a log was requested is
//! only known after the arguments are parsed. They cost one `Instant` and one
//! static label each, with no allocation until the file is written.

use std::fs;
use std::io::{self, Write};
use std::time::Instant;

/// Upstream records a couple of dozen marks; oxvim's startup has far fewer,
/// and a full array simply stops recording rather than reallocating.
const MAX_MARKS: usize = 16;

/// Startup milestones, timed from process start.
pub struct StartupTimer {
    origin: Instant,
    marks: [(&'static str, Instant); MAX_MARKS],
    count: usize,
}

impl StartupTimer {
    /// Begin timing at process start and record the opening mark.
    #[must_use]
    pub fn start() -> Self {
        let origin = Instant::now();
        let mut timer = Self { origin, marks: [("", origin); MAX_MARKS], count: 0 };
        timer.mark("--- OXVIM STARTING ---");
        timer
    }

    /// Record one milestone.
    pub fn mark(&mut self, label: &'static str) {
        if self.count == MAX_MARKS {
            return;
        }
        self.marks[self.count] = (label, Instant::now());
        self.count += 1;
    }

    /// Write the log to `path`, closing it with the final mark.
    pub fn finish(mut self, path: &str) -> io::Result<()> {
        self.mark("--- OXVIM STARTED ---");
        let mut file = io::BufWriter::new(fs::File::create(path)?);
        writeln!(file, "--- Startup times for process: Primary (or UI client) ---")?;
        writeln!(file)?;
        writeln!(file, "times in msec")?;
        writeln!(file, " clock   self+sourced   self:  sourced script")?;
        writeln!(file, " clock   elapsed:              other lines")?;
        writeln!(file)?;
        let mut previous = self.origin;
        for (label, at) in &self.marks[..self.count] {
            let clock = at.duration_since(self.origin).as_secs_f64() * 1000.0;
            let elapsed = at.duration_since(previous).as_secs_f64() * 1000.0;
            previous = *at;
            writeln!(file, "{clock:07.3}  {elapsed:07.3}: {label}")?;
        }
        file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_carries_the_header_and_every_mark_in_order() {
        let path =
            std::env::temp_dir().join(format!("oxvim-startuptime-{}.log", std::process::id()));
        let mut timer = StartupTimer::start();
        timer.mark("parsing arguments");
        timer.mark("opening buffers");
        timer.finish(path.to_str().unwrap()).unwrap();

        let log = fs::read_to_string(&path).unwrap();
        let _removed = fs::remove_file(&path);
        assert!(
            log.starts_with("--- Startup times for process: Primary (or UI client) ---\n"),
            "{log}"
        );
        assert!(log.contains("times in msec"), "{log}");
        let mark_lines = log
            .lines()
            .filter(|line| line.starts_with(|c: char| c.is_ascii_digit()))
            .collect::<Vec<_>>();
        let marks = mark_lines
            .iter()
            .filter_map(|line| line.split_once(": ").map(|(_, label)| label))
            .collect::<Vec<_>>();
        assert_eq!(
            marks,
            [
                "--- OXVIM STARTING ---",
                "parsing arguments",
                "opening buffers",
                "--- OXVIM STARTED ---"
            ]
        );
        // Every mark line has a cumulative and a delta column, and the
        // cumulative column never goes backwards.
        let mut last = -1.0_f64;
        for line in &mark_lines {
            let mut columns = line.split_whitespace();
            let clock = columns.next().unwrap().parse::<f64>().unwrap_or_else(|_| panic!("{line}"));
            let delta = columns
                .next()
                .unwrap()
                .trim_end_matches(':')
                .parse::<f64>()
                .unwrap_or_else(|_| panic!("{line}"));
            assert!(clock >= last, "{line}");
            assert!(delta >= 0.0, "{line}");
            last = clock;
        }
    }

    #[test]
    fn a_full_mark_table_stops_recording_instead_of_growing() {
        let mut timer = StartupTimer::start();
        for _ in 0..MAX_MARKS * 2 {
            timer.mark("extra");
        }
        assert_eq!(timer.count, MAX_MARKS);
    }
}

//! The `history` command: the recent past from the daemon's history files,
//! as sparklines per fan, or as JSON lines.
//!
//! Read-only, needs no root and does not talk to the daemon: it reads the
//! files `run` appends to (see [`crate::historyfile`]).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use svalka_core::history::Line;
use svalka_core::state::FanSnapshot;

use crate::historyfile;

/// Characters in a sparkline. Each stands for an equal slice of the time
/// shown.
const WIDTH: usize = 60;

/// The index of the highest bar in [`BARS`].
const TOP_BAR: u8 = 6;

/// From lowest to highest. The full block is left out: in many terminal fonts
/// it fills the whole line, so rows above and below run into each other.
const BARS: [char; TOP_BAR as usize + 1] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇'];

/// What to show.
pub struct Options<'a> {
    /// Only this fan; all fans when `None`.
    pub fan: Option<&'a str>,
    /// Only the ticks this recent; everything when `None`.
    pub since: Option<Duration>,
    /// JSON lines instead of sparklines.
    pub json: bool,
}

/// Prints the history in `state_dir` to `out`. `now` is the current time in
/// milliseconds since the Unix epoch.
///
/// # Errors
///
/// Fails when the files cannot be read, there is no history in the chosen
/// time, or `fan` is not in it.
pub fn run(
    state_dir: &Path,
    options: &Options,
    now: u64,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let lines = historyfile::read(state_dir)
        .with_context(|| format!("cannot read the history in {}", state_dir.display()))?;
    let Some(newest) = lines.last().map(|line| line.unix_millis) else {
        bail!(
            "no history in {}; `svalka run` writes it while it runs",
            state_dir.display()
        );
    };
    let lines = recent(lines, options.since, now);
    if lines.is_empty() {
        bail!(
            "no history in the last {}; the newest tick is {} old",
            human(options.since.unwrap_or_default()),
            human(age(newest, now))
        );
    }
    let fans = fan_names(&lines, options.fan)?;
    if options.json {
        for line in &lines {
            writeln!(out, "{}", only_fans(line, &fans).to_json()?)?;
        }
    } else {
        write!(out, "{}", render(&lines, &fans, now))?;
    }
    Ok(())
}

fn recent(lines: Vec<Line>, since: Option<Duration>, now: u64) -> Vec<Line> {
    let Some(since) = since else { return lines };
    let since = u64::try_from(since.as_millis()).unwrap_or(u64::MAX);
    let start = now.saturating_sub(since);
    lines
        .into_iter()
        .filter(|line| line.unix_millis >= start)
        .collect()
}

/// The fans to show: `wanted` if the history has it, else every fan in it.
fn fan_names(lines: &[Line], wanted: Option<&str>) -> anyhow::Result<Vec<String>> {
    let all: BTreeSet<&String> = lines.iter().flat_map(|line| line.fans.keys()).collect();
    match wanted {
        None => Ok(all.into_iter().cloned().collect()),
        Some(fan) if all.iter().any(|name| *name == fan) => Ok(vec![fan.to_owned()]),
        Some(fan) => bail!(
            "no fan {fan:?} in the history; it has {}",
            all.into_iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn only_fans(line: &Line, fans: &[String]) -> Line {
    Line {
        fans: line
            .fans
            .iter()
            .filter(|(name, _)| fans.contains(name))
            .map(|(name, fan)| (name.clone(), fan.clone()))
            .collect(),
        ..line.clone()
    }
}

/// A heading with the time covered, then per fan a sparkline of its
/// temperature, duty and rpm.
fn render(lines: &[Line], fans: &[String], now: u64) -> String {
    let first = lines.first().map_or(0, |line| line.unix_millis);
    let newest = lines.last().map_or(0, |line| line.unix_millis);
    let mut text = format!(
        "Last {}, {} ticks; the newest is {} old.\n",
        human(age(first, newest)),
        lines.len(),
        human(age(newest, now))
    );
    let timeline = Timeline::new(first, newest);
    for fan in fans {
        text.push('\n');
        text.push_str(&render_fan(lines, fan, &timeline));
    }
    text
}

fn render_fan(lines: &[Line], fan: &str, timeline: &Timeline) -> String {
    let samples = |value: fn(&FanSnapshot) -> Option<f64>| -> Vec<(u64, f64)> {
        lines
            .iter()
            .filter_map(|line| Some((line.unix_millis, value(line.fans.get(fan)?)?)))
            .collect()
    };
    let output = lines
        .iter()
        .rev()
        .find_map(|line| line.fans.get(fan))
        .map_or("", |snapshot| snapshot.output.as_str());
    let source = lines
        .iter()
        .rev()
        .find_map(|line| line.fans.get(fan)?.source.clone());
    let mut text = format!("{fan}  {output}\n");
    let rows = [
        Row {
            label: "temp",
            unit: "°C",
            decimals: 1,
            min_span: 5.0,
            samples: samples(|fan| fan.celsius),
            note: source.map(|source| format!(" ({source})")),
        },
        Row {
            label: "duty",
            unit: "%",
            decimals: 0,
            min_span: 10.0,
            samples: samples(|fan| Some(fan.duty)),
            note: None,
        },
        Row {
            label: "rpm",
            unit: "rpm",
            decimals: 0,
            min_span: 100.0,
            samples: samples(|fan| fan.rpm.map(f64::from)),
            note: None,
        },
    ];
    for row in rows {
        let _ = writeln!(text, "  {}", row.render(timeline));
    }
    text
}

/// One sparkline and its range.
struct Row {
    label: &'static str,
    unit: &'static str,
    decimals: usize,
    /// The least the sparkline spans, in the row's unit (see [`range`]).
    min_span: f64,
    /// Timestamp and value, oldest first.
    samples: Vec<(u64, f64)>,
    /// After the latest value, e.g. which source it came from.
    note: Option<String>,
}

impl Row {
    fn render(&self, timeline: &Timeline) -> String {
        let values = self.samples.iter().map(|&(_, value)| value);
        let (Some(low), Some(high), Some(&(_, latest))) = (
            values.clone().reduce(f64::min),
            values.reduce(f64::max),
            self.samples.last(),
        ) else {
            return format!("{:<4}  no readings", self.label);
        };
        let (bottom, top) = range(low, high, self.min_span);
        let line = sparkline(&timeline.slices(&self.samples), bottom, top);
        let number = |value: f64| format!("{value:.0$}", self.decimals);
        format!(
            "{:<4}  {line}  {}–{} {}, now {} {}{}",
            self.label,
            number(low),
            number(high),
            self.unit,
            number(latest),
            self.unit,
            self.note.as_deref().unwrap_or("")
        )
    }
}

/// What the lowest and highest bar stand for: the readings' own range, so
/// any change shows, but at least `min_span`, so a steady reading that wobbles
/// a little stays a flat line along the bottom.
fn range(low: f64, high: f64, min_span: f64) -> (f64, f64) {
    (low, high.max(low + min_span))
}

/// The time shown, cut into [`WIDTH`] equal slices.
struct Timeline {
    first: u64,
    span: u64,
}

impl Timeline {
    fn new(first: u64, newest: u64) -> Self {
        Self {
            first,
            span: newest - first + 1,
        }
    }

    /// The highest value in each slice. A sample holds until the next one (or
    /// the end) if that comes within twice the usual spacing: rpm, read every
    /// 10 s, fills the slices between readbacks. A longer gap, such as while
    /// the daemon was stopped, leaves its slices empty (`None`).
    fn slices(&self, samples: &[(u64, f64)]) -> Vec<Option<f64>> {
        let hold = usual_spacing(samples).saturating_mul(2);
        let end = self.first + self.span - 1;
        let mut slices = vec![None; WIDTH];
        for (i, &(millis, value)) in samples.iter().enumerate() {
            let until = match samples.get(i + 1) {
                Some(&(next, _)) if next - millis <= hold => (next - 1).max(millis),
                None if end.saturating_sub(millis) <= hold => end,
                _ => millis,
            };
            for slot in &mut slices[self.index(millis)..=self.index(until)] {
                *slot = Some(slot.map_or(value, |highest: f64| highest.max(value)));
            }
        }
        slices
    }

    fn index(&self, millis: u64) -> usize {
        let offset = u128::from(millis.saturating_sub(self.first));
        let index = offset * WIDTH as u128 / u128::from(self.span);
        usize::try_from(index).unwrap_or(WIDTH - 1).min(WIDTH - 1)
    }
}

/// The median time between samples, in milliseconds; 0 for fewer than two.
fn usual_spacing(samples: &[(u64, f64)]) -> u64 {
    let mut gaps: Vec<u64> = samples
        .windows(2)
        .map(|pair| pair[1].0 - pair[0].0)
        .collect();
    gaps.sort_unstable();
    gaps.get(gaps.len().saturating_sub(1) / 2)
        .copied()
        .unwrap_or(0)
}

/// One bar per slice between `bottom` and `top`, a space for an empty slice.
fn sparkline(slices: &[Option<f64>], bottom: f64, top: f64) -> String {
    slices
        .iter()
        .map(|slice| match slice {
            None => ' ',
            Some(value) => {
                let fraction = ((value - bottom) / (top - bottom)).clamp(0.0, 1.0);
                // fraction is within 0..=1, so the index is within 0..=TOP_BAR.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let index = (fraction * f64::from(TOP_BAR)).round() as usize;
                BARS[index]
            }
        })
        .collect()
}

fn age(then: u64, now: u64) -> Duration {
    Duration::from_millis(now.saturating_sub(then))
}

/// E.g. `0.4 s`, `45 s`, `29 min 30 s`, `1 h 2 min`.
fn human(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0..10 => format!("{:.1} s", duration.as_secs_f64()),
        10..60 => format!("{seconds} s"),
        60..3600 => format!("{} min {} s", seconds / 60, seconds % 60),
        _ => format!("{} h {} min", seconds / 3600, seconds % 3600 / 60),
    }
}

/// Parses `--since`: a whole number followed by `s`, `m` or `h`.
///
/// # Errors
///
/// Fails on anything else.
pub fn parse_since(text: &str) -> Result<Duration, String> {
    let error = || format!("invalid time {text:?}; use a number with s, m or h, e.g. \"5m\"");
    let (number, unit) = text.split_at(text.len().saturating_sub(1));
    let seconds_per_unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return Err(error()),
    };
    let number: u64 = number.parse().map_err(|_| error())?;
    Ok(Duration::from_secs(number * seconds_per_unit))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use svalka_core::history::VERSION;
    use svalka_core::state::FanMode;

    use super::*;

    /// A tick at `second` with the cooler at `celsius` and `duty`; every
    /// tenth second is a readback with `rpm`.
    fn tick(second: u64, celsius: f64, duty: f64, rpm: u32) -> Line {
        let readback = second.is_multiple_of(10);
        Line {
            version: VERSION,
            unix_millis: second * 1000,
            readback,
            sensors: BTreeMap::from([("cpu".into(), Some(celsius))]),
            fans: BTreeMap::from([(
                "cpu_cooler".into(),
                FanSnapshot {
                    output: "nct6798 pwm2".into(),
                    mode: FanMode::Normal,
                    source: Some("cpu".into()),
                    celsius: Some(celsius),
                    smoothed_celsius: Some(celsius),
                    effective_celsius: Some(celsius),
                    duty,
                    pwm_raw: 0,
                    pwm_enable: readback.then_some(1),
                    rpm: readback.then_some(rpm),
                },
            )]),
        }
    }

    #[test]
    fn a_sparkline_runs_from_the_lowest_bar_to_the_highest() {
        let slices = [Some(0.0), Some(50.0), None, Some(100.0), Some(150.0)];

        assert_eq!(sparkline(&slices, 0.0, 100.0), "▁▄ ▇▇");
    }

    #[test]
    fn each_slice_shows_its_highest_value_and_long_gaps_stay_empty() {
        let timeline = Timeline::new(0, 59_999);
        let samples = [(0, 40.0), (500, 70.0), (30_000, 50.0)];

        let slices = timeline.slices(&samples);

        assert_eq!(slices.len(), WIDTH);
        assert_eq!(slices[0], Some(70.0));
        assert_eq!(slices[1], None);
        assert_eq!(slices[30], Some(50.0));
    }

    #[test]
    fn a_sample_holds_until_the_next_one_comes_at_the_usual_spacing() {
        let timeline = Timeline::new(0, 59_999);
        // Every 10 s, then a 30 s gap.
        let samples = [(0, 1.0), (10_000, 2.0), (20_000, 3.0), (50_000, 4.0)];

        let slices = timeline.slices(&samples);

        assert_eq!(slices[5], Some(1.0));
        assert_eq!(slices[15], Some(2.0));
        assert_eq!(slices[20], Some(3.0));
        assert_eq!(slices[21], None, "a gap of three readbacks stays a gap");
        assert_eq!(slices[50], Some(4.0));
        assert_eq!(slices[59], Some(4.0), "the last one holds to the end");
    }

    #[test]
    fn a_steady_reading_is_a_flat_line_not_noise() {
        assert_eq!(range(40.0, 40.5, 5.0), (40.0, 45.0));
        assert_eq!(range(40.0, 80.0, 5.0), (40.0, 80.0));
    }

    #[test]
    fn render_shows_each_row_with_its_range_and_latest_value() {
        let lines: Vec<Line> = (0..60)
            .map(|second| {
                let hot = (20..40).contains(&second);
                let (celsius, duty) = if hot { (70.0, 80.0) } else { (45.0, 35.0) };
                tick(second, celsius, duty, if hot { 1800 } else { 1000 })
            })
            .collect();

        let text = render(&lines, &["cpu_cooler".into()], 60_400);

        assert!(
            text.starts_with("Last 59 s, 60 ticks; the newest is 1.4 s old.\n"),
            "{text}"
        );
        assert!(text.contains("cpu_cooler  nct6798 pwm2\n"), "{text}");
        let temp = text.lines().find(|line| line.contains("temp")).unwrap();
        assert!(temp.contains("45.0–70.0 °C, now 45.0 °C (cpu)"), "{temp}");
        assert!(
            temp.contains(&format!("{}{}", "▁".repeat(20), "▇".repeat(20))),
            "{temp}"
        );
        let rpm = text.lines().find(|line| line.contains("rpm")).unwrap();
        assert!(rpm.contains("1000–1800 rpm, now 1000 rpm"), "{rpm}");
        // rpm only on every tenth tick, held until the next.
        assert!(
            rpm.contains(&format!("{}{}", "▁".repeat(20), "▇".repeat(20))),
            "{rpm}"
        );
    }

    #[test]
    fn a_fan_without_readings_says_so() {
        let mut line = tick(1, 45.0, 30.0, 0);
        line.fans.get_mut("cpu_cooler").unwrap().celsius = None;

        let text = render(&[line], &["cpu_cooler".into()], 1000);

        assert!(text.contains("temp  no readings"), "{text}");
    }

    #[test]
    fn since_keeps_only_the_recent_ticks() {
        let lines: Vec<Line> = (0..100).map(|second| tick(second, 45.0, 30.0, 0)).collect();

        let kept = recent(lines, Some(Duration::from_secs(30)), 100_000);

        assert_eq!(kept.len(), 30);
        assert_eq!(kept[0].unix_millis, 70_000);
    }

    #[test]
    fn an_unknown_fan_lists_the_known_ones() {
        let error = fan_names(&[tick(0, 45.0, 30.0, 0)], Some("pump")).unwrap_err();

        assert_eq!(
            error.to_string(),
            r#"no fan "pump" in the history; it has cpu_cooler"#
        );
    }

    #[test]
    fn human_durations() {
        assert_eq!(human(Duration::from_millis(400)), "0.4 s");
        assert_eq!(human(Duration::from_secs(45)), "45 s");
        assert_eq!(human(Duration::from_secs(1770)), "29 min 30 s");
        assert_eq!(human(Duration::from_mins(62)), "1 h 2 min");
    }

    #[test]
    fn since_takes_seconds_minutes_and_hours() {
        assert_eq!(parse_since("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_since("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_since("1h"), Ok(Duration::from_secs(3600)));
        for text in ["", "5", "m", "1.5m", "-5m", "5d"] {
            assert!(parse_since(text).is_err(), "{text:?} should not parse");
        }
    }
}

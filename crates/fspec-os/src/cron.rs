//! Five-field cron expressions, parsed once and rendered as a systemd `OnCalendar=` value or as
//! launchd `StartCalendarInterval` dictionaries.

use anyhow::{Result, bail};

/// Allowed values per field; `None` means every value.
pub type Field = Option<Vec<u8>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    pub minute: Field,
    pub hour: Field,
    pub dom: Field,
    pub month: Field,
    /// 0 is Sunday; cron's 7 is folded into it.
    pub dow: Field,
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

impl Schedule {
    pub fn parse(expr: &str) -> Result<Schedule> {
        let expr = expr.trim();
        let expanded = match expr {
            "@hourly" => "0 * * * *",
            "@daily" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            e if e.starts_with('@') => {
                bail!("schedule `{e}` is not supported, use @hourly, @daily, @weekly, @monthly or five cron fields")
            }
            e => e,
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        let [minute, hour, dom, month, dow] = fields.as_slice() else {
            bail!("schedule `{expr}` needs five cron fields (minute hour day-of-month month day-of-week)");
        };
        let s = Schedule {
            minute: field("minute", minute, 0, 59, &[])?,
            hour: field("hour", hour, 0, 23, &[])?,
            dom: field("day-of-month", dom, 1, 31, &[])?,
            month: field("month", month, 1, 12, &MONTHS)?,
            dow: dow_field(dow)?,
        };
        if s.dom.is_some() && s.dow.is_some() {
            bail!(
                "schedule `{expr}` restricts both day-of-month and day-of-week; cron runs on either, \
                 and no init here can express that"
            );
        }
        Ok(s)
    }

    /// The value for systemd's `OnCalendar=`.
    pub fn on_calendar(&self) -> String {
        let dow = match &self.dow {
            None => String::new(),
            Some(days) => {
                // Monday first, so Mon..Fri stays a range and Sunday sorts last.
                let mut days: Vec<u8> = days.iter().map(|d| if *d == 0 { 7 } else { *d }).collect();
                days.sort_unstable();
                format!(
                    "{} ",
                    compress(&days, |d| ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][*d as usize - 1].into())
                )
            }
        };
        let num = |f: &Field| match f {
            None => "*".to_string(),
            Some(v) => compress(v, |n| format!("{n:02}")),
        };
        format!("{dow}*-{}-{} {}:{}:00", num(&self.month), num(&self.dom), num(&self.hour), num(&self.minute))
    }

    /// One dictionary per combination of listed values, in the order launchd should read them.
    pub fn calendar_intervals(&self) -> Result<Vec<Interval>> {
        const LIMIT: usize = 64;
        let size = [&self.minute, &self.hour, &self.dom, &self.month, &self.dow]
            .iter()
            .map(|f| f.as_ref().map_or(1, Vec::len))
            .product::<usize>();
        if size > LIMIT {
            bail!("schedule expands to {size} launchd calendar entries, more than the {LIMIT} fspec writes");
        }
        let each = |f: &Field| -> Vec<Option<u8>> {
            match f {
                None => vec![None],
                Some(v) => v.iter().copied().map(Some).collect(),
            }
        };
        let mut out = Vec::new();
        for month in each(&self.month) {
            for dom in each(&self.dom) {
                for dow in each(&self.dow) {
                    for hour in each(&self.hour) {
                        for minute in each(&self.minute) {
                            out.push(Interval { minute, hour, day: dom, month, weekday: dow });
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

/// One `StartCalendarInterval` dictionary; a missing key matches every value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interval {
    pub minute: Option<u8>,
    pub hour: Option<u8>,
    pub day: Option<u8>,
    pub month: Option<u8>,
    pub weekday: Option<u8>,
}

/// Runs of three or more become `a..b`, everything else a comma list.
fn compress(values: &[u8], name: impl Fn(&u8) -> String) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < values.len() {
        let mut j = i;
        while j + 1 < values.len() && values[j + 1] == values[j] + 1 {
            j += 1;
        }
        if j - i >= 2 {
            parts.push(format!("{}..{}", name(&values[i]), name(&values[j])));
        } else {
            parts.extend(values[i..=j].iter().map(&name));
        }
        i = j + 1;
    }
    parts.join(",")
}

fn dow_field(text: &str) -> Result<Field> {
    let Some(mut values) = expand("day-of-week", text, 0, 7, &DAYS)? else { return Ok(None) };
    for v in &mut values {
        if *v == 7 {
            *v = 0;
        }
    }
    values.sort_unstable();
    values.dedup();
    Ok(if values.len() == 7 { None } else { Some(values) })
}

fn field(label: &str, text: &str, lo: u8, hi: u8, names: &[&str]) -> Result<Field> {
    let Some(values) = expand(label, text, lo, hi, names)? else { return Ok(None) };
    Ok(if values.len() == usize::from(hi - lo) + 1 { None } else { Some(values) })
}

/// Every value the field lists, sorted. `None` for a bare `*`.
fn expand(label: &str, text: &str, lo: u8, hi: u8, names: &[&str]) -> Result<Option<Vec<u8>>> {
    if text == "*" {
        return Ok(None);
    }
    let value = |s: &str| -> Result<u8> {
        let n = match s.parse::<u8>() {
            Ok(n) => n,
            Err(_) => match names.iter().position(|n| n.eq_ignore_ascii_case(s)) {
                // Month names start at 1, day names at 0.
                Some(i) => i as u8 + if names.len() == 12 { 1 } else { 0 },
                None => {
                    bail!("{label} `{s}` in `{text}` is not a number{}", if names.is_empty() { "" } else { " or name" })
                }
            },
        };
        if !(lo..=hi).contains(&n) {
            bail!("{label} {n} in `{text}` is outside {lo}..{hi}");
        }
        Ok(n)
    };
    let mut out = Vec::new();
    for item in text.split(',') {
        let (base, step) = match item.split_once('/') {
            Some((b, s)) => match s.parse::<u8>() {
                Ok(n) if n > 0 => (b, n),
                _ => bail!("{label} step `{s}` in `{text}` must be a positive number"),
            },
            None => (item, 1),
        };
        let (from, to) = match base {
            "*" => (lo, hi),
            b => match b.split_once('-') {
                Some((a, z)) => (value(a)?, value(z)?),
                None if step == 1 => {
                    let n = value(b)?;
                    (n, n)
                }
                None => {
                    bail!("{label} `{item}` in `{text}`: a step needs `*` or a range, like `*/{step}` or `0-30/{step}`")
                }
            },
        };
        if from > to {
            bail!("{label} range `{base}` in `{text}` runs backwards");
        }
        out.extend((from..=to).step_by(usize::from(step)));
    }
    out.sort_unstable();
    out.dedup();
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal(expr: &str) -> String {
        Schedule::parse(expr).unwrap().on_calendar()
    }

    #[test]
    fn systemd_calendar() {
        assert_eq!(cal("* * * * *"), "*-*-* *:*:00");
        assert_eq!(cal("@hourly"), "*-*-* *:00:00");
        assert_eq!(cal("@daily"), "*-*-* 00:00:00");
        assert_eq!(cal("@weekly"), "Sun *-*-* 00:00:00");
        assert_eq!(cal("@monthly"), "*-*-01 00:00:00");
        assert_eq!(cal("*/15 * * * *"), "*-*-* *:00,15,30,45:00");
        assert_eq!(cal("30 2 * * 1-5"), "Mon..Fri *-*-* 02:30:00");
        assert_eq!(cal("0 9,17 * * sat,sun"), "Sat,Sun *-*-* 09,17:00:00");
        assert_eq!(cal("0 0 1,15 jan-mar *"), "*-01..03-01,15 00:00:00");
        assert_eq!(cal("5 4 * * 7"), "Sun *-*-* 04:05:00");
        assert_eq!(cal("0-59 0-23 * * 0-7"), "*-*-* *:*:00");
    }

    #[test]
    fn rejects_what_it_cannot_express() {
        let err = |e: &str| Schedule::parse(e).unwrap_err().to_string();
        assert!(err("@reboot").contains("@reboot"));
        assert!(err("* * * *").contains("five cron fields"));
        assert!(err("0 0 1 * 1").contains("both day-of-month and day-of-week"));
        assert!(err("60 * * * *").contains("outside 0..59"));
        assert!(err("*/0 * * * *").contains("step"));
        assert!(err("5/10 * * * *").contains("needs `*` or a range"));
        assert!(err("30-10 * * * *").contains("backwards"));
        assert!(err("x * * * *").contains("not a number"));
    }

    #[test]
    fn launchd_intervals() {
        let s = Schedule::parse("0 9,17 * * 1-2").unwrap();
        let got = s.calendar_intervals().unwrap();
        assert_eq!(got.len(), 4);
        assert_eq!(got[0], Interval { minute: Some(0), hour: Some(9), day: None, month: None, weekday: Some(1) });
        assert_eq!(got[3].hour, Some(17));
        assert_eq!(got[3].weekday, Some(2));

        let every = Schedule::parse("* * * * *").unwrap().calendar_intervals().unwrap();
        assert_eq!(every, vec![Interval { minute: None, hour: None, day: None, month: None, weekday: None }]);

        let err = Schedule::parse("*/2 */2 * * *").unwrap().calendar_intervals().unwrap_err();
        assert!(err.to_string().contains("360"), "{err}");
    }
}

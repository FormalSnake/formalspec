//! Timers on inits without a timer unit: `/etc/cron.d/fspec-<name>` for system timers and a
//! managed block in the applying user's crontab for user timers. The schedule is written as the
//! spec has it, a five-field expression or `@hourly`, `@daily`, `@weekly`, `@monthly`.
//!
//! cronie and vixie-cron read `/etc/cron.d`. dcron, busybox crond and snooze do not, and fspec
//! does not probe which cron is installed.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use fspec_core::apply::{Cx, Op};
use fspec_core::journal::{Journal, Undo, remove_any};
use fspec_core::resource::Timer;
use fspec_core::runner::Cmd;
use serde_json::{Map, json};

use crate::cron::Schedule;
use crate::{check_line, check_name, read_or_empty, write_file};

/// Run-parts based crons skip file names with a dot.
pub(crate) fn check(t: &Timer) -> Result<()> {
    check_name("timer", &t.name)?;
    if t.name.contains('.') {
        bail!("timer name `{}` holds a `.`, which cron ignores in /etc/cron.d file names", t.name);
    }
    line(t).map(drop)
}

fn path(t: &Timer) -> String {
    format!("/etc/cron.d/fspec-{}", t.name)
}

/// `<schedule> <command>`; cron reads `%` in a command as a newline.
fn line(t: &Timer) -> Result<String> {
    Schedule::parse(&t.schedule).with_context(|| format!("timer {}", t.name))?;
    check_line("exec", &t.exec)?;
    let exec = t.exec.trim();
    if exec.is_empty() {
        bail!("exec is empty");
    }
    let schedule = t.schedule.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut words: Vec<String> = t.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    for w in &words {
        check_line("env", w)?;
    }
    let env = if words.is_empty() {
        String::new()
    } else {
        words.insert(0, "env".into());
        format!("{} ", shell_words::join(&words))
    };
    Ok(format!("{schedule} {env}{}", exec.replace('%', "\\%")))
}

pub(crate) fn render_file(t: &Timer) -> Result<String> {
    let line = line(t)?;
    let (schedule, command) = split(&line);
    Ok(format!("{}{schedule} root {command}\n", crate::initsvc::HEADER))
}

/// The schedule is one token for `@daily`, five otherwise.
fn split(line: &str) -> (&str, &str) {
    let n = if line.starts_with('@') { 1 } else { 5 };
    let end = line.match_indices(' ').nth(n - 1).map_or(line.len(), |(i, _)| i);
    (&line[..end], line[end..].trim_start())
}

pub(crate) fn render_block(t: &Timer) -> Result<String> {
    Ok(format!("# BEGIN fspec:{0}\n{1}\n# END fspec:{0}\n", t.name, line(t)?))
}

/// The applying user's crontab, empty when there is none.
fn read_crontab(cx: &Cx) -> Result<String> {
    let out = cx.runner.run(&Cmd::new("crontab").arg("-l"))?;
    Ok(if out.status == 0 { out.stdout } else { String::new() })
}

fn block_of<'a>(crontab: &'a str, name: &str) -> Option<&'a str> {
    let begin = format!("# BEGIN fspec:{name}\n");
    let end = format!("# END fspec:{name}\n");
    let start = crontab.find(&begin)?;
    let stop = crontab[start..].find(&end)? + start + end.len();
    Some(&crontab[start..stop])
}

fn splice(crontab: &str, name: &str, block: Option<&str>) -> String {
    let mut out = match block_of(crontab, name) {
        Some(old) => crontab.replacen(old, "", 1),
        None => crontab.to_string(),
    };
    if let Some(block) = block {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(block);
    }
    out
}

pub(crate) fn drift(cx: &Cx, t: &Timer) -> Result<Vec<String>> {
    if !t.user {
        let p = path(t);
        return Ok(match read_or_empty(&cx.resolve(&p)?) {
            None => vec![format!("{p} is missing")],
            Some(on_disk) if on_disk != render_file(t)? => vec![format!("{p} was edited")],
            Some(_) => vec![],
        });
    }
    let crontab = read_crontab(cx)?;
    // A rooted run only logs commands, so no crontab ever comes back.
    if crontab.is_empty() && cx.paths.root != Path::new("/") {
        return Ok(vec![]);
    }
    Ok(match block_of(&crontab, &t.name) {
        None => vec![format!("crontab entry fspec:{} is missing", t.name)],
        Some(b) if b != render_block(t)? => vec![format!("crontab entry fspec:{} was edited", t.name)],
        Some(_) => vec![],
    })
}

pub(crate) fn commit(cx: &Cx, journal: &mut Journal, op: &Op, t: &Timer) -> Result<Map<String, serde_json::Value>> {
    let mut rec = Map::new();
    if !t.user {
        let dest = cx.resolve(&path(t))?;
        journal.backup(&dest)?;
        if op.is_removal() {
            remove_any(&dest)?;
        } else {
            write_file(&dest, &render_file(t)?)?;
            fs::set_permissions(&dest, fs::Permissions::from_mode(0o644))?;
            rec.insert("file".into(), json!(path(t)));
        }
        return Ok(rec);
    }

    let old = cx.runner.run(&Cmd::new("crontab").arg("-l"))?;
    let had = old.status == 0;
    let block = if op.is_removal() { None } else { Some(render_block(t)?) };
    let new = splice(&old.stdout, &t.name, block.as_deref());
    journal.record(Undo::Run {
        cmd: if had { Cmd::new("crontab").arg("-").stdin(old.stdout.clone()) } else { Cmd::new("crontab").arg("-r") },
    })?;
    cx.runner.check(&Cmd::new("crontab").arg("-").stdin(new))?;
    if !op.is_removal() {
        rec.insert("crontab".into(), json!(format!("fspec:{}", t.name)));
    }
    Ok(rec)
}

#[cfg(test)]
mod tests;

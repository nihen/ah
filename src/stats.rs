//! `ah stats`: one screen that sums up a year of agent sessions.
use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike};
use serde::Serialize;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::cli::{self, Field, FieldFilter, SearchMode, SortOrder};
use crate::color;
use crate::pipeline;
use crate::resolver::ResolveOpts;

/// Width of the screen, in terminal columns.
const WIDTH: usize = 60;
/// Agents listed by name; the rest are summed in one row.
const TOP_AGENTS: usize = 5;
/// Columns of the longest agent bar.
const BAR_WIDTH: usize = 30;
/// Longest agent name shown in full; longer ones are cut (custom agents
/// can have any name).
const AGENT_NAME_WIDTH: usize = 12;
/// Longest model name shown in full on an era line.
const MODEL_NAME_WIDTH: usize = WIDTH - 13;

/// Terminal columns `text` takes (a CJK character takes two).
fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// `text` cut to `max` columns, the last one an ellipsis when cut.
fn fit(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_string();
    }
    let mut cut = String::new();
    for c in text.chars() {
        if width(&cut) + UnicodeWidthChar::width(c).unwrap_or(0) + 1 > max {
            break;
        }
        cut.push(c);
    }
    cut.push('…');
    cut
}

/// `text` padded with spaces to `columns`.
fn padded(text: &str, columns: usize) -> String {
    format!("{text}{}", " ".repeat(columns.saturating_sub(width(text))))
}

/// One year of sessions, as `ah stats` shows it. A session counts when it
/// has a prompt in the year, and each prompt counts on the local day it was
/// sent.
#[derive(Debug, Serialize)]
pub struct Stats {
    pub year: i32,
    /// First and last day with a prompt.
    #[serde(serialize_with = "iso_date")]
    pub first_day: NaiveDate,
    #[serde(serialize_with = "iso_date")]
    pub last_day: NaiveDate,
    pub prompts: usize,
    pub sessions: usize,
    pub active_days: usize,
    /// Longest run of consecutive days with a prompt.
    pub streak: usize,
    /// Most prompts in one session.
    pub longest_session: usize,
    /// Hour of the day (local) with the most prompts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_hour: Option<u32>,
    /// Agents by prompts, most first.
    pub agents: Vec<AgentShare>,
    /// Number of distinct models that answered.
    pub models: usize,
    /// Runs of months led by one model (the model whose sessions took the
    /// most prompts that month).
    pub eras: Vec<Era>,
    pub persona: Persona,
    /// Prompts counted at their session's modification time, because the
    /// agent records no time for them.
    pub undated_prompts: usize,
    /// Prompts per day.
    #[serde(serialize_with = "iso_date_keys")]
    pub days: BTreeMap<NaiveDate, Day>,
}

fn iso_date<S: serde::Serializer>(date: &NaiveDate, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(date)
}

fn iso_date_keys<S: serde::Serializer>(
    days: &BTreeMap<NaiveDate, Day>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_map(days.iter().map(|(date, day)| (date.to_string(), day)))
}

#[derive(Debug, Serialize)]
pub struct AgentShare {
    pub agent: String,
    pub prompts: usize,
    pub sessions: usize,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Era {
    /// First and last month (1–12).
    pub from: u32,
    pub to: u32,
    pub model: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Persona {
    pub title: &'static str,
    pub line: String,
}

#[derive(Debug, Serialize)]
pub struct Day {
    pub prompts: usize,
    /// Agent with the most prompts that day.
    pub agent: String,
}

/// What `build` reads of one session.
pub struct SessionInput {
    pub agent: String,
    /// Model that answered last (empty if unknown).
    pub model: String,
    /// Every model that answered.
    pub models: Vec<String>,
    /// One entry per user prompt (Unix seconds; `None` if not recorded).
    pub prompt_times: Vec<Option<i64>>,
    /// Stands in for the prompts without a time.
    pub modified: Option<NaiveDateTime>,
}

fn local_time(unix: i64) -> Option<NaiveDateTime> {
    Local
        .timestamp_opt(unix, 0)
        .single()
        .map(|t| t.naive_local())
}

/// The agent or model with the highest count; ties go to the name that
/// sorts first, so the result does not depend on map order.
fn leader(counts: &HashMap<&str, usize>) -> Option<String> {
    counts
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(name, _)| name.to_string())
}

/// Sum up the prompts of `year` sent up to `now` (Unix seconds; a record
/// dated later is a clock error, not activity). `None` when there are none.
pub fn build(year: i32, now: i64, sessions: &[SessionInput]) -> Option<Stats> {
    // Local times repeat when clocks are set back, so recorded times are
    // compared as instants; only a session's modification time, which
    // stands in for a missing one, is compared as a local time.
    let now_local = local_time(now);
    let mut prompts = 0;
    let mut session_count = 0;
    let mut longest_session = 0;
    let mut undated_prompts = 0;
    let mut hours = [0usize; 24];
    let mut night_prompts = 0;
    let mut timed_prompts = 0;
    let mut models: BTreeSet<&str> = BTreeSet::new();
    let mut by_agent: HashMap<&str, (usize, usize)> = HashMap::new();
    let mut by_day: BTreeMap<NaiveDate, HashMap<&str, usize>> = BTreeMap::new();
    let mut by_month: BTreeMap<u32, HashMap<&str, usize>> = BTreeMap::new();

    for session in sessions {
        let mut in_year = 0;
        for time in &session.prompt_times {
            if time.is_some_and(|t| t > now) {
                continue;
            }
            let dated = time.and_then(local_time);
            let stand_in = || session.modified.filter(|at| Some(*at) <= now_local);
            let Some(at) = dated.or_else(stand_in).filter(|at| at.year() == year) else {
                continue;
            };
            in_year += 1;
            if dated.is_some() {
                hours[at.hour() as usize] += 1;
                timed_prompts += 1;
                if at.hour() < 5 {
                    night_prompts += 1;
                }
            } else {
                undated_prompts += 1;
            }
            *by_day
                .entry(at.date())
                .or_default()
                .entry(&session.agent)
                .or_default() += 1;
            if !session.model.is_empty() {
                *by_month
                    .entry(at.month())
                    .or_default()
                    .entry(&session.model)
                    .or_default() += 1;
            }
        }
        if in_year == 0 {
            continue;
        }
        prompts += in_year;
        session_count += 1;
        longest_session = longest_session.max(in_year);
        models.extend(session.models.iter().map(String::as_str));
        let agent = by_agent.entry(&session.agent).or_default();
        agent.0 += in_year;
        agent.1 += 1;
    }
    let (first_day, last_day) = (*by_day.keys().next()?, *by_day.keys().next_back()?);

    let mut streak = 0;
    let mut run = 0;
    let mut previous: Option<NaiveDate> = None;
    for day in by_day.keys() {
        run = if previous.and_then(|p| p.succ_opt()) == Some(*day) {
            run + 1
        } else {
            1
        };
        streak = streak.max(run);
        previous = Some(*day);
    }

    let mut agents: Vec<AgentShare> = by_agent
        .into_iter()
        .map(|(agent, (prompts, sessions))| AgentShare {
            agent: agent.to_string(),
            prompts,
            sessions,
        })
        .collect();
    agents.sort_by(|a, b| {
        b.prompts
            .cmp(&a.prompts)
            .then_with(|| a.agent.cmp(&b.agent))
    });

    let mut eras: Vec<Era> = Vec::new();
    for (month, counts) in &by_month {
        let Some(model) = leader(counts) else {
            continue;
        };
        match eras.last_mut() {
            // Only months next to each other: a month without prompts
            // belongs to no era.
            Some(era) if era.model == model && era.to + 1 == *month => era.to = *month,
            _ => eras.push(Era {
                from: *month,
                to: *month,
                model,
            }),
        }
    }

    let peak_hour = (timed_prompts > 0)
        .then(|| (0..24).max_by_key(|h| (hours[*h as usize], std::cmp::Reverse(*h))))
        .flatten();
    let persona = persona(&agents, prompts, &eras, streak, night_prompts);
    let days = by_day
        .into_iter()
        .map(|(day, counts)| {
            let day_stat = Day {
                prompts: counts.values().sum(),
                agent: leader(&counts).unwrap_or_default(),
            };
            (day, day_stat)
        })
        .collect::<BTreeMap<_, _>>();

    Some(Stats {
        year,
        first_day,
        last_day,
        prompts,
        sessions: session_count,
        active_days: days.len(),
        streak,
        longest_session,
        peak_hour,
        agents,
        models: models.len(),
        eras,
        persona,
        undated_prompts,
        days,
    })
}

/// A title that follows from the user's own numbers alone: the first rule
/// that applies.
fn persona(
    agents: &[AgentShare],
    prompts: usize,
    eras: &[Era],
    streak: usize,
    night_prompts: usize,
) -> Persona {
    let percent = |part: usize, whole: usize| (part * 100).checked_div(whole).unwrap_or(0);
    let top = &agents[0];
    // Of all prompts: one without a recorded time is not a night prompt.
    let night = percent(night_prompts, prompts);
    // Eras also end at a month without prompts; only a different model
    // after an era is a change.
    let switches = eras.windows(2).filter(|w| w[0].model != w[1].model).count();
    let (title, line) = if agents.len() >= 5 {
        (
            "The Polyglot",
            format!("{} agents, one human.", agents.len()),
        )
    } else if switches >= 5 {
        (
            "The Model Hopper",
            format!("{switches} lead-model changes in one year."),
        )
    } else if night >= 30 {
        (
            "The Night Owl",
            format!("{night}% of your prompts came after midnight."),
        )
    } else if streak >= 30 {
        (
            "The Marathoner",
            format!("{streak} days without a day off."),
        )
    } else if percent(top.prompts, prompts) >= 90 {
        (
            "The Loyalist",
            format!(
                "{}% {}. No regrets.",
                percent(top.prompts, prompts),
                fit(&top.agent, AGENT_NAME_WIDTH)
            ),
        )
    } else {
        ("The Explorer", "Still mapping the territory.".to_string())
    };
    Persona { title, line }
}

/// `12345` → `12,345`.
fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn month_name(month: u32) -> &'static str {
    const NAMES: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    NAMES[(month as usize - 1) % 12]
}

/// Characters of the screen. `ascii` avoids the block elements and the
/// punctuation that East Asian terminals may draw two columns wide.
struct Glyphs {
    /// A day without prompts, then four levels of activity.
    levels: [char; 5],
    bar: char,
    dash: &'static str,
    dot: &'static str,
    /// Between the title and its line.
    long_dash: &'static str,
    /// Marks a name cut by `fit`.
    ellipsis: &'static str,
}

const UNICODE: Glyphs = Glyphs {
    levels: ['·', '░', '▒', '▓', '█'],
    bar: '█',
    dash: "–",
    dot: "·",
    long_dash: "—",
    ellipsis: "…",
};
const ASCII: Glyphs = Glyphs {
    levels: ['.', '-', '=', '+', '#'],
    bar: '#',
    dash: "-",
    dot: "/",
    long_dash: "-",
    ellipsis: "~",
};

/// 256-color code of each built-in agent. An agent keeps its color on every
/// user's screen, so shared screens can be compared at a glance.
const AGENT_COLORS: [(&str, u8); 8] = [
    ("claude", 209),
    ("codex", 36),
    ("copilot", 99),
    ("gemini", 33),
    ("cursor", 44),
    ("agy", 213),
    ("grok", 255),
    ("opencode", 220),
];
/// Colors for other agents (custom ones in `~/.ahrc`), handed out in the
/// order of their share and reused when they run out.
const SPARE_COLORS: [u8; 4] = [141, 180, 203, 110];
/// Color of the row that sums up the agents not listed by name.
const REST_COLOR: u8 = 245;

/// The color of every agent in `stats`.
fn agent_colors(stats: &Stats) -> HashMap<&str, u8> {
    let mut spare = SPARE_COLORS.iter().cycle();
    stats
        .agents
        .iter()
        .map(|a| {
            let fixed = AGENT_COLORS.iter().find(|(name, _)| *name == a.agent);
            let color = fixed.map_or_else(|| *spare.next().unwrap(), |(_, color)| *color);
            (a.agent.as_str(), color)
        })
        .collect()
}

struct Painter {
    color: bool,
}

impl Painter {
    fn paint(&self, code: &str, text: &str) -> String {
        if self.color && !text.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
    fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }
    fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }
    fn color(&self, code: u8, text: &str) -> String {
        self.paint(&format!("38;5;{code}"), text)
    }
}

/// Spaces between `left` and `right` on one line of `WIDTH` columns.
fn spread(left: &str, right: &str) -> usize {
    WIDTH.saturating_sub(width(left) + width(right))
}

/// The screen as lines, without a trailing newline.
pub fn render(stats: &Stats, ascii: bool, color: bool) -> Vec<String> {
    let glyphs = if ascii { &ASCII } else { &UNICODE };
    let p = Painter { color };
    let mut out: Vec<String> = Vec::new();

    let title = format!(" ah wrapped {}", stats.year);
    let range = format!(
        "{} {} {} {} {}",
        month_name(stats.first_day.month()),
        stats.first_day.day(),
        glyphs.dash,
        month_name(stats.last_day.month()),
        stats.last_day.day()
    );
    out.push(format!(
        "{}{}{}",
        p.bold(&title),
        " ".repeat(spread(&title, &range)),
        p.dim(&range)
    ));
    out.push(String::new());

    let plural = |n: usize, one: &str, many: &str| {
        let word = if n == 1 { one } else { many };
        format!("{} {}", p.bold(&grouped(n)), word)
    };
    out.push(format!(
        "   {}   {}   {}",
        plural(stats.prompts, "prompt", "prompts"),
        plural(stats.sessions, "session", "sessions"),
        plural(stats.active_days, "active day", "active days"),
    ));
    out.push(format!(
        "   {} {dot} {} {dot} {}-day streak",
        plural(stats.agents.len(), "agent", "agents"),
        plural(stats.models, "model", "models"),
        p.bold(&grouped(stats.streak)),
        dot = glyphs.dot,
    ));
    out.push(String::new());

    out.extend(render_calendar(stats, glyphs, &p));
    out.push(String::new());

    let colors = agent_colors(stats);
    let mut rows: Vec<(String, usize, u8)> = stats
        .agents
        .iter()
        .take(TOP_AGENTS)
        .map(|a| {
            let name = fit(&a.agent, AGENT_NAME_WIDTH).replace('…', glyphs.ellipsis);
            (name, a.prompts, colors[a.agent.as_str()])
        })
        .collect();
    let rest = &stats.agents[stats.agents.len().min(TOP_AGENTS)..];
    if !rest.is_empty() {
        rows.push((
            format!("+{} more", rest.len()),
            rest.iter().map(|a| a.prompts).sum(),
            REST_COLOR,
        ));
    }
    let longest = rows.iter().map(|r| r.1).max().unwrap_or(1).max(1);
    let name_width = rows.iter().map(|r| width(&r.0)).max().unwrap_or(0);
    for (name, prompts, color) in &rows {
        let cells = (prompts * BAR_WIDTH).div_ceil(longest).clamp(1, BAR_WIDTH);
        let bar: String = std::iter::repeat_n(glyphs.bar, cells).collect();
        out.push(format!(
            "   {}  {}{}  {:>3}%",
            padded(name, name_width),
            p.color(*color, &bar),
            " ".repeat(BAR_WIDTH - cells),
            (prompts * 100 + stats.prompts / 2) / stats.prompts.max(1),
        ));
    }

    if !stats.eras.is_empty() {
        out.push(String::new());
        out.push(format!("   {}", p.dim("Your eras (lead model)")));
        for era in &stats.eras {
            let months = if era.from == era.to {
                month_name(era.from).to_string()
            } else {
                format!(
                    "{}{}{}",
                    month_name(era.from),
                    glyphs.dash,
                    month_name(era.to)
                )
            };
            let model = fit(&era.model, MODEL_NAME_WIDTH).replace('…', glyphs.ellipsis);
            out.push(format!("   {months:<9} {model}"));
        }
    }
    out.push(String::new());

    let mut facts = Vec::new();
    if let Some(hour) = stats.peak_hour {
        facts.push(format!("Peak hour {}", p.bold(&format!("{hour}:00"))));
    }
    facts.push(format!(
        "Longest session {} prompts",
        p.bold(&grouped(stats.longest_session))
    ));
    out.push(format!("   {}", facts.join(&format!(" {} ", glyphs.dot))));
    out.push(String::new());
    // On one line when it fits, else the line goes under the title.
    let line = stats.persona.line.replace('…', glyphs.ellipsis);
    let you_are = format!("   You are: {}", stats.persona.title);
    let one_line = format!("{you_are} {} {line}", glyphs.long_dash);
    let title = format!("   You are: {}", p.bold(stats.persona.title));
    if width(&one_line) <= WIDTH {
        out.push(format!("{title} {} {line}", glyphs.long_dash));
    } else {
        out.push(title);
        out.push(format!("   {line}"));
    }
    out.push(String::new());

    let note = if stats.undated_prompts > 0 {
        format!(
            " ~{} prompts dated by file time",
            grouped(stats.undated_prompts)
        )
    } else {
        String::new()
    };
    let url = "github.com/nihen/ah";
    out.push(format!(
        "{}{}{}",
        p.dim(&note),
        " ".repeat(spread(&note, url)),
        p.dim(url)
    ));
    out
}

/// The year as a grid: one column per week (Monday first), one row per
/// weekday, shaded by prompts and colored by the day's lead agent.
fn render_calendar(stats: &Stats, glyphs: &Glyphs, p: &Painter) -> Vec<String> {
    let Some(jan1) = NaiveDate::from_ymd_opt(stats.year, 1, 1) else {
        return Vec::new();
    };
    let start = jan1 - Duration::days(jan1.weekday().num_days_from_monday() as i64);
    let week_of = |day: NaiveDate| ((day - start).num_days() / 7) as usize;
    let weeks = week_of(stats.last_day) + 1;

    // Shade by quartile among the active days.
    let mut counts: Vec<usize> = stats.days.values().map(|d| d.prompts).collect();
    counts.sort_unstable();
    let quartile = |q: usize| counts[(counts.len() * q / 4).min(counts.len() - 1)];
    let bounds = [quartile(1), quartile(2), quartile(3)];
    let level = |prompts: usize| 1 + bounds.iter().filter(|b| prompts > **b).count();
    let colors = agent_colors(stats);

    let mut labels = vec![' '; weeks];
    for month in 1..=12 {
        let Some(first) = NaiveDate::from_ymd_opt(stats.year, month, 1) else {
            continue;
        };
        let col = week_of(first);
        if first > stats.last_day || col + 3 > weeks {
            break;
        }
        for (i, c) in month_name(month).chars().enumerate() {
            labels[col + i] = c;
        }
    }
    let mut out = vec![format!(
        "     {}",
        p.dim(labels.iter().collect::<String>().trim_end())
    )];
    for (weekday, label) in ["Mon", "", "Wed", "", "Fri", "", "Sun"].iter().enumerate() {
        let mut row = String::new();
        for week in 0..weeks {
            let day = start + Duration::days((week * 7 + weekday) as i64);
            if day.year() != stats.year || day > stats.last_day {
                row.push(' ');
                continue;
            }
            match stats.days.get(&day) {
                Some(d) => {
                    let glyph = glyphs.levels[level(d.prompts)].to_string();
                    let color = colors.get(d.agent.as_str()).copied().unwrap_or(REST_COLOR);
                    row.push_str(&p.color(color, &glyph));
                }
                None => row.push_str(&p.dim(&glyphs.levels[0].to_string())),
            }
        }
        out.push(format!(
            " {} {}",
            p.dim(&format!("{label:<3}")),
            row.trim_end()
        ));
    }
    out
}

fn parse_modified(value: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M").ok()
}

/// Every session, read through the same pipeline as `ah log` (filters, one
/// row per session).
fn collect(filter: &cli::FilterArgs) -> Result<Vec<SessionInput>, String> {
    // Like `ah project`, every directory unless one is named.
    let mut filters = FieldFilter::from_options(&filter.agent, &filter.project);
    if let Some(dir) = filter.dir.as_ref().filter(|_| !filter.all) {
        filters.push(FieldFilter {
            field: Field::Cwd,
            value: cli::FilterArgs::resolve_dir(dir),
        });
    }
    let result = pipeline::run_pipeline(&pipeline::PipelineParams {
        resolve_fields: vec![
            Field::Agent,
            Field::Model,
            Field::Models,
            Field::PromptTimes,
            Field::ModifiedAt,
        ],
        resolve_opts: ResolveOpts::default(),
        filters,
        // Every session is read: prompts are dated by their own records,
        // which a file's modification time can predate (a copied or
        // restored file, a clock that was off).
        since: None,
        until: None,
        query: String::new(),
        search_mode: SearchMode::Text,
        sort_field: Field::ModifiedAt,
        sort_order: SortOrder::Desc,
        collect_limit: filter.limit,
        running: false,
        require_resume_cmd: false,
    })?;
    Ok(result
        .sessions
        .into_iter()
        .map(|mut session| {
            let mut take = |field: Field| session.fields.remove(&field).unwrap_or_default();
            SessionInput {
                agent: take(Field::Agent),
                model: take(Field::Model),
                models: take(Field::Models)
                    .split(',')
                    .filter(|m| !m.is_empty())
                    .map(str::to_string)
                    .collect(),
                prompt_times: serde_json::from_str(&take(Field::PromptTimes)).unwrap_or_default(),
                modified: parse_modified(&take(Field::ModifiedAt)),
            }
        })
        .collect())
}

/// `ah stats`: print the year in review.
pub fn run(args: &cli::StatsArgs, filter: &cli::FilterArgs) -> Result<(), String> {
    if !filter.remote.is_empty() {
        return Err("ah stats does not read remote hosts yet; run it on each host.".into());
    }
    if filter.query.is_some() || filter.since.is_some() || filter.until.is_some() {
        return Err(
            "ah stats covers one calendar year: use --year instead of -q/--since/--until.".into(),
        );
    }
    // Options that narrow other commands must not pass for a narrowed count.
    if filter.running || filter.prompt_only || filter.raw_search {
        return Err("ah stats does not take --running, -p or --raw-search.".into());
    }
    let now = Local::now();
    let year = args.year.unwrap_or_else(|| now.year());
    let sessions = collect(filter)?;
    let Some(stats) = build(year, now.timestamp(), &sessions) else {
        return Err(format!("No prompts found in {year}."));
    };
    if args.json {
        let json = serde_json::to_string(&stats).map_err(|e| e.to_string())?;
        println!("{json}");
    } else {
        for line in render(&stats, args.ascii, color::use_color()) {
            println!("{line}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unix(date: &str, hour: u32) -> Option<i64> {
        let day = NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap();
        let at = day.and_hms_opt(hour, 0, 0).unwrap();
        Some(
            Local
                .from_local_datetime(&at)
                .earliest()
                .unwrap()
                .timestamp(),
        )
    }

    /// A `now` after every date the tests use.
    fn later() -> i64 {
        unix("2099-01-01", 0).unwrap()
    }

    fn session(agent: &str, model: &str, times: Vec<Option<i64>>) -> SessionInput {
        SessionInput {
            agent: agent.to_string(),
            model: model.to_string(),
            models: vec![model.to_string()],
            prompt_times: times,
            modified: None,
        }
    }

    #[test]
    fn prompts_count_on_their_own_day_and_year() {
        let sessions = [
            // Spans New Year: only the 2026 prompts count.
            session(
                "codex",
                "m1",
                vec![
                    unix("2025-12-31", 23),
                    unix("2026-01-01", 1),
                    unix("2026-01-02", 10),
                ],
            ),
            session("claude", "m2", vec![unix("2026-01-03", 10); 3]),
            // Entirely outside the year: not a session of 2026.
            session("grok", "m3", vec![unix("2025-06-01", 10)]),
        ];
        let stats = build(2026, later(), &sessions).unwrap();
        assert_eq!(stats.prompts, 5);
        assert_eq!(stats.sessions, 2);
        assert_eq!(stats.active_days, 3);
        assert_eq!(stats.streak, 3);
        assert_eq!(stats.longest_session, 3);
        assert_eq!(stats.models, 2);
        assert_eq!(stats.peak_hour, Some(10));
        let agents: Vec<_> = stats
            .agents
            .iter()
            .map(|a| (a.agent.as_str(), a.prompts, a.sessions))
            .collect();
        assert_eq!(agents, [("claude", 3, 1), ("codex", 2, 1)]);
        assert_eq!(
            stats.days[&NaiveDate::from_ymd_opt(2026, 1, 3).unwrap()].agent,
            "claude"
        );
        assert!(build(2024, later(), &sessions).is_none());
    }

    #[test]
    fn prompts_without_a_time_use_the_session_time() {
        let mut undated = session("cursor", "", vec![None, None]);
        undated.modified = parse_modified("2026-03-05 12:30");
        let stats = build(2026, later(), &[undated]).unwrap();
        assert_eq!(stats.prompts, 2);
        assert_eq!(stats.undated_prompts, 2);
        assert_eq!(
            stats.first_day,
            NaiveDate::from_ymd_opt(2026, 3, 5).unwrap()
        );
        // No recorded time, no peak hour.
        assert_eq!(stats.peak_hour, None);
        assert!(stats.eras.is_empty());
    }

    #[test]
    fn eras_merge_consecutive_months_with_one_lead_model() {
        let sessions = [
            session("codex", "a", vec![unix("2026-01-10", 9); 2]),
            session("codex", "b", vec![unix("2026-01-11", 9)]),
            session("codex", "a", vec![unix("2026-02-10", 9)]),
            session("claude", "b", vec![unix("2026-04-10", 9)]),
            session("claude", "a", vec![unix("2026-05-10", 9)]),
        ];
        let eras = build(2026, later(), &sessions).unwrap().eras;
        let era = |from, to, model: &str| Era {
            from,
            to,
            model: model.to_string(),
        };
        assert_eq!(eras, [era(1, 2, "a"), era(4, 4, "b"), era(5, 5, "a")]);

        // A month without prompts ends an era, even between two months
        // led by the same model.
        let gap = [
            session("codex", "a", vec![unix("2026-01-10", 9)]),
            session("codex", "a", vec![unix("2026-03-10", 9)]),
        ];
        assert_eq!(
            build(2026, later(), &gap).unwrap().eras,
            [era(1, 1, "a"), era(3, 3, "a")]
        );
    }

    #[test]
    fn persona_takes_the_first_rule_that_applies() {
        let loyal = [session("claude", "m", vec![unix("2026-02-01", 14); 10])];
        assert_eq!(
            build(2026, later(), &loyal).unwrap().persona.title,
            "The Loyalist"
        );

        let owl = [
            session("claude", "m", vec![unix("2026-02-01", 2); 4]),
            session("codex", "m", vec![unix("2026-02-01", 14); 6]),
        ];
        assert_eq!(
            build(2026, later(), &owl).unwrap().persona.title,
            "The Night Owl"
        );

        let split = [
            session("claude", "m", vec![unix("2026-02-01", 14); 5]),
            session("codex", "m", vec![unix("2026-02-01", 14); 5]),
        ];
        assert_eq!(
            build(2026, later(), &split).unwrap().persona.title,
            "The Explorer"
        );

        let many: Vec<_> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|agent| session(agent, "m", vec![unix("2026-02-01", 14)]))
            .collect();
        let persona = build(2026, later(), &many).unwrap().persona;
        assert_eq!(persona.title, "The Polyglot");
        assert_eq!(persona.line, "5 agents, one human.");
    }

    #[test]
    fn screen_fits_the_width_in_both_glyph_sets() {
        let mut sessions = Vec::new();
        for (i, agent) in [
            "claude", "codex", "copilot", "cursor", "opencode", "grok", "agy",
        ]
        .iter()
        .enumerate()
        {
            for month in 1..=12 {
                let day = format!("2026-{month:02}-{:02}", 1 + i * 3);
                sessions.push(session(
                    agent,
                    &format!("some-long-model-name-{}", month / 2),
                    vec![unix(&day, 11); 1 + i * 400],
                ));
            }
        }
        let stats = build(2026, later(), &sessions).unwrap();
        for ascii in [false, true] {
            let lines = render(&stats, ascii, false);
            for line in &lines {
                assert!(line.chars().count() <= WIDTH, "{line:?}");
                assert_eq!(line.trim_end(), line, "trailing space: {line:?}");
                assert!(!ascii || line.is_ascii(), "{line:?}");
            }
            assert!(lines[0].starts_with(" ah wrapped 2026"));
            assert!(lines.iter().any(|l| l.contains("+2 more")));
        }
    }

    #[test]
    fn agents_keep_their_color_whatever_their_rank() {
        let stats_of = |agents: &[(&str, usize)]| {
            let sessions: Vec<_> = agents
                .iter()
                .map(|(agent, n)| session(agent, "m", vec![unix("2026-02-01", 14); *n]))
                .collect();
            build(2026, later(), &sessions).unwrap()
        };
        let first = stats_of(&[("claude", 9), ("codex", 1), ("mine", 1), ("other", 1)]);
        let second = stats_of(&[("codex", 9), ("claude", 1), ("other", 2)]);
        let (a, b) = (agent_colors(&first), agent_colors(&second));
        assert_eq!(a["claude"], b["claude"]);
        assert_eq!(a["codex"], b["codex"]);
        assert_ne!(a["claude"], a["codex"]);
        // Custom agents take the spare colors in the order of their share.
        assert_eq!((a["mine"], a["other"]), (SPARE_COLORS[0], SPARE_COLORS[1]));
        assert_eq!(b["other"], SPARE_COLORS[0]);

        // The bar of an agent is drawn in its color.
        let screen = render(&first, false, true).join("\n");
        let claude = AGENT_COLORS.iter().find(|(n, _)| *n == "claude").unwrap().1;
        assert!(
            screen.contains(&format!("claude  \x1b[38;5;{claude}m")),
            "{screen}"
        );
    }

    #[test]
    fn prompts_dated_after_now_are_left_out() {
        let sessions = [session(
            "claude",
            "m",
            vec![unix("2026-03-01", 9), unix("2026-12-31", 9)],
        )];
        let now = unix("2026-06-01", 0).unwrap();
        let stats = build(2026, now, &sessions).unwrap();
        assert_eq!(stats.prompts, 1);
        assert_eq!(stats.last_day, NaiveDate::from_ymd_opt(2026, 3, 1).unwrap());
    }

    #[test]
    fn night_share_is_of_all_prompts() {
        // 2 of 3 timed prompts are at night, but only 2 of 10 prompts.
        let mut undated = session("cursor", "", vec![None; 7]);
        undated.modified = parse_modified("2026-03-05 12:30");
        let sessions = [
            session(
                "claude",
                "m",
                vec![
                    unix("2026-03-05", 2),
                    unix("2026-03-05", 3),
                    unix("2026-03-05", 14),
                ],
            ),
            undated,
        ];
        assert_eq!(
            build(2026, later(), &sessions).unwrap().persona.title,
            "The Explorer"
        );
    }

    #[test]
    fn every_line_fits_with_long_names_long_titles_and_54_weeks() {
        // 2040 starts on a Sunday and is a leap year: 54 calendar columns.
        let long_model = "m".repeat(80);
        let sessions = [
            session(
                "a-custom-agent-with-a-long-name",
                &long_model,
                vec![unix("2040-01-01", 2); 4],
            ),
            session("claude", &long_model, vec![unix("2040-12-31", 14); 6]),
        ];
        let stats = build(2040, later(), &sessions).unwrap();
        assert_eq!(stats.persona.title, "The Night Owl");
        for ascii in [false, true] {
            let lines = render(&stats, ascii, false);
            for line in &lines {
                assert!(line.chars().count() <= WIDTH, "{line:?}");
                assert!(!ascii || line.is_ascii(), "{line:?}");
            }
            // The title's line moves below it when one line is too long.
            let title = lines.iter().position(|l| l == "   You are: The Night Owl");
            let line = &lines[title.expect("title on its own line") + 1];
            assert_eq!(line, "   40% of your prompts came after midnight.");
            let ellipsis = if ascii { "~" } else { "…" };
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains(&format!("a-custom-ag{ellipsis}  ")))
            );
        }

        // A long agent name is cut in the title's line too.
        let loyal = [session(
            "a-custom-agent-with-a-long-name",
            "m",
            vec![unix("2040-06-01", 14)],
        )];
        let persona = build(2040, later(), &loyal).unwrap().persona;
        assert_eq!(persona.line, "100% a-custom-ag…. No regrets.");
    }

    #[test]
    fn a_prompt_before_now_counts_even_if_its_local_time_reads_later() {
        // When clocks are set back, an earlier instant can read as a later
        // local time. Whatever the zone, 20 minutes ago is not the future.
        let sent = unix("2026-11-01", 1).unwrap() + 50 * 60;
        let stats = build(
            2026,
            sent + 20 * 60,
            &[session("claude", "m", vec![Some(sent)])],
        );
        assert_eq!(stats.unwrap().prompts, 1);
        // One second later than now is.
        assert!(build(2026, sent - 1, &[session("claude", "m", vec![Some(sent)])]).is_none());

        // A stand-in time after now is left out too.
        let mut undated = session("cursor", "", vec![None]);
        undated.modified = parse_modified("2026-12-31 12:00");
        assert!(build(2026, unix("2026-06-01", 0).unwrap(), &[undated]).is_none());
    }

    #[test]
    fn model_hopper_counts_changes_of_model_not_eras() {
        let month = |m: u32, model: &str| {
            session("claude", model, vec![unix(&format!("2026-{m:02}-10"), 14)])
        };
        // One model every other month: six eras, no change.
        let same: Vec<_> = [1, 3, 5, 7, 9, 11].iter().map(|m| month(*m, "a")).collect();
        let stats = build(2026, later(), &same).unwrap();
        assert_eq!(stats.eras.len(), 6);
        assert_eq!(stats.persona.title, "The Loyalist");

        // Six models in six months: five changes.
        let hop: Vec<_> = ["a", "b", "c", "d", "e", "f"]
            .iter()
            .enumerate()
            .map(|(i, model)| month(i as u32 + 1, model))
            .collect();
        let persona = build(2026, later(), &hop).unwrap().persona;
        assert_eq!(persona.title, "The Model Hopper");
        assert_eq!(persona.line, "5 lead-model changes in one year.");
    }

    #[test]
    fn wide_characters_are_measured_in_columns() {
        assert_eq!(fit("abc", 3), "abc");
        assert_eq!(fit("abcd", 3), "ab…");
        assert_eq!(fit("あいう", 6), "あいう");
        assert_eq!(fit("あいうえ", 6), "あい…");
        assert_eq!(padded("あ", 4), "あ  ");

        let wide_model = "あ".repeat(47);
        let sessions = [
            session(
                "エージェント名が長い",
                &wide_model,
                vec![unix("2026-02-01", 14); 3],
            ),
            session("claude", &wide_model, vec![unix("2026-02-02", 14)]),
        ];
        let stats = build(2026, later(), &sessions).unwrap();
        for line in render(&stats, false, false) {
            assert!(width(&line) <= WIDTH, "{line:?}");
        }
    }

    #[test]
    fn grouped_inserts_thousands_separators() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(1234567), "1,234,567");
    }
}

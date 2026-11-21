//! `ah stats`: one screen that sums up a year of agent sessions.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::SystemTime;

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike};
use serde::Serialize;

use crate::cli::{self, Field, FieldFilter, SearchMode, SortOrder};
use crate::color;
use crate::pipeline;
use crate::resolver::ResolveOpts;

/// Width of the screen, in terminal columns.
const WIDTH: usize = 58;
/// Agents listed by name; the rest are summed in one row.
const TOP_AGENTS: usize = 5;
/// Columns of the longest agent bar.
const BAR_WIDTH: usize = 30;

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

/// Sum up the prompts of `year`. `None` when there are none.
pub fn build(year: i32, sessions: &[SessionInput]) -> Option<Stats> {
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
            let dated = time.and_then(local_time);
            let Some(at) = dated.or(session.modified).filter(|at| at.year() == year) else {
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
            Some(era) if era.model == model => era.to = *month,
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
    let persona = persona(
        &agents,
        prompts,
        &eras,
        streak,
        night_prompts,
        timed_prompts,
    );
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
    timed_prompts: usize,
) -> Persona {
    let percent = |part: usize, whole: usize| (part * 100).checked_div(whole).unwrap_or(0);
    let top = &agents[0];
    let night = percent(night_prompts, timed_prompts);
    let (title, line) = if agents.len() >= 5 {
        (
            "The Polyglot",
            format!("{} agents, one human.", agents.len()),
        )
    } else if eras.len() >= 6 {
        (
            "The Model Hopper",
            format!("{} lead models in one year.", eras.len()),
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
                top.agent
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
}

const UNICODE: Glyphs = Glyphs {
    levels: ['·', '░', '▒', '▓', '█'],
    bar: '█',
    dash: "–",
    dot: "·",
};
const ASCII: Glyphs = Glyphs {
    levels: ['.', '-', '=', '+', '#'],
    bar: '#',
    dash: "-",
    dot: "/",
};

/// 256-color codes for the agents, by rank; the rest share the last one.
const AGENT_COLORS: [u8; TOP_AGENTS + 1] = [39, 208, 77, 170, 220, 245];

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
    fn agent(&self, rank: usize, text: &str) -> String {
        let code = AGENT_COLORS[rank.min(TOP_AGENTS)];
        self.paint(&format!("38;5;{code}"), text)
    }
}

/// `left` and `right` on one line of `WIDTH` columns. Both are plain ASCII
/// apart from the glyphs, which are one column each.
fn spread(left: &str, right: &str) -> usize {
    WIDTH.saturating_sub(left.chars().count() + right.chars().count())
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

    let rank_of: HashMap<&str, usize> = stats
        .agents
        .iter()
        .enumerate()
        .map(|(rank, a)| (a.agent.as_str(), rank))
        .collect();
    let mut rows: Vec<(String, usize, usize)> = stats
        .agents
        .iter()
        .take(TOP_AGENTS)
        .map(|a| (a.agent.clone(), a.prompts, rank_of[a.agent.as_str()]))
        .collect();
    let rest = &stats.agents[stats.agents.len().min(TOP_AGENTS)..];
    if !rest.is_empty() {
        rows.push((
            format!("+{} more", rest.len()),
            rest.iter().map(|a| a.prompts).sum(),
            TOP_AGENTS,
        ));
    }
    let longest = rows.iter().map(|r| r.1).max().unwrap_or(1).max(1);
    let name_width = rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(0);
    for (name, prompts, rank) in &rows {
        let cells = (prompts * BAR_WIDTH).div_ceil(longest).clamp(1, BAR_WIDTH);
        let bar: String = std::iter::repeat_n(glyphs.bar, cells).collect();
        out.push(format!(
            "   {name:<name_width$}  {}{}  {:>3}%",
            p.agent(*rank, &bar),
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
            out.push(format!("   {months:<9} {}", era.model));
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
    out.push(format!(
        "   You are: {} {} {}",
        p.bold(stats.persona.title),
        if ascii { "-" } else { "—" },
        stats.persona.line
    ));
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
    let rank_of: HashMap<&str, usize> = stats
        .agents
        .iter()
        .enumerate()
        .map(|(rank, a)| (a.agent.as_str(), rank))
        .collect();

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
                    let rank = rank_of.get(d.agent.as_str()).copied().unwrap_or(TOP_AGENTS);
                    row.push_str(&p.agent(rank, &glyph));
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

/// Sessions with a prompt that may fall in `year`, read through the same
/// pipeline as `ah log` (filters, one row per session).
fn collect(filter: &cli::FilterArgs, year: i32) -> Result<Vec<SessionInput>, String> {
    // Like `ah project`, every directory unless one is named.
    let mut filters = FieldFilter::from_options(&filter.agent, &filter.project);
    if let Some(dir) = filter.dir.as_ref().filter(|_| !filter.all) {
        filters.push(FieldFilter {
            field: Field::Cwd,
            value: cli::FilterArgs::resolve_dir(dir),
        });
    }
    // A session last written before the year began has no prompt in it.
    let year_start: Option<SystemTime> = Local
        .with_ymd_and_hms(year, 1, 1, 0, 0, 0)
        .earliest()
        .map(|t: DateTime<Local>| t.into());
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
        since: year_start,
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
    let year = args.year.unwrap_or_else(|| Local::now().year());
    let sessions = collect(filter, year)?;
    let Some(stats) = build(year, &sessions) else {
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
        let stats = build(2026, &sessions).unwrap();
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
        assert!(build(2024, &sessions).is_none());
    }

    #[test]
    fn prompts_without_a_time_use_the_session_time() {
        let mut undated = session("cursor", "", vec![None, None]);
        undated.modified = parse_modified("2026-03-05 12:30");
        let stats = build(2026, &[undated]).unwrap();
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
        let eras = build(2026, &sessions).unwrap().eras;
        let era = |from, to, model: &str| Era {
            from,
            to,
            model: model.to_string(),
        };
        assert_eq!(eras, [era(1, 2, "a"), era(4, 4, "b"), era(5, 5, "a")]);
    }

    #[test]
    fn persona_takes_the_first_rule_that_applies() {
        let loyal = [session("claude", "m", vec![unix("2026-02-01", 14); 10])];
        assert_eq!(build(2026, &loyal).unwrap().persona.title, "The Loyalist");

        let owl = [
            session("claude", "m", vec![unix("2026-02-01", 2); 4]),
            session("codex", "m", vec![unix("2026-02-01", 14); 6]),
        ];
        assert_eq!(build(2026, &owl).unwrap().persona.title, "The Night Owl");

        let split = [
            session("claude", "m", vec![unix("2026-02-01", 14); 5]),
            session("codex", "m", vec![unix("2026-02-01", 14); 5]),
        ];
        assert_eq!(build(2026, &split).unwrap().persona.title, "The Explorer");

        let many: Vec<_> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|agent| session(agent, "m", vec![unix("2026-02-01", 14)]))
            .collect();
        let persona = build(2026, &many).unwrap().persona;
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
        let stats = build(2026, &sessions).unwrap();
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
    fn grouped_inserts_thousands_separators() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(1234567), "1,234,567");
    }
}

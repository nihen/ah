use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::SystemTime;

use chrono::{Local, NaiveDate, NaiveDateTime, TimeDelta};
use clap::{Parser, Subcommand};
use const_format::concatcp;

/// Parse a comma-separated list of field names into typed field values.
fn parse_field_list<F: FromStr<Err = String>>(s: &str) -> Result<Vec<F>, String> {
    let mut fields = Vec::new();
    for name in s.split(',') {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        fields.push(F::from_str(name)?);
    }
    Ok(fields)
}

/// Parse a time spec: "2026-03-20", "2026-03-20 15:00", "3d", "1w", "12h"
fn parse_time_spec(s: &str) -> Result<SystemTime, String> {
    // Try relative: Nd, Nw, Nh, Nm
    if let Some(delta) = parse_relative(s) {
        let dt = Local::now() - delta;
        return Ok(SystemTime::from(dt));
    }

    // Try date-time: "2026-03-20 15:00"
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
        let local = ndt.and_local_timezone(Local).single();
        if let Some(dt) = local {
            return Ok(SystemTime::from(dt));
        }
    }

    // Try date: "2026-03-20"
    if let Ok(nd) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let ndt = nd.and_hms_opt(0, 0, 0).unwrap();
        let local = ndt.and_local_timezone(Local).single();
        if let Some(dt) = local {
            return Ok(SystemTime::from(dt));
        }
    }

    Err(format!(
        "Invalid time spec: '{}' (expected: YYYY-MM-DD, YYYY-MM-DD HH:MM, Nd, Nw, Nh, Nm)",
        s
    ))
}

fn parse_relative(s: &str) -> Option<TimeDelta> {
    let s = s.trim();
    if s.len() < 2 {
        return None;
    }
    let (num_str, unit) = s.split_at(s.len() - 1);
    let n: i64 = num_str.parse().ok()?;
    match unit {
        "h" => TimeDelta::try_hours(n),
        "d" => TimeDelta::try_days(n),
        "w" => TimeDelta::try_weeks(n),
        "m" => n.checked_mul(30).and_then(TimeDelta::try_days),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Field {
    Agent,
    Project,
    ProjectRaw,
    ModifiedAt,
    CreatedAt,
    Title,
    FirstPrompt,
    LastPrompt,
    Prompts,
    Responses,
    Messages,
    Transcript,
    Matched,
    Path,
    Cwd,
    Id,
    ResumeCmd,
    Turns,
    Size,
    Running,
    Pid,
}

impl Field {
    pub fn name(&self) -> &'static str {
        match self {
            Field::Agent => "agent",
            Field::Project => "project",
            Field::ProjectRaw => "project_raw",
            Field::ModifiedAt => "modified_at",
            Field::CreatedAt => "created_at",
            Field::Title => "title",
            Field::FirstPrompt => "first_prompt",
            Field::LastPrompt => "last_prompt",
            Field::Prompts => "prompts",
            Field::Responses => "responses",
            Field::Messages => "messages",
            Field::Transcript => "transcript",
            Field::Matched => "matched",
            Field::Path => "path",
            Field::Cwd => "cwd",
            Field::Id => "id",
            Field::ResumeCmd => "resume_cmd",
            Field::Turns => "turns",
            Field::Size => "size",
            Field::Running => "running",
            Field::Pid => "pid",
        }
    }

    pub fn all() -> Vec<Field> {
        vec![
            Field::Agent,
            Field::Project,
            Field::ProjectRaw,
            Field::ModifiedAt,
            Field::CreatedAt,
            Field::Title,
            Field::FirstPrompt,
            Field::LastPrompt,
            Field::Prompts,
            Field::Responses,
            Field::Messages,
            Field::Transcript,
            Field::Matched,
            Field::Path,
            Field::Cwd,
            Field::Id,
            Field::ResumeCmd,
            Field::Turns,
            Field::Size,
            Field::Running,
            Field::Pid,
        ]
    }

    pub fn all_names() -> &'static str {
        "agent, project, project_raw, modified_at, created_at, title, first_prompt, last_prompt, prompts, responses, messages, transcript, matched, path, cwd, id, resume_cmd, turns, size, running, pid"
    }

    pub fn description(&self) -> &'static str {
        match self {
            Field::Agent => "Agent name",
            Field::Project => "Normalized project name (usually basename of cwd)",
            Field::ProjectRaw => "Agent-native project identifier",
            Field::ModifiedAt => "Session modified time",
            Field::CreatedAt => "Session created time",
            Field::Title => "Session title or first prompt",
            Field::FirstPrompt => "First user prompt",
            Field::LastPrompt => "Last user prompt",
            Field::Prompts => "User prompts as JSON array",
            Field::Responses => "Agent responses as JSON array",
            Field::Messages => "All messages as JSON array [{role, text}, ...]",
            Field::Transcript => "Joined transcript text",
            Field::Matched => "Matching excerpts for QUERY",
            Field::Path => "Session file path",
            Field::Cwd => "Session working directory",
            Field::Id => "Agent-specific session id (for resume)",
            Field::ResumeCmd => "Shell command to resume",
            Field::Turns => "User prompt count",
            Field::Size => "Session file size in bytes",
            Field::Running => "Whether the session is currently running",
            Field::Pid => "PID of running agent process",
        }
    }

    pub fn example(&self) -> &'static str {
        match self {
            Field::Agent => "claude",
            Field::Project => "myapp",
            Field::ProjectRaw => "Users-you-src-myapp",
            Field::ModifiedAt => "2026-03-20 10:30",
            Field::CreatedAt => "2026-03-20 09:00",
            Field::Title => "fix auth token handling",
            Field::FirstPrompt => "fix the auth bug in login.rs",
            Field::LastPrompt => "looks good, ship it",
            Field::Prompts => "[\"fix the auth bug\",\"ship it\"]",
            Field::Responses => "[\"I'll fix the token...\",\"Done!\"]",
            Field::Messages => {
                "[{\"role\":\"user\",\"text\":\"fix auth\"},{\"role\":\"assistant\",\"text\":\"Done!\"}]"
            }
            Field::Transcript => "User: fix the auth bug...",
            Field::Matched => "...auth token was expired...",
            Field::Path => "~/.claude/projects/-myapp/abc.jsonl",
            Field::Cwd => "/Users/you/src/myapp",
            Field::Id => "abc12345-...",
            Field::ResumeCmd => "cd '/path' && 'claude' '--resume' 'id'",
            Field::Turns => "12",
            Field::Size => "34567",
            Field::Running => "true",
            Field::Pid => "12345",
        }
    }

    /// Whether this field should be compared numerically when sorting.
    pub fn is_numeric(&self) -> bool {
        matches!(self, Field::Turns | Field::Size | Field::Pid)
    }
}

impl FromStr for Field {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "agent" => Ok(Field::Agent),
            "project" => Ok(Field::Project),
            "project_raw" => Ok(Field::ProjectRaw),
            "modified_at" => Ok(Field::ModifiedAt),
            "created_at" => Ok(Field::CreatedAt),
            "title" => Ok(Field::Title),
            "first_prompt" => Ok(Field::FirstPrompt),
            "last_prompt" => Ok(Field::LastPrompt),
            "prompts" => Ok(Field::Prompts),
            "responses" => Ok(Field::Responses),
            "messages" => Ok(Field::Messages),
            "transcript" => Ok(Field::Transcript),
            "matched" => Ok(Field::Matched),
            "path" => Ok(Field::Path),
            "cwd" => Ok(Field::Cwd),
            "id" => Ok(Field::Id),
            "resume_cmd" => Ok(Field::ResumeCmd),
            "turns" => Ok(Field::Turns),
            "size" => Ok(Field::Size),
            "running" => Ok(Field::Running),
            "pid" => Ok(Field::Pid),
            _ => Err(format!(
                "unknown field '{}'. available: {}",
                s,
                Field::all_names()
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SearchMode {
    All,
    Prompt,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutputFormat {
    Log,
    Table,
    Tsv,
    Ltsv,
    Json,
}

impl OutputFormat {
    pub fn from_flags(json: bool, ltsv: bool, tsv: bool, table: bool) -> Self {
        if json {
            OutputFormat::Json
        } else if ltsv {
            OutputFormat::Ltsv
        } else if tsv {
            OutputFormat::Tsv
        } else if table {
            OutputFormat::Table
        } else if crate::pager::is_active() || std::io::IsTerminal::is_terminal(&std::io::stdout())
        {
            OutputFormat::Log
        } else {
            OutputFormat::Tsv
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SortOrder {
    Asc,
    Desc,
}

impl SortOrder {
    pub fn from_flag(asc: bool) -> Self {
        if asc { SortOrder::Asc } else { SortOrder::Desc }
    }
}

/// Filter by agent/project. Builds where-filter list from --agent/--project.
#[derive(Debug, Clone)]
pub struct FieldFilter {
    pub field: Field,
    pub value: String,
}

impl FieldFilter {
    pub fn matches_all(filters: &[FieldFilter], fields: &BTreeMap<Field, String>) -> bool {
        filters
            .iter()
            .all(|f| fields.get(&f.field).map(|v| v.as_str()).unwrap_or("") == f.value)
    }

    pub fn ensure_fields(filters: &[FieldFilter], resolve_fields: &mut Vec<Field>) {
        for f in filters {
            if !resolve_fields.contains(&f.field) {
                resolve_fields.push(f.field);
            }
        }
    }

    /// Build filter list from --agent/--project options.
    pub fn from_options(agent: &Option<String>, project: &Option<String>) -> Vec<FieldFilter> {
        let mut filters = Vec::new();
        if let Some(a) = agent {
            filters.push(FieldFilter {
                field: Field::Agent,
                value: a.clone(),
            });
        }
        if let Some(p) = project {
            filters.push(FieldFilter {
                field: Field::Project,
                value: p.clone(),
            });
        }
        filters
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "ah",
    version,
    about = "Search, inspect, and resume coding-agent sessions from one CLI",
    arg_required_else_help = true,
    override_help = TOP_HELP_TEXT
)]
pub struct Cli {
    #[command(flatten)]
    pub filter: FilterArgs,

    #[command(flatten)]
    pub ia: InteractiveArgs,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// List sessions
    #[command(alias = "search", alias = "ls", override_help = LOG_HELP_TEXT)]
    Log(SearchArgs),

    /// List known projects
    #[command(name = "project", alias = "projects", override_help = PROJECT_HELP_TEXT)]
    Project(ListProjectsArgs),

    /// Show session transcript
    #[command(alias = "cat", override_help = SHOW_HELP_TEXT)]
    Show(ShowArgs),

    /// Resume an agent session
    #[command(override_help = RESUME_HELP_TEXT)]
    Resume(ResumeArgs),

    /// Show session summary per agent
    #[command(name = "agent", override_help = AGENT_HELP_TEXT)]
    Agent(AgentArgs),

    /// List agent memory files
    #[command(name = "memory", override_help = MEMORY_HELP_TEXT)]
    Memory(MemoryArgs),

    /// List supported agents
    #[command(name = "list-agents", override_help = LIST_AGENTS_HELP_TEXT)]
    ListAgents(ListAgentsArgs),

    /// Generate shell completion script
    #[command(name = "completion", override_help = COMPLETION_HELP_TEXT)]
    Completion(CompletionArgs),

    /// Generate man page
    #[command(name = "man", override_help = MAN_HELP_TEXT)]
    Man(ManArgs),
}

#[derive(Parser, Debug)]
pub struct ManArgs {
    /// Subcommand name (e.g. "log", "show"); omit for main page
    pub subcommand: Option<String>,

    /// Generate all man pages into a directory
    #[arg(long)]
    pub out_dir: Option<std::path::PathBuf>,
}

#[derive(Parser, Debug)]
pub struct CompletionArgs {
    /// Shell type
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

/// Common filter options shared across all subcommands.
#[derive(Parser, Debug)]
pub struct FilterArgs {
    /// Filter by agent name (e.g. claude, codex, gemini)
    #[arg(long = "agent", global = true)]
    pub agent: Option<String>,

    /// Filter by project name (agent-specific, use "ah log -o project" to see values)
    #[arg(long = "project", global = true)]
    pub project: Option<String>,

    /// Filter by working directory (use "." for current directory)
    #[arg(short = 'd', long = "dir", global = true)]
    pub dir: Option<String>,

    /// Show all sessions (disable cwd filtering)
    #[arg(short = 'a', long = "all", global = true, conflicts_with = "dir")]
    pub all: bool,

    /// Show all sessions including all configured remotes (-a + all remotes)
    #[arg(short = 'A', global = true, conflicts_with = "dir")]
    pub all_remote: bool,

    /// Full-text search query (regex)
    #[arg(short = 'q', long = "query", global = true)]
    pub query: Option<String>,

    /// Search only user prompts (use with -q)
    #[arg(short = 'p', long = "prompt-only", global = true)]
    pub prompt_only: bool,

    /// Max session files to scan (default: 0, no limit)
    #[arg(short = 'n', long = "limit", default_value_t = 0, global = true)]
    pub limit: usize,

    /// Show sessions newer than date (e.g. "2026-03-20", "3d", "1w", "2m" = ~60 days)
    #[arg(long = "since", global = true)]
    pub since: Option<String>,

    /// Show sessions older than date (e.g. "2026-03-20", "3d", "1w", "2m" = ~60 days)
    #[arg(long = "until", global = true)]
    pub until: Option<String>,

    /// Show only currently running sessions (Claude only for now)
    #[arg(long = "running", global = true)]
    pub running: bool,

    /// Include sessions from a remote host (repeatable; name must match [remotes.*] in ~/.ahrc)
    #[arg(long = "remote", global = true, conflicts_with = "dir")]
    pub remote: Vec<String>,

    /// Force colored output (even through pipes)
    #[arg(long = "color", conflicts_with = "no_color", global = true)]
    pub color: bool,

    /// Disable colored output
    #[arg(long = "no-color", conflicts_with = "color", global = true)]
    pub no_color: bool,

    /// Disable automatic pager
    #[arg(long = "no-pager", global = true)]
    pub no_pager: bool,

    /// Show debug info (glob expansion, scan counts, timing) on stderr
    #[arg(long = "debug", global = true)]
    pub debug: bool,
}

impl FilterArgs {
    pub fn to_filters(&self) -> Vec<FieldFilter> {
        let mut filters = FieldFilter::from_options(&self.agent, &self.project);
        if self.all || self.all_remote {
            // --all / -A: no cwd filter
        } else if let Some(d) = &self.dir {
            let dir = Self::resolve_dir(d);
            filters.push(FieldFilter {
                field: Field::Cwd,
                value: dir,
            });
        } else {
            // Default: filter by current directory
            let dir = Self::resolve_dir(".");
            filters.push(FieldFilter {
                field: Field::Cwd,
                value: dir,
            });
        }
        filters
    }

    pub fn search_mode(&self) -> SearchMode {
        if self.prompt_only {
            SearchMode::Prompt
        } else {
            SearchMode::All
        }
    }

    /// Parse --since into a SystemTime lower bound
    pub fn since_time(&self) -> Result<Option<std::time::SystemTime>, String> {
        self.since.as_ref().map(|s| parse_time_spec(s)).transpose()
    }

    /// Parse --until into a SystemTime upper bound
    pub fn until_time(&self) -> Result<Option<std::time::SystemTime>, String> {
        self.until.as_ref().map(|s| parse_time_spec(s)).transpose()
    }

    pub fn resolve_dir(d: &str) -> String {
        if d == "." {
            std::env::current_dir()
                .ok()
                .and_then(|p| std::fs::canonicalize(&p).ok())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| d.to_string())
        } else {
            std::fs::canonicalize(d)
                .ok()
                .or_else(|| {
                    // Strip surrounding quotes (e.g. from fzf preview passing shell-quoted paths)
                    let stripped = crate::output::strip_quotes(d);
                    if stripped != d {
                        std::fs::canonicalize(stripped).ok()
                    } else {
                        None
                    }
                })
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| d.to_string())
        }
    }
}

/// Common options shared by log and fuzzy.
#[derive(Parser, Debug)]
pub struct CommonArgs {
    /// Select output fields (comma-separated, replaces defaults)
    #[arg(short = 'o', long = "fields")]
    pub fields: Option<String>,
}

impl CommonArgs {
    pub fn parse_fields(&self) -> Result<Option<Vec<Field>>, String> {
        self.fields
            .as_ref()
            .map(|f| parse_field_list(f))
            .transpose()
    }
}

#[derive(Parser, Debug)]
pub struct SearchArgs {
    #[command(flatten)]
    pub common: CommonArgs,

    /// Add fields to defaults (comma-separated)
    #[arg(short = 'O', long = "extra-fields")]
    extra_fields: Option<String>,

    /// Output as one-line table (default on TTY is multi-line log format)
    #[arg(long = "table", conflicts_with_all = ["tsv", "ltsv", "json"])]
    table: bool,

    /// Output as TSV (tab-separated values; default when piped)
    #[arg(long = "tsv", conflicts_with_all = ["table", "ltsv", "json"])]
    tsv: bool,

    /// Output as LTSV (Labeled Tab-Separated Values)
    #[arg(long = "ltsv", conflicts_with_all = ["table", "json", "tsv"])]
    ltsv: bool,

    /// Output as JSON Lines
    #[arg(long = "json", conflicts_with_all = ["table", "ltsv", "tsv"])]
    pub json: bool,

    /// Max characters for transcript field
    #[arg(long = "transcript-limit", default_value_t = 500)]
    transcript_limit: usize,

    /// Max characters for auto-generated title (0 = no limit)
    #[arg(long = "title-limit", default_value_t = 50)]
    title_limit: usize,

    /// Sort by field (default: modified_at)
    #[arg(short = 'S', long = "sort")]
    sort: Option<String>,

    /// Sort ascending
    #[arg(long = "asc", conflicts_with = "desc")]
    asc: bool,

    /// Sort descending (default)
    #[arg(long = "desc", conflicts_with = "asc")]
    desc: bool,

    /// List available output fields and exit (use with --json for machine-readable output)
    #[arg(short = 'L', long = "list-fields")]
    pub field_list: bool,
}

impl SearchArgs {
    /// Whether LTSV format is requested (for -i mode: selector display format).
    pub fn ltsv(&self) -> bool {
        self.ltsv
    }

    /// Parse --sort into Field (default: ModifiedAt).
    pub fn sort_field(&self) -> Result<Field, String> {
        match self.sort.as_deref() {
            Some(s) => s.parse::<Field>(),
            None => Ok(Field::ModifiedAt),
        }
    }

    /// Whether this command should use the pager.
    pub fn wants_pager(&self) -> bool {
        !self.json && !self.ltsv && !self.tsv && !self.table && !self.field_list
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShowFormat {
    Pretty,
    Raw,
    Json,
    Md,
}

#[derive(Parser, Debug)]
pub struct ShowArgs {
    #[command(flatten)]
    pub common: CommonArgs,

    /// Show first N messages only
    #[arg(long = "head")]
    pub head: Option<usize>,

    /// Pretty-print with colors (default)
    #[arg(long = "pretty", conflicts_with_all = ["raw", "json", "md"])]
    pretty: bool,

    /// Output raw session file content
    #[arg(long = "raw", conflicts_with_all = ["pretty", "json", "md"])]
    raw: bool,

    /// Output normalized JSON Lines ({"role":"user","text":"..."})
    #[arg(long = "json", conflicts_with_all = ["pretty", "raw", "md"])]
    json: bool,

    /// Output as Markdown (## User / ## Assistant headers)
    #[arg(long = "md", conflicts_with_all = ["pretty", "raw", "json"])]
    md: bool,

    /// Follow session output in real-time (like tail -f)
    #[arg(short = 'f', long = "follow")]
    pub follow: bool,

    /// Session ID or path
    pub session: Option<String>,
}

impl ShowArgs {
    /// Build a ShowArgs for internal use (e.g. from fuzzy selection).
    pub fn with_session(head: Option<usize>, session: Option<String>) -> Self {
        ShowArgs {
            common: CommonArgs { fields: None },
            head,
            pretty: false,
            raw: false,
            json: false,
            md: false,
            follow: false,
            session,
        }
    }

    pub fn format(&self) -> ShowFormat {
        if self.raw {
            ShowFormat::Raw
        } else if self.json {
            ShowFormat::Json
        } else if self.md {
            ShowFormat::Md
        } else {
            ShowFormat::Pretty
        }
    }

    /// Whether this command should use the pager.
    pub fn wants_pager(&self) -> bool {
        !self.follow && matches!(self.format(), ShowFormat::Pretty | ShowFormat::Md)
    }
}

#[derive(Parser, Debug)]
pub struct ResumeArgs {
    #[command(flatten)]
    pub common: CommonArgs,

    /// Print the resolved resume command and exit (read-only; does not execute)
    #[arg(long = "print")]
    pub print: bool,

    /// Session ID or path
    pub session: Option<String>,

    /// Use LTSV format for interactive selector display
    #[arg(long = "ltsv")]
    pub ltsv: bool,

    /// Extra arguments passed to the agent command (after --)
    #[arg(last = true)]
    pub extra_args: Vec<String>,
}

#[derive(Parser, Debug)]
pub struct AgentArgs {
    /// Output as table (default on TTY)
    #[arg(long = "table", conflicts_with_all = ["tsv", "ltsv", "json"])]
    table: bool,

    /// Output as TSV (tab-separated values; default when piped)
    #[arg(long = "tsv", conflicts_with_all = ["table", "ltsv", "json"])]
    tsv: bool,

    /// Output as LTSV (Labeled Tab-Separated Values)
    #[arg(long = "ltsv", conflicts_with_all = ["table", "json", "tsv"])]
    pub ltsv: bool,

    /// Output as JSON Lines
    #[arg(long = "json", conflicts_with_all = ["table", "ltsv", "tsv"])]
    pub json: bool,
}

impl AgentArgs {
    pub fn output_format(&self) -> OutputFormat {
        OutputFormat::from_flags(self.json, self.ltsv, self.tsv, self.table)
    }

    pub fn wants_pager(&self) -> bool {
        !self.json && !self.ltsv && !self.tsv && !self.table
    }
}

#[derive(Parser, Debug)]
pub struct ListAgentsArgs {
    /// Output as table (default on TTY)
    #[arg(long = "table", conflicts_with_all = ["tsv", "ltsv", "json"])]
    table: bool,

    /// Output as JSON Lines
    #[arg(long = "json", conflicts_with_all = ["table", "ltsv", "tsv"])]
    pub json: bool,

    /// Output as TSV (tab-separated values; default when piped)
    #[arg(long = "tsv", conflicts_with_all = ["table", "ltsv", "json"])]
    pub tsv: bool,

    /// Output as LTSV (Labeled Tab-Separated Values)
    #[arg(long = "ltsv", conflicts_with_all = ["table", "json", "tsv"])]
    pub ltsv: bool,
}

impl ListAgentsArgs {
    pub fn output_format(&self) -> OutputFormat {
        OutputFormat::from_flags(self.json, self.ltsv, self.tsv, self.table)
    }

    pub fn wants_pager(&self) -> bool {
        !self.json && !self.ltsv && !self.tsv && !self.table
    }
}

/// Interactive mode options (fuzzy finder).
#[derive(Parser, Debug)]
pub struct InteractiveArgs {
    /// Interactive mode via fuzzy finder (like git rebase -i)
    #[arg(short = 'i', long = "interactive", global = true)]
    pub interactive: bool,

    /// Selector executable path (for -i mode; default: $AH_SELECTOR or fzf)
    #[arg(short = 's', global = true, requires = "interactive")]
    pub selector: Option<String>,

    /// Disable preview in interactive mode
    #[arg(long = "no-preview", global = true, requires = "interactive")]
    pub no_preview: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProjectField {
    Project,
    ProjectRaw,
    Cwd,
    SessionCount,
    Sessions,
    Agents,
    LastModifiedAt,
    FirstCreatedAt,
}

#[allow(dead_code)]
impl ProjectField {
    pub fn name(&self) -> &'static str {
        match self {
            ProjectField::Project => "project",
            ProjectField::ProjectRaw => "project_raw",
            ProjectField::Cwd => "cwd",
            ProjectField::SessionCount => "session_count",
            ProjectField::Sessions => "sessions",
            ProjectField::Agents => "agents",
            ProjectField::LastModifiedAt => "last_modified_at",
            ProjectField::FirstCreatedAt => "first_created_at",
        }
    }

    pub fn all() -> Vec<ProjectField> {
        vec![
            ProjectField::Project,
            ProjectField::ProjectRaw,
            ProjectField::Cwd,
            ProjectField::SessionCount,
            ProjectField::Sessions,
            ProjectField::Agents,
            ProjectField::LastModifiedAt,
            ProjectField::FirstCreatedAt,
        ]
    }

    pub fn all_names() -> &'static str {
        "project, project_raw, cwd, session_count, sessions, agents, last_modified_at, first_created_at"
    }

    pub fn description(&self) -> &'static str {
        match self {
            ProjectField::Project => "Normalized project name",
            ProjectField::ProjectRaw => "Agent-native identifiers (comma-separated)",
            ProjectField::Cwd => "Working directory",
            ProjectField::SessionCount => "Session count",
            ProjectField::Sessions => "Session details as JSON array",
            ProjectField::Agents => "Agent names (comma-separated)",
            ProjectField::LastModifiedAt => "Last session modified time",
            ProjectField::FirstCreatedAt => "First session created time",
        }
    }

    pub fn example(&self) -> &'static str {
        match self {
            ProjectField::Project => "myapp",
            ProjectField::ProjectRaw => "src/github.com/org/myapp",
            ProjectField::Cwd => "/home/user/src/github.com/org/myapp",
            ProjectField::SessionCount => "42",
            ProjectField::Sessions => r#"[{"agent":"claude","title":"fix auth",...}]"#,
            ProjectField::Agents => "claude, codex, gemini",
            ProjectField::LastModifiedAt => "2026-03-21 15:16",
            ProjectField::FirstCreatedAt => "2026-01-15 10:00",
        }
    }

    /// Whether this field should be compared numerically when sorting.
    pub fn is_numeric(&self) -> bool {
        matches!(self, ProjectField::SessionCount)
    }
}

impl FromStr for ProjectField {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "project" => Ok(ProjectField::Project),
            "project_raw" => Ok(ProjectField::ProjectRaw),
            "cwd" => Ok(ProjectField::Cwd),
            "session_count" => Ok(ProjectField::SessionCount),
            "sessions" => Ok(ProjectField::Sessions),
            "agents" => Ok(ProjectField::Agents),
            "last_modified_at" => Ok(ProjectField::LastModifiedAt),
            "first_created_at" => Ok(ProjectField::FirstCreatedAt),
            _ => Err(format!(
                "unknown field '{}'. available: {}",
                s,
                ProjectField::all_names()
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MemoryField {
    Agent,
    Project,
    Type,
    Name,
    Description,
    ModifiedAt,
    CreatedAt,
    Size,
    Lines,
    FileName,
    Path,
    Body,
    Matched,
}

#[allow(dead_code)]
impl MemoryField {
    pub fn name(&self) -> &'static str {
        match self {
            MemoryField::Agent => "agent",
            MemoryField::Project => "project",
            MemoryField::Type => "type",
            MemoryField::Name => "name",
            MemoryField::Description => "description",
            MemoryField::ModifiedAt => "modified_at",
            MemoryField::CreatedAt => "created_at",
            MemoryField::Size => "size",
            MemoryField::Lines => "lines",
            MemoryField::FileName => "file_name",
            MemoryField::Path => "path",
            MemoryField::Body => "body",
            MemoryField::Matched => "matched",
        }
    }

    pub fn all() -> Vec<MemoryField> {
        vec![
            MemoryField::Agent,
            MemoryField::Project,
            MemoryField::Type,
            MemoryField::Name,
            MemoryField::Description,
            MemoryField::ModifiedAt,
            MemoryField::CreatedAt,
            MemoryField::Size,
            MemoryField::Lines,
            MemoryField::FileName,
            MemoryField::Path,
            MemoryField::Body,
            MemoryField::Matched,
        ]
    }

    pub fn all_names() -> &'static str {
        "agent, project, type, name, description, modified_at, created_at, size, lines, file_name, path, body, matched"
    }

    pub fn description(&self) -> &'static str {
        match self {
            MemoryField::Agent => "Agent name",
            MemoryField::Project => "Decoded project path",
            MemoryField::Type => "Memory type (user/feedback/project/reference/instruction)",
            MemoryField::Name => "Memory name (from frontmatter or filename)",
            MemoryField::Description => "Memory description (from frontmatter)",
            MemoryField::ModifiedAt => "File modified time",
            MemoryField::CreatedAt => "File creation time (birthtime)",
            MemoryField::Size => "File size in bytes",
            MemoryField::Lines => "Number of lines",
            MemoryField::FileName => "File name",
            MemoryField::Path => "File path",
            MemoryField::Body => "Memory body content",
            MemoryField::Matched => "Matched search snippet",
        }
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self, MemoryField::Size | MemoryField::Lines)
    }

    pub fn example(&self) -> &'static str {
        match self {
            MemoryField::Agent => "claude",
            MemoryField::Project => "myapp",
            MemoryField::Type => "feedback",
            MemoryField::Name => "fzf/peco embed support keep",
            MemoryField::Description => "ahs should keep fzf/peco embedded...",
            MemoryField::ModifiedAt => "2026-03-21 15:16",
            MemoryField::CreatedAt => "2026-03-20 10:00",
            MemoryField::Size => "720",
            MemoryField::Lines => "11",
            MemoryField::FileName => "feedback_fzf_embed.md",
            MemoryField::Path => "~/.claude/projects/-Users-.../memory/feedback_fzf.md",
            MemoryField::Body => "Memory content text...",
            MemoryField::Matched => "...matched line...",
        }
    }
}

impl FromStr for MemoryField {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "agent" => Ok(MemoryField::Agent),
            "project" => Ok(MemoryField::Project),
            "type" => Ok(MemoryField::Type),
            "name" => Ok(MemoryField::Name),
            "description" => Ok(MemoryField::Description),
            "modified_at" => Ok(MemoryField::ModifiedAt),
            "created_at" => Ok(MemoryField::CreatedAt),
            "size" => Ok(MemoryField::Size),
            "lines" => Ok(MemoryField::Lines),
            "file_name" => Ok(MemoryField::FileName),
            "path" => Ok(MemoryField::Path),
            "body" => Ok(MemoryField::Body),
            "matched" => Ok(MemoryField::Matched),
            _ => Err(format!(
                "unknown field '{}'. available: {}",
                s,
                MemoryField::all_names()
            )),
        }
    }
}

#[derive(Parser, Debug)]
pub struct MemoryArgs {
    /// Select output fields (comma-separated, replaces defaults)
    #[arg(short = 'o', long = "fields")]
    pub fields: Option<String>,

    /// Add fields to defaults (comma-separated)
    #[arg(short = 'O', long = "extra-fields")]
    extra_fields: Option<String>,

    /// Output as table (default on TTY)
    #[arg(long = "table", conflicts_with_all = ["tsv", "ltsv", "json"])]
    table: bool,

    /// Output as TSV (tab-separated values; default when piped)
    #[arg(long = "tsv", conflicts_with_all = ["table", "ltsv", "json"])]
    tsv: bool,

    /// Output as LTSV (Labeled Tab-Separated Values)
    #[arg(long = "ltsv", conflicts_with_all = ["table", "json", "tsv"])]
    ltsv: bool,

    /// Output as JSON Lines
    #[arg(long = "json", conflicts_with_all = ["table", "ltsv", "tsv"])]
    pub json: bool,

    /// Sort by field (default: modified_at)
    #[arg(short = 'S', long = "sort")]
    sort: Option<String>,

    /// Sort ascending
    #[arg(long = "asc", conflicts_with = "desc")]
    asc: bool,

    /// Sort descending (default)
    #[arg(long = "desc", conflicts_with = "asc")]
    desc: bool,

    /// Filter by memory type (user/feedback/project/reference/instruction)
    #[arg(short = 't', long = "type")]
    pub memory_type: Option<String>,

    /// List available output fields and exit (use with --json for machine-readable output)
    #[arg(short = 'L', long = "list-fields")]
    pub field_list: bool,
}

impl MemoryArgs {
    pub fn wants_pager(&self) -> bool {
        !self.json && !self.ltsv && !self.tsv && !self.table && !self.field_list
    }
}

#[derive(Debug)]
pub struct MemoryResolvedArgs {
    pub fields: Vec<MemoryField>,
    pub output_format: OutputFormat,
    pub sort_order: SortOrder,
    pub sort_field: MemoryField,
    pub memory_type: Option<String>,
}

impl MemoryResolvedArgs {
    pub fn from_args(raw: MemoryArgs) -> Result<Self, String> {
        let output_format = OutputFormat::from_flags(raw.json, raw.ltsv, raw.tsv, raw.table);
        let sort_order = SortOrder::from_flag(raw.asc);
        let sort_field = match raw.sort.as_deref() {
            Some(s) => s.parse::<MemoryField>()?,
            None => MemoryField::ModifiedAt,
        };

        let default_fields = vec![
            MemoryField::Agent,
            MemoryField::Project,
            MemoryField::Type,
            MemoryField::Name,
            MemoryField::ModifiedAt,
            MemoryField::Description,
        ];
        let fields = if let Some(ref f) = raw.fields {
            parse_field_list(f)?
        } else if let Some(ref f) = raw.extra_fields {
            let mut fields = default_fields;
            fields.extend(parse_field_list::<MemoryField>(f)?);
            fields
        } else {
            default_fields
        };

        Ok(MemoryResolvedArgs {
            fields,
            output_format,
            sort_order,
            sort_field,
            memory_type: raw.memory_type,
        })
    }

    /// Build args for `ah memory -i` (includes Path for key; sort matches list output).
    pub fn from_args_interactive(raw: &MemoryArgs) -> Result<Self, String> {
        let sort_order = SortOrder::Desc;
        let sort_field = match raw.sort.as_deref() {
            Some(s) => s.parse::<MemoryField>()?,
            None => MemoryField::ModifiedAt,
        };

        let mut fields: Vec<MemoryField> = if let Some(ref f) = raw.fields {
            parse_field_list(f)?
        } else {
            vec![
                MemoryField::Agent,
                MemoryField::Project,
                MemoryField::Type,
                MemoryField::Name,
                MemoryField::ModifiedAt,
            ]
        };

        // Ensure Path is always present (used as key for selection)
        if !fields.contains(&MemoryField::Path) {
            fields.insert(0, MemoryField::Path);
        } else if fields[0] != MemoryField::Path {
            fields.retain(|&x| x != MemoryField::Path);
            fields.insert(0, MemoryField::Path);
        }

        Ok(MemoryResolvedArgs {
            fields,
            output_format: OutputFormat::Tsv,
            sort_order,
            sort_field,
            memory_type: raw.memory_type.clone(),
        })
    }
}

#[derive(Parser, Debug)]
pub struct ListProjectsArgs {
    /// Select output fields (comma-separated, replaces defaults)
    #[arg(short = 'o', long = "fields")]
    pub fields: Option<String>,

    /// Add fields to defaults (comma-separated)
    #[arg(short = 'O', long = "extra-fields")]
    extra_fields: Option<String>,

    /// Output as table (default on TTY)
    #[arg(long = "table", conflicts_with_all = ["tsv", "ltsv", "json"])]
    table: bool,

    /// Output as TSV (tab-separated values; default when piped)
    #[arg(long = "tsv", conflicts_with_all = ["table", "ltsv", "json"])]
    tsv: bool,

    /// Output as LTSV (Labeled Tab-Separated Values)
    #[arg(long = "ltsv", conflicts_with_all = ["table", "json", "tsv"])]
    ltsv: bool,

    /// Output as JSON Lines
    #[arg(long = "json", conflicts_with_all = ["table", "ltsv", "tsv"])]
    pub json: bool,

    /// Sort by field (default: last_modified_at)
    #[arg(short = 'S', long = "sort")]
    sort: Option<String>,

    /// Sort ascending
    #[arg(long = "asc", conflicts_with = "desc")]
    asc: bool,

    /// Sort descending (default)
    #[arg(long = "desc", conflicts_with = "asc")]
    desc: bool,

    /// List available output fields and exit (use with --json for machine-readable output)
    #[arg(short = 'L', long = "list-fields")]
    pub field_list: bool,
}

impl ListProjectsArgs {
    /// Whether LTSV format is requested (for -i mode: selector display format).
    pub fn ltsv(&self) -> bool {
        self.ltsv
    }

    pub fn wants_pager(&self) -> bool {
        !self.json && !self.ltsv && !self.tsv && !self.table && !self.field_list
    }
}

#[derive(Debug)]
pub struct ListProjectsResolvedArgs {
    pub fields: Vec<ProjectField>,
    pub output_format: OutputFormat,
    pub sort_order: SortOrder,
    pub sort_field: ProjectField,
}

impl ListProjectsResolvedArgs {
    pub fn from_args(raw: ListProjectsArgs) -> Result<Self, String> {
        let output_format = OutputFormat::from_flags(raw.json, raw.ltsv, raw.tsv, raw.table);
        let sort_order = SortOrder::from_flag(raw.asc);
        let sort_field = match raw.sort.as_deref() {
            Some(s) => s.parse::<ProjectField>()?,
            None => ProjectField::LastModifiedAt,
        };

        let default_fields = vec![
            ProjectField::Project,
            ProjectField::SessionCount,
            ProjectField::LastModifiedAt,
            ProjectField::Agents,
        ];
        let fields = if let Some(ref f) = raw.fields {
            parse_field_list(f)?
        } else if let Some(ref f) = raw.extra_fields {
            let mut fields = default_fields;
            fields.extend(parse_field_list::<ProjectField>(f)?);
            fields
        } else {
            default_fields
        };

        Ok(ListProjectsResolvedArgs {
            fields,
            output_format,
            sort_order,
            sort_field,
        })
    }

    /// Build args for `ah project -i` (includes cwd for the first column; sort matches list output).
    pub fn from_interactive(raw: &ListProjectsArgs) -> Result<Self, String> {
        let sort_order = SortOrder::Desc;
        let sort_field = match raw.sort.as_deref() {
            Some(s) => s.parse::<ProjectField>()?,
            None => ProjectField::LastModifiedAt,
        };

        let mut fields: Vec<ProjectField> = if let Some(ref f) = raw.fields {
            parse_field_list(f)?
        } else {
            vec![
                ProjectField::Project,
                ProjectField::SessionCount,
                ProjectField::LastModifiedAt,
            ]
        };

        if !fields.contains(&ProjectField::Cwd) {
            fields.insert(0, ProjectField::Cwd);
        } else if fields[0] != ProjectField::Cwd {
            fields.retain(|&x| x != ProjectField::Cwd);
            fields.insert(0, ProjectField::Cwd);
        }

        Ok(ListProjectsResolvedArgs {
            fields,
            output_format: OutputFormat::Tsv,
            sort_order,
            sort_field,
        })
    }
}

#[derive(Debug)]
pub struct Args {
    pub search_mode: SearchMode,
    pub output_format: OutputFormat,
    pub fields: Vec<Field>,
    pub limit: usize,
    pub sort_order: SortOrder,
    pub sort_field: Field,
    pub query: String,
    pub transcript_limit: usize,
    pub title_limit: usize,
    pub filters: Vec<FieldFilter>,
    pub since: Option<SystemTime>,
    pub until: Option<SystemTime>,
    pub running: bool,
}

impl Args {
    pub fn from_search_args(raw: SearchArgs, filter: &FilterArgs) -> Result<Self, String> {
        let query = filter.query.clone().unwrap_or_default();
        let search_mode = filter.search_mode();
        let output_format = OutputFormat::from_flags(raw.json, raw.ltsv, raw.tsv, raw.table);
        let sort_order = SortOrder::from_flag(raw.asc);
        let sort_field = match raw.sort.as_deref() {
            Some(s) => s.parse::<Field>()?,
            None => Field::ModifiedAt,
        };

        let default_fields = if output_format == OutputFormat::Log {
            if !query.is_empty() {
                vec![
                    Field::Agent,
                    Field::Project,
                    Field::Cwd,
                    Field::CreatedAt,
                    Field::ModifiedAt,
                    Field::Title,
                    Field::FirstPrompt,
                    Field::Matched,
                    Field::Id,
                ]
            } else {
                vec![
                    Field::Agent,
                    Field::Project,
                    Field::Cwd,
                    Field::CreatedAt,
                    Field::ModifiedAt,
                    Field::Title,
                    Field::FirstPrompt,
                    Field::Id,
                ]
            }
        } else if !query.is_empty() {
            vec![
                Field::Agent,
                Field::Project,
                Field::ModifiedAt,
                Field::Title,
                Field::Matched,
                Field::Id,
            ]
        } else {
            vec![
                Field::Agent,
                Field::Project,
                Field::ModifiedAt,
                Field::Title,
                Field::Id,
            ]
        };
        let fields = if let Some(fields) = raw.common.parse_fields()? {
            fields
        } else if let Some(ref f) = raw.extra_fields {
            let mut fields = default_fields;
            fields.extend(parse_field_list::<Field>(f)?);
            fields
        } else {
            default_fields
        };

        let since = filter.since_time()?;
        let until = filter.until_time()?;
        let title_limit = if output_format == OutputFormat::Log {
            0
        } else {
            raw.title_limit
        };

        Ok(Args {
            search_mode,
            output_format,
            fields,
            limit: filter.limit,
            sort_order,
            sort_field,
            query,
            transcript_limit: raw.transcript_limit,
            title_limit,
            filters: filter.to_filters(),
            since,
            until,
            running: filter.running,
        })
    }
}

const TOP_HELP_TEXT: &str = r#"Search, inspect, and resume coding-agent sessions from one CLI
(read-only except resume)

Defaults to the current directory. Use -a to search across all known sessions.

Usage:
  ah <COMMAND> [OPTIONS]

Commands:
  log                 List sessions
  project             List known projects
  show                Show session transcript
  resume              Resume an agent session
  memory              List agent memory and instruction files
  agent               Show session summary per agent

Help / setup:
  list-agents         List supported agents
  completion          Generate shell completion script
  man                 Generate man page

Global options:
  -a, --all               Show all sessions (disable default cwd filtering)
  -A                      Show all sessions including all configured remotes
  --agent <NAME>          Filter by agent name (e.g. claude, codex, gemini)
  --project <NAME>        Filter by project name
  -d, --dir <PATH>        Filter by working directory (default: current directory)
  -q, --query <REGEX>     Full-text search query (regex, case-insensitive)
  -p, --prompt-only       Search only user prompts (use with -q)
  -n, --limit N           Max session files to scan (default: 0, no limit)
  -i, --interactive       Interactive mode via fuzzy finder (fzf/sk)
  -s <CMD>                Override fuzzy selector (default: $AH_SELECTOR or fzf)
  --no-preview            Disable preview in interactive mode
  --running               Show only currently running sessions (Claude only for now)
  --remote <NAME>         Include sessions from remote host (requires ah on remote; see ~/.ahrc [remotes.*])
  --since <SPEC>          Show sessions newer than (e.g. "2026-03-20", "3d", "1w", "2m" = ~60 days)
  --until <SPEC>          Show sessions older than (e.g. "2026-03-20", "3d", "1w", "2m" = ~60 days)
  --color                 Force colored output (even through pipes)
  --no-color              Disable colored output
  --no-pager              Disable automatic pager
  -h, --help              Show this help
  -V, --version           Show version

Examples:
  ah log                      # latest sessions for the current directory
  ah log -a -q "auth"         # search across all known sessions
  ah log -A                   # all sessions including all remotes
  ah resume                   # resume the latest matching session
  ah show -q "OAuth"          # show the latest matching session
  ah resume -i                # browse sessions with fzf/sk and resume

Run `ah <COMMAND> --help` for subcommand-specific options.

Configuration:
  ~/.ahrc (TOML) — optional config file for agent customization and remote hosts.
  See https://github.com/nihen/ah#configuration-ahrc for details."#;

const GLOBAL_OPTIONS: &str = r#"Global options:
  -a, --all               Show all sessions (disable default cwd filtering)
  -A                      Show all sessions including all configured remotes
  --agent <NAME>          Filter by agent name (e.g. claude, codex, gemini)
  --project <NAME>        Filter by project name
  -d, --dir <PATH>        Filter by working directory (default: current directory)
  -q, --query <REGEX>     Full-text search query (regex, case-insensitive)
  -p, --prompt-only       Search only user prompts (use with -q)
  -n, --limit N           Max session files to scan (default: 0, no limit)
  --since <SPEC>          Show sessions newer than (e.g. "2026-03-20", "3d", "1w", "2m" = ~60 days)
  --until <SPEC>          Show sessions older than (e.g. "2026-03-20", "3d", "1w", "2m" = ~60 days)
  --running               Show only currently running sessions (Claude only for now)
  --remote <NAME>         Include sessions from remote host (requires ah on remote; see ~/.ahrc [remotes.*])
  --color                 Force colored output (even through pipes)
  --no-color              Disable colored output
  --no-pager              Disable automatic pager
  --debug                 Show debug info (glob expansion, scan counts, timing) on stderr"#;

const DISPLAY_OPTIONS: &str = r#"Display options:
  --color                 Force colored output (even through pipes)
  --no-color              Disable colored output
  --no-pager              Disable automatic pager
  --debug                 Show debug info (glob expansion, scan counts, timing) on stderr"#;

const PROJECT_GLOBAL_OPTIONS: &str = concatcp!(
    r#"Global options:
  -a, --all               Show all projects (default; -a has no effect)
  -A                      Show all projects including all configured remotes
  --agent <NAME>          Filter by agent name (e.g. claude, codex, gemini)
  -d, --dir <PATH>        Filter by working directory
  -n, --limit N           Max session files to scan (default: 0, no limit)
  --remote <NAME>         Include projects from remote host (requires ah on remote; see ~/.ahrc [remotes.*])
  --since <SPEC>          Show sessions newer than (e.g. "2026-03-20", "3d", "1w", "2m" = ~60 days)
  --until <SPEC>          Show sessions older than (e.g. "2026-03-20", "3d", "1w", "2m" = ~60 days)

"#,
    DISPLAY_OPTIONS
);

const MEMORY_GLOBAL_OPTIONS: &str = concatcp!(
    r#"Global options:
  -a, --all               Show all memory files (disable default cwd filtering)
  -A                      Show all memory files including all configured remotes
  --agent <NAME>          Filter by agent name (e.g. claude)
  -d, --dir <PATH>        Filter by working directory (default: current directory)
  -q, --query <REGEX>     Search query (regex, case-insensitive)
  --remote <NAME>         Include memory files from remote host (requires ah on remote; see ~/.ahrc [remotes.*])
  --since <SPEC>          Show files newer than (e.g. "2026-03-20", "3d", "1w")
  --until <SPEC>          Show files older than (e.g. "2026-03-20", "3d", "1w")

"#,
    DISPLAY_OPTIONS
);

const AGENT_GLOBAL_OPTIONS: &str = concatcp!(
    r#"Global options:
  -a, --all               Show all agents (disable default cwd filtering)
  -A                      Show all agents including all configured remotes
  --agent <NAME>          Filter by agent name (e.g. claude, codex, gemini)
  -n, --limit N           Max session files to scan (default: 0, no limit)
  --remote <NAME>         Include sessions from remote host (requires ah on remote; see ~/.ahrc [remotes.*])
  --since <SPEC>          Show sessions newer than (e.g. "2026-03-20", "3d", "1w", "2m" = ~60 days)
  --until <SPEC>          Show sessions older than (e.g. "2026-03-20", "3d", "1w", "2m" = ~60 days)

"#,
    DISPLAY_OPTIONS
);

const LOG_HELP_TEXT: &str = concatcp!(
    "List sessions

Usage:
  ah log [OPTIONS]

Options:
  -o, --fields <FIELDS>   Select output fields (replaces defaults, see --list-fields)
                          Default: agent, project, modified_at, title, id
                          With -q: agent, project, modified_at, title, matched, id
  -O, --extra-fields <FIELDS>  Add fields to defaults (comma-separated)
  --table                 Aligned table with header row
  --tsv                   Tab-separated values (no header, no color)
  --ltsv                  Labeled Tab-Separated Values (in -i mode: selector display format)
  --json                  JSON Lines output
  -S, --sort <FIELD>      Sort by field (default: modified_at)
  --asc                   Sort ascending
  --desc                  Sort descending (default)
  --transcript-limit N    Max characters for transcript field (default: 500)
  --title-limit N         Max characters for auto-generated title (default: 50, 0 = no limit)
  -L, --list-fields       List available output fields and exit (use with --json for machine-readable output)

Default output (when no format flag is given):
  git-log style multi-line with auto-pager on TTY, plain TSV when piped

Interactive mode:
  -i, --interactive       Browse sessions via fuzzy finder; prints selected path
  -s <CMD>                Selector command (default: $AH_SELECTOR or fzf)
  --no-preview            Disable transcript preview (enabled by default for fzf, sk)

",
    GLOBAL_OPTIONS
);

const SHOW_HELP_TEXT: &str = concatcp!(
    "Show session transcript

Usage:
  ah show [OPTIONS] [SESSION]

If SESSION is omitted, ah shows the latest session matching stdin, -q, and other filters.

Options:
  --head N                Show first N messages only
  --pretty                Pretty-print with colors (default)
  --raw                   Output raw session file content
  --json                  Output normalized JSON Lines ({\"role\":\"user\",\"text\":\"...\"})
  --md                    Output as Markdown (## User / ## Assistant headers)
  -f, --follow            Follow session output in real-time (like tail -f)

Interactive mode:
  -i, --interactive       Select session via fuzzy finder then show it
  -o, --fields <FIELDS>   Display fields in interactive mode (default: agent,project,modified_at,title)
  -s <CMD>                Selector command (default: $AH_SELECTOR or fzf)
  --no-preview            Disable transcript preview

",
    GLOBAL_OPTIONS
);

const RESUME_HELP_TEXT: &str = concatcp!(
    "Resume an agent session

Usage:
  ah resume [OPTIONS] [SESSION] [-- EXTRA_ARGS...]

If SESSION is omitted, ah resumes the latest session matching stdin, -q, and other filters.

Arguments after -- are passed directly to the agent command.

The only command that launches an agent process; other commands are read-only.

Options:
  --print                 Print the resolved resume command and exit (read-only; does not execute)

Interactive mode:
  -i, --interactive       Select session via fuzzy finder then resume it
  -o, --fields <FIELDS>   Display fields in interactive mode (default: agent,project,modified_at,title)
  -s <CMD>                Selector command (default: $AH_SELECTOR or fzf)
  --ltsv                  Use LTSV format for interactive selector display
  --no-preview            Disable transcript preview

Examples:
  ah resume                   # resume latest matching session
  ah resume --print           # print the resolved resume command
  ah resume a1b2c3d4          # resume by ID
  ah resume -i                # interactive selection
  ah resume -- --dry-run      # pass extra args to agent

",
    GLOBAL_OPTIONS
);

const PROJECT_HELP_TEXT: &str = concatcp!(
    "List known projects

Usage:
  ah project [OPTIONS]

Options:
  -o, --fields <FIELDS>   Select output fields (replaces defaults, see --list-fields)
                          Default: project, session_count, last_modified_at, agents
                          In -i mode default: cwd, project, session_count, last_modified_at
  -O, --extra-fields <FIELDS>  Add fields to defaults (comma-separated)
  --table                 Aligned table with header row
  --tsv                   Tab-separated values (no header, no color)
  --ltsv                  Labeled Tab-Separated Values (in -i mode: selector display format)
  --json                  JSON Lines output
  -S, --sort <FIELD>      Sort by field (default: last_modified_at)
  --asc                   Sort ascending
  --desc                  Sort descending (default)
  -L, --list-fields       List available output fields and exit (use with --json for machine-readable output)

Default output (when no format flag is given):
  Aligned table with auto-pager on TTY, plain TSV when piped

Interactive mode:
  -i, --interactive       Browse projects via fuzzy finder; prints selected cwd
  -s <CMD>                Selector command (default: $AH_SELECTOR or fzf)
  --no-preview            Disable preview

",
    PROJECT_GLOBAL_OPTIONS
);

const AGENT_HELP_TEXT: &str = concatcp!(
    "Show session summary per agent

Usage:
  ah agent [OPTIONS]

Shows how many sessions were found for each agent and the latest modified time.

Options:
  --table                 Output as aligned table
  --tsv                   Output as TSV (tab-separated values)
  --ltsv                  Output as LTSV (Labeled TSV)
  --json                  Output as JSON Lines

Default output (when no format flag is given):
  Aligned table with auto-pager on TTY, plain TSV when piped

",
    AGENT_GLOBAL_OPTIONS
);

const MEMORY_HELP_TEXT: &str = concatcp!(
    "List agent memory files

Usage:
  ah memory [OPTIONS]

Options:
  -o, --fields <FIELDS>   Select output fields (replaces defaults, see --list-fields)
                          Default: agent, project, type, name, modified_at, description
  -O, --extra-fields <FIELDS>  Add fields to defaults (comma-separated)
  -t, --type <TYPE>       Filter by memory type (user/feedback/project/reference/instruction)
  --table                 Aligned table with header row
  --tsv                   Tab-separated values (no header, no color)
  --ltsv                  Labeled Tab-Separated Values
  --json                  JSON Lines output
  -S, --sort <FIELD>      Sort by field (default: modified_at)
  --asc                   Sort ascending
  --desc                  Sort descending (default)
  -L, --list-fields       List available output fields and exit (use with --json for machine-readable output)

Default output (when no format flag is given):
  Aligned table with auto-pager on TTY, plain TSV when piped

Interactive mode:
  -i, --interactive       Browse memory files via fuzzy finder
  -s <CMD>                Selector command (default: $AH_SELECTOR or fzf)
  --no-preview            Disable preview

",
    MEMORY_GLOBAL_OPTIONS
);

const LIST_AGENTS_HELP_TEXT: &str = r#"List supported agents

Usage:
  ah list-agents [OPTIONS]

Options:
  --table   Table output (default on TTY)
  --tsv     Tab-separated values (default when piped)
  --ltsv    Labeled Tab-Separated Values
  --json    JSON Lines output

Shows each built-in agent, its file patterns, and supported capabilities.

Examples:
  ah list-agents
  ah list-agents --json
  ah list-agents --json | jq '.id'
  ah list-agents --tsv | column -t -s$'\t'"#;

const COMPLETION_HELP_TEXT: &str = r#"Generate shell completion script

Usage:
  ah completion <SHELL>

Supported shells:
  bash, zsh, fish, elvish, powershell

Setup:
  zsh:  mkdir -p ~/.zfunc && ah completion zsh > ~/.zfunc/_ah
        # Add to .zshrc: fpath=(~/.zfunc $fpath); autoload -Uz compinit && compinit
  bash: ah completion bash > ~/.local/share/bash-completion/completions/ah
  fish: ah completion fish > ~/.config/fish/completions/ah.fish"#;

const MAN_HELP_TEXT: &str = r#"Generate man page

Usage:
  ah man [SUBCOMMAND]
  ah man --out-dir <DIR>

The generated man page is written to stdout in roff format.
Use --out-dir to generate all man pages (ah.1, ah-log.1, etc.) into a directory.

Examples:
  ah man                      # main man page to stdout
  ah man log                  # ah-log(1) man page to stdout
  ah man --out-dir ./man      # generate all pages into ./man/
  ah man | mandoc -man        # preview the main man page
  # or via: make install"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_field_from_str() {
        assert_eq!(Field::from_str("agent"), Ok(Field::Agent));
        assert_eq!(Field::from_str("modified_at"), Ok(Field::ModifiedAt));
        assert_eq!(Field::from_str("created_at"), Ok(Field::CreatedAt));
        assert_eq!(Field::from_str("id"), Ok(Field::Id));
        assert_eq!(Field::from_str("resume_cmd"), Ok(Field::ResumeCmd));
        assert_eq!(Field::from_str("first_prompt"), Ok(Field::FirstPrompt));
        assert_eq!(Field::from_str("transcript"), Ok(Field::Transcript));
        assert_eq!(Field::from_str("responses"), Ok(Field::Responses));
        assert_eq!(Field::from_str("messages"), Ok(Field::Messages));
        assert!(Field::from_str("invalid").is_err());
        assert!(Field::from_str("body").is_err());
    }

    #[test]
    fn test_field_name_roundtrip() {
        for f in &Field::all() {
            assert_eq!(Field::from_str(f.name()), Ok(*f));
        }
    }

    #[test]
    fn test_field_all_count() {
        assert_eq!(Field::all().len(), 21);
    }

    #[test]
    fn test_field_all_no_duplicates() {
        let all = Field::all();
        let mut deduped = all.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(all.len(), deduped.len());
    }

    #[test]
    fn test_parse_fields_comma_separated() {
        let input = "agent,title,path";
        let fields: Result<Vec<Field>, _> = input
            .split(',')
            .map(|s| Field::from_str(s.trim()))
            .collect();
        assert_eq!(
            fields.unwrap(),
            vec![Field::Agent, Field::Title, Field::Path]
        );
    }

    #[test]
    fn test_parse_fields_invalid_errors() {
        assert!(Field::from_str("nonexistent").is_err());
    }

    #[test]
    fn test_field_filter_from_options() {
        let filters =
            FieldFilter::from_options(&Some("claude".to_string()), &Some("myapp".to_string()));
        assert_eq!(filters.len(), 2);
        assert_eq!(filters[0].field, Field::Agent);
        assert_eq!(filters[0].value, "claude");
        assert_eq!(filters[1].field, Field::Project);
        assert_eq!(filters[1].value, "myapp");
    }

    #[test]
    fn test_field_filter_from_options_none() {
        let filters = FieldFilter::from_options(&None, &None);
        assert!(filters.is_empty());
    }

    #[test]
    fn test_field_filter_matches_all() {
        let mut fields = BTreeMap::new();
        fields.insert(Field::Agent, "claude".to_string());
        fields.insert(Field::Project, "myapp".to_string());

        let filters = FieldFilter::from_options(&Some("claude".to_string()), &None);
        assert!(FieldFilter::matches_all(&filters, &fields));

        let filters = FieldFilter::from_options(&Some("codex".to_string()), &None);
        assert!(!FieldFilter::matches_all(&filters, &fields));

        let filters =
            FieldFilter::from_options(&Some("claude".to_string()), &Some("myapp".to_string()));
        assert!(FieldFilter::matches_all(&filters, &fields));

        let filters =
            FieldFilter::from_options(&Some("claude".to_string()), &Some("other".to_string()));
        assert!(!FieldFilter::matches_all(&filters, &fields));
    }

    #[test]
    fn test_output_format_from_flags() {
        // Default depends on TTY/Pager status
        let default =
            if crate::pager::is_active() || std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                OutputFormat::Log
            } else {
                OutputFormat::Tsv
            };
        assert_eq!(
            OutputFormat::from_flags(false, false, false, false),
            default
        );
        assert_eq!(
            OutputFormat::from_flags(false, false, true, false),
            OutputFormat::Tsv
        );
        assert_eq!(
            OutputFormat::from_flags(true, false, false, false),
            OutputFormat::Json
        );
        assert_eq!(
            OutputFormat::from_flags(false, true, false, false),
            OutputFormat::Ltsv
        );
        assert_eq!(
            OutputFormat::from_flags(false, false, false, true),
            OutputFormat::Table
        );
    }

    #[test]
    fn test_sort_order_from_flag() {
        assert_eq!(SortOrder::from_flag(false), SortOrder::Desc);
        assert_eq!(SortOrder::from_flag(true), SortOrder::Asc);
    }

    #[test]
    fn test_parse_field_list_valid() {
        let fields: Vec<Field> = parse_field_list("agent,title,path").unwrap();
        assert_eq!(fields, vec![Field::Agent, Field::Title, Field::Path]);
    }

    #[test]
    fn test_parse_field_list_with_spaces() {
        let fields: Vec<Field> = parse_field_list("agent , title , path").unwrap();
        assert_eq!(fields, vec![Field::Agent, Field::Title, Field::Path]);
    }

    #[test]
    fn test_parse_field_list_empty_items() {
        let fields: Vec<Field> = parse_field_list("agent,,title,").unwrap();
        assert_eq!(fields, vec![Field::Agent, Field::Title]);
    }

    #[test]
    fn test_parse_field_list_project_fields() {
        let fields: Vec<ProjectField> = parse_field_list("project,agents,cwd").unwrap();
        assert_eq!(
            fields,
            vec![
                ProjectField::Project,
                ProjectField::Agents,
                ProjectField::Cwd
            ]
        );
    }
}

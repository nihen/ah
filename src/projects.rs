//! Project aggregation (shared by `ah project` and `ah fuzzy project`).

use std::collections::{BTreeMap, HashMap};
use std::time::SystemTime;

use rayon::prelude::*;

use crate::agents;
use crate::agents::common::{canonical_home, format_mtime};
use crate::cli;
use crate::cli::{
    Field, FieldFilter, FilterArgs, ListProjectsResolvedArgs, ProjectField, SortOrder,
};
use crate::collector;
use crate::config;
use crate::resolver;

/// Build sorted project records, or an error message if nothing is collected.
pub fn build_project_records(
    args: &ListProjectsResolvedArgs,
    filter: &FilterArgs,
) -> Result<Vec<BTreeMap<ProjectField, String>>, String> {
    let home = canonical_home();

    let files = collector::collect_files(filter.limit);
    if files.is_empty() {
        return Err("No session files found.".to_string());
    }

    let since = filter.since_time()?;
    let until = filter.until_time()?;
    let field_filters = FieldFilter::from_options(&filter.agent, &None);

    let needs_cwd = args.fields.contains(&ProjectField::Cwd) || filter.dir.is_some();
    let needs_project_raw = args.fields.contains(&ProjectField::ProjectRaw);
    let needs_sessions = args.fields.contains(&ProjectField::Sessions);
    let needs_created_at = args.fields.contains(&ProjectField::FirstCreatedAt)
        || args.sort_field == ProjectField::FirstCreatedAt;

    let session_fields: Vec<Field> = if needs_sessions {
        vec![
            Field::Agent,
            Field::Project,
            Field::ProjectRaw,
            Field::ModifiedAt,
            Field::CreatedAt,
            Field::Title,
            Field::FirstPrompt,
            Field::LastPrompt,
            Field::Path,
            Field::Cwd,
            Field::Id,
            Field::ResumeCmd,
            Field::Turns,
            Field::Size,
        ]
    } else {
        Vec::new()
    };

    let resolve_fields = if needs_sessions {
        session_fields.clone()
    } else {
        let mut fields = vec![Field::Project, Field::Agent];
        if needs_cwd {
            fields.push(Field::Cwd);
        }
        if needs_project_raw {
            fields.push(Field::ProjectRaw);
        }
        if needs_created_at {
            fields.push(Field::CreatedAt);
        }
        fields
    };

    struct ProjectInfo {
        agents: Vec<String>,
        project_raws: Vec<String>,
        cwds: Vec<String>,
        sessions: Vec<serde_json::Map<String, serde_json::Value>>,
        count: usize,
        latest: SystemTime,
        first_created: Option<String>,
    }

    let cwd_filter = filter.dir.as_ref().map(|d| cli::FilterArgs::resolve_dir(d));

    let lp_opts =
        resolver::ResolveOpts::default_with_title_limit(if needs_sessions { 50 } else { 0 });

    struct ResolvedEntry {
        agent_id: String,
        project: String,
        mtime: SystemTime,
        fields: BTreeMap<Field, String>,
    }

    let resolved: Vec<ResolvedEntry> = files
        .par_iter()
        .filter_map(|(path, mtime)| {
            if let Some(ref since) = since {
                if mtime < since {
                    return None;
                }
            }
            if let Some(ref until) = until {
                if mtime > until {
                    return None;
                }
            }

            let agent = config::find_agent_for_path(path);
            let plugin = agent
                .map(|a| a.plugin)
                .unwrap_or_else(agents::unknown_plugin);
            let fields =
                resolver::resolve_fields(path, plugin, *mtime, &home, &resolve_fields, &lp_opts);

            if !FieldFilter::matches_all(&field_filters, &fields) {
                return None;
            }

            if let Some(ref cwd_val) = cwd_filter {
                let session_cwd = fields.get(&Field::Cwd).map(|v| v.as_str()).unwrap_or("");
                if session_cwd != cwd_val {
                    return None;
                }
            }

            let project = fields.get(&Field::Project).cloned().unwrap_or_default();
            if project.is_empty() || project == "?" {
                return None;
            }

            Some(ResolvedEntry {
                agent_id: agent
                    .map(|a| a.id.clone())
                    .unwrap_or_else(|| plugin.id().to_string()),
                project,
                mtime: *mtime,
                fields,
            })
        })
        .collect();

    let mut projects: HashMap<String, ProjectInfo> = HashMap::new();

    for entry in &resolved {
        let info = projects
            .entry(entry.project.clone())
            .or_insert(ProjectInfo {
                agents: Vec::new(),
                project_raws: Vec::new(),
                cwds: Vec::new(),
                sessions: Vec::new(),
                count: 0,
                latest: SystemTime::UNIX_EPOCH,
                first_created: None,
            });
        info.agents.push(entry.agent_id.clone());
        info.count += 1;
        if entry.mtime > info.latest {
            info.latest = entry.mtime;
        }
        if needs_created_at {
            if let Some(created) = entry.fields.get(&Field::CreatedAt) {
                if !created.is_empty()
                    && info
                        .first_created
                        .as_ref()
                        .is_none_or(|c| created.as_str() < c.as_str())
                {
                    info.first_created = Some(created.clone());
                }
            }
        }
        if needs_project_raw {
            if let Some(raw) = entry.fields.get(&Field::ProjectRaw) {
                if !raw.is_empty() && !info.project_raws.contains(raw) {
                    info.project_raws.push(raw.clone());
                }
            }
        }
        if needs_cwd {
            if let Some(cwd) = entry.fields.get(&Field::Cwd) {
                if !cwd.is_empty() && !info.cwds.contains(cwd) {
                    info.cwds.push(cwd.clone());
                }
            }
        }
        if needs_sessions {
            let mut obj = serde_json::Map::new();
            for sf in &session_fields {
                if let Some(val) = entry.fields.get(sf) {
                    if !val.is_empty() {
                        obj.insert(
                            sf.name().to_string(),
                            serde_json::Value::String(val.clone()),
                        );
                    }
                }
            }
            info.sessions.push(obj);
        }
    }

    if let Some(ref proj) = filter.project {
        projects.retain(|k, _| k == proj);
    }

    if projects.is_empty() {
        return Err("No projects found.".to_string());
    }

    let sorted: Vec<(String, ProjectInfo)> = projects.into_iter().collect();

    // Build fields set including sort_field for sorting even if not in output
    let mut resolve_fields: Vec<ProjectField> = args.fields.clone();
    if !resolve_fields.contains(&args.sort_field) {
        resolve_fields.push(args.sort_field);
    }

    let mut records: Vec<BTreeMap<ProjectField, String>> = sorted
        .iter()
        .map(|(project, info)| {
            let mut record = BTreeMap::new();
            let mut agents = info.agents.clone();
            agents.sort();
            agents.dedup();

            for field in &resolve_fields {
                let val = match field {
                    ProjectField::Project => project.clone(),
                    ProjectField::ProjectRaw => info.project_raws.join(", "),
                    ProjectField::Cwd => info.cwds.first().cloned().unwrap_or_default(),
                    ProjectField::SessionCount => info.count.to_string(),
                    ProjectField::Sessions => {
                        serde_json::to_string(&info.sessions).unwrap_or_else(|_| "[]".to_string())
                    }
                    ProjectField::Agents => agents.join(", "),
                    ProjectField::LastModifiedAt => format_mtime(info.latest),
                    ProjectField::FirstCreatedAt => info.first_created.clone().unwrap_or_default(),
                };
                record.insert(*field, val);
            }
            record
        })
        .collect();

    let sf = args.sort_field;
    let numeric = sf.is_numeric();
    match args.sort_order {
        SortOrder::Desc => records
            .sort_by(|a, b| crate::output::compare_field_values(b.get(&sf), a.get(&sf), numeric)),
        SortOrder::Asc => records
            .sort_by(|a, b| crate::output::compare_field_values(a.get(&sf), b.get(&sf), numeric)),
    }

    // Remove sort_field from records if it's not in the requested output fields
    if !args.fields.contains(&sf) {
        for record in &mut records {
            record.remove(&sf);
        }
    }

    Ok(records)
}

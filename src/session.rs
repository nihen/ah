use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::cli::Field;

#[derive(Debug, Clone)]
pub struct Session {
    /// Local sessions: absolute filesystem path. Remote sessions (from
    /// `--remote`): the value is `"<remote_name>:<absolute_remote_path>"`,
    /// which is NOT a valid local path — never pass it to `metadata()` /
    /// `read_to_string()` / etc. Read `Field::Path` for display, and route
    /// through `remote::parse_remote_path` before any filesystem access.
    pub path: PathBuf,
    pub fields: BTreeMap<Field, String>,
}

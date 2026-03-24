use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::cli::Field;

#[derive(Debug, Clone)]
pub struct Session {
    pub path: PathBuf,
    pub fields: BTreeMap<Field, String>,
}

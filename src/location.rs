//! Versioned occurrence locations, validated against the selected source text.

#[derive(Debug, Clone)]
pub struct Location {
    pub record: usize,
    pub fragment: usize,
    pub start: usize,
    pub end: usize,
    digest: String,
}

impl Location {
    pub fn new(
        record: usize,
        fragment: usize,
        start: usize,
        end: usize,
        fingerprint: &str,
    ) -> Self {
        Self {
            record,
            fragment,
            start,
            end,
            digest: fingerprint.to_string(),
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        let fields: Vec<_> = value.split(':').collect();
        let invalid = || {
            "Invalid position; use the position from `ah search --json` or --verbose".to_string()
        };
        if fields.len() != 6
            || fields[0] != "v1"
            || fields[5].len() != 64
            || !fields[5].bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(invalid());
        }
        let numbers: Vec<usize> = fields[1..5]
            .iter()
            .map(|s| s.parse().map_err(|_| invalid()))
            .collect::<Result<_, _>>()?;
        if numbers[0] == 0 || numbers[1] == 0 || numbers[2] > numbers[3] {
            return Err(invalid());
        }
        Ok(Self {
            record: numbers[0],
            fragment: numbers[1],
            start: numbers[2],
            end: numbers[3],
            digest: fields[5].to_ascii_lowercase(),
        })
    }

    pub fn matches(&self, text: &str, fingerprint: &str) -> bool {
        text.get(self.start..self.end).is_some() && fingerprint == self.digest
    }
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "v1:{}:{}:{}:{}:{}",
            self.record, self.fragment, self.start, self.end, self.digest
        )
    }
}

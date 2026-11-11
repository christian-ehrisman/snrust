//! ServiceNow encoded query (`sysparm_query`) bidirectional parser and builder.
//!
//! Preserves both structured filter chips (`active=true`, `priority<=2`, `short_descriptionLIKEvpn`)
//! and raw complex segments (`^NQ`, `javascript:...`) losslessly so classic power users and
//! visual filter bar users can switch freely.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterOperator {
    Equals,
    NotEquals,
    Contains,
    DoesNotContain,
    StartsWith,
    EndsWith,
    LessOrEqual,
    GreaterOrEqual,
    LessThan,
    GreaterThan,
    IsEmpty,
    IsNotEmpty,
    In,
}

impl FilterOperator {
    pub fn sn_token(self) -> &'static str {
        match self {
            Self::Equals => "=",
            Self::NotEquals => "!=",
            Self::Contains => "LIKE",
            Self::DoesNotContain => "NOT LIKE",
            Self::StartsWith => "STARTSWITH",
            Self::EndsWith => "ENDSWITH",
            Self::LessOrEqual => "<=",
            Self::GreaterOrEqual => ">=",
            Self::LessThan => "<",
            Self::GreaterThan => ">",
            Self::IsEmpty => "ISEMPTY",
            Self::IsNotEmpty => "ISNOTEMPTY",
            Self::In => "IN",
        }
    }

    pub fn symbol_label(self) -> &'static str {
        match self {
            Self::Equals => "=",
            Self::NotEquals => "≠",
            Self::Contains => "contains",
            Self::DoesNotContain => "!contains",
            Self::StartsWith => "starts with",
            Self::EndsWith => "ends with",
            Self::LessOrEqual => "≤",
            Self::GreaterOrEqual => "≥",
            Self::LessThan => "<",
            Self::GreaterThan => ">",
            Self::IsEmpty => "is empty",
            Self::IsNotEmpty => "is not empty",
            Self::In => "in",
        }
    }

    pub fn takes_value(self) -> bool {
        !matches!(self, Self::IsEmpty | Self::IsNotEmpty)
    }

    pub fn all() -> &'static [Self] {
        &[
            Self::Equals,
            Self::NotEquals,
            Self::Contains,
            Self::DoesNotContain,
            Self::StartsWith,
            Self::EndsWith,
            Self::LessOrEqual,
            Self::GreaterOrEqual,
            Self::LessThan,
            Self::GreaterThan,
            Self::IsEmpty,
            Self::IsNotEmpty,
            Self::In,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterClause {
    pub field: String,
    pub operator: FilterOperator,
    pub value: String,
    pub display_hint: Option<String>,
}

impl FilterClause {
    pub fn new(
        field: impl Into<String>,
        operator: FilterOperator,
        value: impl Into<String>,
    ) -> Self {
        Self {
            field: field.into(),
            operator,
            value: value.into(),
            display_hint: None,
        }
    }

    pub fn to_sn_segment(&self) -> String {
        if self.operator.takes_value() {
            format!("{}{}{}", self.field, self.operator.sn_token(), self.value)
        } else {
            format!("{}{}", self.field, self.operator.sn_token())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortSpec {
    pub field: String,
    pub descending: bool,
}

impl SortSpec {
    pub fn asc(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            descending: false,
        }
    }

    pub fn desc(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            descending: true,
        }
    }

    pub fn to_sn_segment(&self) -> String {
        if self.descending {
            format!("ORDERBYDESC{}", self.field)
        } else {
            format!("ORDERBY{}", self.field)
        }
    }
}

/// Structured representation of a ServiceNow `sysparm_query`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodedQuery {
    pub clauses: Vec<FilterClause>,
    pub raw_passthrough: Vec<String>,
    pub sorts: Vec<SortSpec>,
}

impl EncodedQuery {
    pub fn parse(raw_query: &str) -> Self {
        let trimmed = raw_query.trim();
        if trimmed.is_empty() {
            return Self::default();
        }

        // If the query contains OR branches (`^OR`) or New Query (`^NQ`), keep complex segments
        // intact while extracting trailing ORDERBY clauses cleanly.
        let mut clauses = Vec::new();
        let mut raw_passthrough = Vec::new();
        let mut sorts = Vec::new();

        for part in split_caret_segments(trimmed) {
            if part.is_empty() || part == "EQ" {
                continue;
            }
            if let Some(field) = part.strip_prefix("ORDERBYDESC")
                && !field.is_empty()
            {
                sorts.push(SortSpec::desc(field));
                continue;
            }
            if let Some(field) = part.strip_prefix("ORDERBY")
                && !field.is_empty()
            {
                sorts.push(SortSpec::asc(field));
                continue;
            }

            if part.starts_with("OR") || part.starts_with("NQ") || part.contains("javascript:") {
                raw_passthrough.push(part);
                continue;
            }

            if let Some(parsed) = parse_single_clause(&part) {
                clauses.push(parsed);
            } else {
                raw_passthrough.push(part);
            }
        }

        Self {
            clauses,
            raw_passthrough,
            sorts,
        }
    }

    pub fn to_encoded_string(&self) -> String {
        let mut segments: Vec<String> = Vec::new();
        for c in &self.clauses {
            segments.push(c.to_sn_segment());
        }
        for raw in &self.raw_passthrough {
            segments.push(raw.clone());
        }
        for s in &self.sorts {
            segments.push(s.to_sn_segment());
        }
        segments.join("^")
    }

    /// Encoded filter string *excluding* `ORDERBY` clauses (useful for `stats` / `COUNT` APIs).
    pub fn filter_only_string(&self) -> String {
        let mut segments: Vec<String> = Vec::new();
        for c in &self.clauses {
            segments.push(c.to_sn_segment());
        }
        for raw in &self.raw_passthrough {
            segments.push(raw.clone());
        }
        segments.join("^")
    }

    /// Toggles sort order for `field`: None -> ASC -> DESC -> ASC.
    pub fn toggle_sort(&mut self, field: &str) {
        let next_desc = match self.sorts.first() {
            Some(existing) if existing.field == field => !existing.descending,
            _ => false,
        };
        self.sorts.clear();
        self.sorts.push(SortSpec {
            field: field.to_string(),
            descending: next_desc,
        });
    }

    /// Appends a delta high-water mark (`sys_updated_on>{watermark}`) to an existing filter.
    pub fn with_updated_after(&self, watermark: &str) -> String {
        let base = self.filter_only_string();
        let delta = format!("sys_updated_on>{watermark}^ORDERBYsys_updated_on");
        if base.is_empty() {
            delta
        } else {
            format!("{base}^{delta}")
        }
    }
}

fn split_caret_segments(raw: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut chars = raw.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '^' {
            if chars.peek() == Some(&'^') {
                // Escaped literal caret `^^` inside ServiceNow string value
                current.push('^');
                chars.next();
            } else {
                result.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

fn parse_single_clause(segment: &str) -> Option<FilterClause> {
    // Ordered longest/most-specific operator token first so `!=` and `<=` match before `=` or `<`.
    const OPS: &[(&str, FilterOperator)] = &[
        ("ISNOTEMPTY", FilterOperator::IsNotEmpty),
        ("ISEMPTY", FilterOperator::IsEmpty),
        ("NOT LIKE", FilterOperator::DoesNotContain),
        ("STARTSWITH", FilterOperator::StartsWith),
        ("ENDSWITH", FilterOperator::EndsWith),
        ("LIKE", FilterOperator::Contains),
        ("!=", FilterOperator::NotEquals),
        ("<=", FilterOperator::LessOrEqual),
        (">=", FilterOperator::GreaterOrEqual),
        ("IN", FilterOperator::In),
        ("=", FilterOperator::Equals),
        ("<", FilterOperator::LessThan),
        (">", FilterOperator::GreaterThan),
    ];

    for &(token, op) in OPS {
        if matches!(op, FilterOperator::IsEmpty | FilterOperator::IsNotEmpty) {
            if let Some(field) = segment.strip_suffix(token)
                && is_valid_field_identifier(field)
            {
                return Some(FilterClause::new(field, op, ""));
            }
            continue;
        }

        // Word-based operators (`LIKE`, `IN`, etc.) must follow a valid identifier boundary.
        let mut search_from = 0;
        while let Some(rel_idx) = segment[search_from..].find(token) {
            let idx = search_from + rel_idx;
            let field = &segment[..idx];
            let val = &segment[idx + token.len()..];
            if is_valid_field_identifier(field) {
                // Guard against matching `IN` inside a field name like `u_REGION=us` (lowercase field names in SN,
                // or ensure `IN` isn't a prefix of another uppercase operator).
                let valid_boundary = if token.chars().all(|c| c.is_ascii_uppercase() || c == ' ') {
                    field.chars().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.'
                    })
                } else {
                    true
                };
                if valid_boundary {
                    return Some(FilterClause::new(field, op, val));
                }
            }
            search_from = idx + 1;
            if search_from >= segment.len() {
                break;
            }
        }
    }
    None
}

fn is_valid_field_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

//! ServiceNow encoded query (`sysparm_query`) model, parser and serializer.
//!
//! A query is kept as the same tree the classic condition builder edits:
//!
//! ```text
//! EncodedQuery
//!   groups        joined by ^NQ   ("new query": the result matches any group)
//!     rows        joined by ^     (every row must match)
//!       alternatives joined by ^OR (any alternative may match)
//! ```
//!
//! Anything the parser does not understand (`RLQUERY`, `GROUPBY`, a future operator) is kept
//! verbatim as a [`Condition::Raw`] in its original position, so parsing and re-serializing
//! never reorders a query or changes what it means.

use serde::{Deserialize, Serialize};

use crate::schema::FieldType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    NotIn,
    Between,
    On,
    NotOn,
    Anything,
    EmptyString,
    SameAs,
    NotSameAs,
    Dynamic,
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
            Self::NotIn => "NOT IN",
            Self::Between => "BETWEEN",
            Self::On => "ON",
            Self::NotOn => "NOTON",
            Self::Anything => "ANYTHING",
            Self::EmptyString => "EMPTYSTRING",
            Self::SameAs => "SAMEAS",
            Self::NotSameAs => "NSAMEAS",
            Self::Dynamic => "DYNAMIC",
        }
    }

    /// Compact form for filter chips.
    pub fn symbol_label(self) -> &'static str {
        match self {
            Self::Equals => "=",
            Self::NotEquals => "≠",
            Self::Contains => "contains",
            Self::DoesNotContain => "does not contain",
            Self::StartsWith => "starts with",
            Self::EndsWith => "ends with",
            Self::LessOrEqual => "≤",
            Self::GreaterOrEqual => "≥",
            Self::LessThan => "<",
            Self::GreaterThan => ">",
            Self::IsEmpty => "is empty",
            Self::IsNotEmpty => "is not empty",
            Self::In => "is one of",
            Self::NotIn => "is not one of",
            Self::Between => "between",
            Self::On => "on",
            Self::NotOn => "not on",
            Self::Anything => "is anything",
            Self::EmptyString => "is empty string",
            Self::SameAs => "is same as",
            Self::NotSameAs => "is different from",
            Self::Dynamic => "is (dynamic)",
        }
    }

    /// Wording used by the condition builder, matching the classic UI's operator names.
    pub fn label(self) -> &'static str {
        match self {
            Self::Equals => "is",
            Self::NotEquals => "is not",
            Self::LessOrEqual => "less than or is",
            Self::GreaterOrEqual => "greater than or is",
            Self::LessThan => "less than",
            Self::GreaterThan => "greater than",
            other => other.symbol_label(),
        }
    }

    pub fn takes_value(self) -> bool {
        !matches!(
            self,
            Self::IsEmpty | Self::IsNotEmpty | Self::Anything | Self::EmptyString
        )
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
            Self::NotIn,
            Self::Between,
            Self::On,
            Self::NotOn,
            Self::Anything,
            Self::EmptyString,
            Self::SameAs,
            Self::NotSameAs,
            Self::Dynamic,
        ]
    }

    /// The operators the classic condition builder offers for a field of this type.
    pub fn for_field_type(field_type: &FieldType) -> &'static [Self] {
        use FilterOperator::*;
        match field_type {
            FieldType::Boolean => &[Equals, NotEquals, IsEmpty, IsNotEmpty, Anything],
            FieldType::Choice | FieldType::SysClassName => {
                &[Equals, NotEquals, In, NotIn, IsEmpty, IsNotEmpty, Anything]
            }
            FieldType::Reference { .. } => &[
                Equals,
                NotEquals,
                IsEmpty,
                IsNotEmpty,
                Contains,
                StartsWith,
                EndsWith,
                DoesNotContain,
                Dynamic,
                Anything,
            ],
            FieldType::Integer | FieldType::Decimal | FieldType::Currency => &[
                Equals,
                NotEquals,
                LessThan,
                LessOrEqual,
                GreaterThan,
                GreaterOrEqual,
                Between,
                IsEmpty,
                IsNotEmpty,
                Anything,
            ],
            FieldType::Date | FieldType::DateTime => &[
                On,
                NotOn,
                LessThan,
                LessOrEqual,
                GreaterThan,
                GreaterOrEqual,
                Between,
                IsEmpty,
                IsNotEmpty,
                Anything,
            ],
            _ => &[
                Equals,
                NotEquals,
                Contains,
                DoesNotContain,
                StartsWith,
                EndsWith,
                In,
                NotIn,
                IsEmpty,
                IsNotEmpty,
                EmptyString,
                Anything,
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterClause {
    pub field: String,
    pub operator: FilterOperator,
    pub value: String,
    /// Human-readable form of `value` (a reference's display value, a choice label), when
    /// known. Not part of the encoded query.
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

/// One condition: understood well enough to edit, or carried through untouched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Condition {
    Clause(FilterClause),
    /// A segment the parser does not model, without its `^`, `^OR` or `^NQ` joiner.
    Raw(String),
}

impl Condition {
    pub fn to_sn_segment(&self) -> String {
        match self {
            Self::Clause(clause) => clause.to_sn_segment(),
            Self::Raw(raw) => raw.clone(),
        }
    }

    pub fn as_clause(&self) -> Option<&FilterClause> {
        match self {
            Self::Clause(clause) => Some(clause),
            Self::Raw(_) => None,
        }
    }
}

/// Alternatives joined by `^OR`. Never empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionRow {
    pub alternatives: Vec<Condition>,
}

impl ConditionRow {
    pub fn single(condition: Condition) -> Self {
        Self {
            alternatives: vec![condition],
        }
    }

    /// The clause, when this row is one plain condition with no `^OR` alternatives.
    pub fn as_single_clause(&self) -> Option<&FilterClause> {
        match self.alternatives.as_slice() {
            [Condition::Clause(clause)] => Some(clause),
            _ => None,
        }
    }
}

/// Rows joined by `^`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionGroup {
    pub rows: Vec<ConditionRow>,
}

/// Structured representation of a ServiceNow `sysparm_query`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodedQuery {
    /// Joined by `^NQ`. A query with no conditions has no groups.
    pub groups: Vec<ConditionGroup>,
    pub sorts: Vec<SortSpec>,
}

/// A literal `^` inside a value is written `^^`.
fn escape_carets(segment: &str) -> String {
    segment.replace('^', "^^")
}

impl EncodedQuery {
    pub fn parse(raw_query: &str) -> Self {
        let mut query = Self::default();
        let mut group = ConditionGroup::default();
        for part in split_caret_segments(raw_query.trim()) {
            if part.is_empty() || part == "EQ" {
                continue;
            }
            if let Some(field) = part.strip_prefix("ORDERBYDESC")
                && is_valid_field_identifier(field)
            {
                query.sorts.push(SortSpec::desc(field));
                continue;
            }
            if let Some(field) = part.strip_prefix("ORDERBY")
                && is_valid_field_identifier(field)
            {
                query.sorts.push(SortSpec::asc(field));
                continue;
            }
            if let Some(rest) = part.strip_prefix("NQ") {
                query.groups.push(std::mem::take(&mut group));
                if !rest.is_empty() {
                    group.rows.push(ConditionRow::single(parse_condition(rest)));
                }
                continue;
            }
            // `^OR` attaches to the condition before it. With nothing before it in this
            // group it is kept as a row of its own so the text still round-trips.
            if let Some(rest) = part.strip_prefix("OR")
                && !rest.is_empty()
                && let Some(row) = group.rows.last_mut()
            {
                row.alternatives.push(parse_condition(rest));
                continue;
            }
            group
                .rows
                .push(ConditionRow::single(parse_condition(&part)));
        }
        query.groups.push(group);
        // A lone empty group is "no filter"; empty groups beside others are real `^NQ`s.
        if query.groups.len() == 1 && query.groups[0].rows.is_empty() {
            query.groups.clear();
        }
        query
    }

    fn filter_segments(&self) -> Vec<String> {
        let mut segments = Vec::new();
        for (group_ix, group) in self.groups.iter().enumerate() {
            let mut first_in_group = true;
            if group.rows.is_empty() && group_ix > 0 {
                segments.push("NQ".to_string());
            }
            for row in &group.rows {
                for (alt_ix, condition) in row.alternatives.iter().enumerate() {
                    let text = escape_carets(&condition.to_sn_segment());
                    let joiner = if alt_ix > 0 {
                        "OR"
                    } else if first_in_group && group_ix > 0 {
                        "NQ"
                    } else {
                        ""
                    };
                    segments.push(format!("{joiner}{text}"));
                    first_in_group = false;
                }
            }
        }
        segments
    }

    pub fn to_encoded_string(&self) -> String {
        let mut segments = self.filter_segments();
        segments.extend(self.sorts.iter().map(SortSpec::to_sn_segment));
        segments.join("^")
    }

    /// Encoded filter string *excluding* `ORDERBY` clauses (useful for `stats` / `COUNT` APIs).
    pub fn filter_only_string(&self) -> String {
        self.filter_segments().join("^")
    }

    pub fn is_empty(&self) -> bool {
        self.groups.iter().all(|group| group.rows.is_empty())
    }

    /// Requires `clause` in addition to the existing filter. With `^NQ` branches that means
    /// adding it to every branch: `(A ^NQ B) AND C` is `A^C ^NQ B^C`.
    pub fn and_clause(&mut self, clause: FilterClause) {
        if self.groups.is_empty() {
            self.groups.push(ConditionGroup::default());
        }
        for group in &mut self.groups {
            group
                .rows
                .push(ConditionRow::single(Condition::Clause(clause.clone())));
        }
    }

    /// Drops plain single-clause rows matching `predicate` from every branch.
    pub fn remove_clauses(&mut self, predicate: impl Fn(&FilterClause) -> bool) {
        for group in &mut self.groups {
            group
                .rows
                .retain(|row| !row.as_single_clause().is_some_and(&predicate));
        }
        self.normalize();
    }

    /// Removes rows left without alternatives and groups left without rows.
    pub fn normalize(&mut self) {
        for group in &mut self.groups {
            group.rows.retain(|row| !row.alternatives.is_empty());
        }
        self.groups.retain(|group| !group.rows.is_empty());
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

    /// The filter restricted to records updated after `watermark`, oldest first. The
    /// restriction is added to every `^NQ` branch; appending it once would bind it to the
    /// last branch only and return unchanged records from the others.
    pub fn with_updated_after(&self, watermark: &str) -> String {
        let mut delta = Self {
            groups: self.groups.clone(),
            sorts: vec![SortSpec::asc("sys_updated_on")],
        };
        delta.and_clause(FilterClause::new(
            "sys_updated_on",
            FilterOperator::GreaterThan,
            watermark,
        ));
        delta.to_encoded_string()
    }
}

/// How a condition joins the one before it, in the flat row-per-condition form the
/// condition builder edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Joiner {
    /// `^`: also required.
    And,
    /// `^OR`: an alternative to the condition before it.
    Or,
    /// `^NQ`: starts a new set of conditions; records matching any set are returned.
    NewQuery,
}

impl EncodedQuery {
    /// The conditions in order, each with how it joins its predecessor. The first
    /// condition's joiner is [`Joiner::And`].
    pub fn flatten(&self) -> Vec<(Joiner, Condition)> {
        let mut flat = Vec::new();
        for (group_ix, group) in self.groups.iter().enumerate() {
            let mut first_in_group = true;
            for row in &group.rows {
                for (alt_ix, condition) in row.alternatives.iter().enumerate() {
                    let joiner = if alt_ix > 0 {
                        Joiner::Or
                    } else if first_in_group && group_ix > 0 {
                        Joiner::NewQuery
                    } else {
                        Joiner::And
                    };
                    flat.push((joiner, condition.clone()));
                    first_in_group = false;
                }
            }
        }
        flat
    }

    /// Rebuilds the tree from flat conditions. An `Or` with nothing before it in its group
    /// becomes an ordinary condition.
    pub fn from_flat(
        flat: impl IntoIterator<Item = (Joiner, Condition)>,
        sorts: Vec<SortSpec>,
    ) -> Self {
        let mut groups = vec![ConditionGroup::default()];
        for (joiner, condition) in flat {
            if joiner == Joiner::NewQuery {
                groups.push(ConditionGroup::default());
            }
            let group = groups.last_mut().expect("at least one group");
            match (joiner, group.rows.last_mut()) {
                (Joiner::Or, Some(row)) => row.alternatives.push(condition),
                _ => group.rows.push(ConditionRow::single(condition)),
            }
        }
        let mut query = Self { groups, sorts };
        query.normalize();
        query
    }

    /// Keeps the first `count` conditions (in [`Self::flatten`] order) and drops the rest,
    /// as clicking a breadcrumb does in the classic list header.
    pub fn truncated(&self, count: usize) -> Self {
        Self::from_flat(self.flatten().into_iter().take(count), self.sorts.clone())
    }
}

/// A saved list filter (`sys_filter`), shown as a pill above the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedFilter {
    pub id: String,
    pub title: String,
    pub table: String,
    /// Encoded query, possibly including `ORDERBY`.
    pub filter: String,
    /// Whether the signed-in user may rename, update or delete it. Global filters are
    /// read-only here.
    pub editable: bool,
}

fn parse_condition(segment: &str) -> Condition {
    match parse_single_clause(segment) {
        Some(clause) => Condition::Clause(clause),
        None => Condition::Raw(segment.to_string()),
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
        ("EMPTYSTRING", FilterOperator::EmptyString),
        ("ANYTHING", FilterOperator::Anything),
        ("NOT LIKE", FilterOperator::DoesNotContain),
        ("NOT IN", FilterOperator::NotIn),
        ("STARTSWITH", FilterOperator::StartsWith),
        ("ENDSWITH", FilterOperator::EndsWith),
        ("NSAMEAS", FilterOperator::NotSameAs),
        ("SAMEAS", FilterOperator::SameAs),
        ("BETWEEN", FilterOperator::Between),
        ("DYNAMIC", FilterOperator::Dynamic),
        ("NOTON", FilterOperator::NotOn),
        ("LIKE", FilterOperator::Contains),
        ("!=", FilterOperator::NotEquals),
        ("<=", FilterOperator::LessOrEqual),
        (">=", FilterOperator::GreaterOrEqual),
        ("IN", FilterOperator::In),
        ("ON", FilterOperator::On),
        ("=", FilterOperator::Equals),
        ("<", FilterOperator::LessThan),
        (">", FilterOperator::GreaterThan),
    ];

    for &(token, op) in OPS {
        if !op.takes_value() {
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

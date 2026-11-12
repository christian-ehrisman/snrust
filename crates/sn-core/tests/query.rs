use sn_core::{Condition, EncodedQuery, FieldType, FilterClause, FilterOperator, Joiner};

/// Real-world shapes that must survive parse → serialize byte for byte.
const ROUND_TRIPS: &[&str] = &[
    "",
    "active=true",
    "active=true^priority<=2^short_descriptionLIKEvpn^ORDERBYDESCnumber",
    "active=true^priority=1^ORpriority=2",
    "active=true^priority=1^ORpriority=2^ORpriority=3^state!=7",
    "active=true^NQassigned_toISEMPTY^priority=1",
    "a=1^ORb=2^NQc=3^ORd=4^NQe=5^ORDERBYa^ORDERBYDESCb",
    "assigned_to=javascript:gs.getUserID()^active=true",
    "sys_created_onONToday@javascript:gs.beginningOfToday()@javascript:gs.endOfToday()",
    "sys_updated_onBETWEENjavascript:gs.daysAgoStart(7)@javascript:gs.daysAgoEnd(0)",
    "stateIN1,2,3^categoryNOT INnetwork,hardware",
    "caller_id.emailENDSWITH@example.com^caller_id.managerISNOTEMPTY",
    "assigned_toDYNAMIC90d1921e5f510100a9ad2572f2b477fe",
    "descriptionANYTHING^close_notesEMPTYSTRING",
    "short_descriptionLIKEa^^b",
    "RLQUERYtask_sla.task,>=1^has_breached=true^ENDRLQUERY^active=true",
    "active=true^GROUPBYassignment_group",
    "opened_bySAMEAScaller_id^ORassigned_toNSAMEAScaller_id",
];

#[test]
fn queries_round_trip_without_reordering() {
    for raw in ROUND_TRIPS {
        assert_eq!(EncodedQuery::parse(raw).to_encoded_string(), *raw);
    }
}

#[test]
fn or_and_new_query_structure_is_modelled() {
    let query =
        EncodedQuery::parse("active=true^priority=1^ORpriority=2^NQstateIN6,7^ORDERBYnumber");
    assert_eq!(query.groups.len(), 2);
    let first = &query.groups[0];
    assert_eq!(first.rows.len(), 2);
    assert_eq!(first.rows[0].as_single_clause().unwrap().field, "active");
    assert_eq!(first.rows[1].alternatives.len(), 2);
    assert!(first.rows[1].as_single_clause().is_none());
    let alternative = first.rows[1].alternatives[1].as_clause().unwrap();
    assert_eq!(
        (
            alternative.field.as_str(),
            alternative.operator,
            alternative.value.as_str()
        ),
        ("priority", FilterOperator::Equals, "2")
    );
    assert_eq!(
        query.groups[1].rows[0].as_single_clause().unwrap().operator,
        FilterOperator::In
    );
    assert_eq!(query.sorts.len(), 1);
    assert_eq!(
        query.filter_only_string(),
        "active=true^priority=1^ORpriority=2^NQstateIN6,7"
    );
}

#[test]
fn unknown_segments_stay_raw_in_place_and_values_keep_operator_words() {
    let query = EncodedQuery::parse("active=true^RLQUERYtask_sla.task,>=1^ENDRLQUERY^priority=1");
    let rows = &query.groups[0].rows;
    assert!(
        matches!(&rows[1].alternatives[0], Condition::Raw(raw) if raw == "RLQUERYtask_sla.task,>=1")
    );
    assert!(matches!(&rows[2].alternatives[0], Condition::Raw(raw) if raw == "ENDRLQUERY"));
    assert_eq!(rows[3].as_single_clause().unwrap().field, "priority");

    // Operator words inside a value do not split it.
    for (raw, operator, value) in [
        ("name=JOHNSON", FilterOperator::Equals, "JOHNSON"),
        (
            "short_descriptionLIKEON CALL IN PROGRESS",
            FilterOperator::Contains,
            "ON CALL IN PROGRESS",
        ),
        ("u_note=a!=b", FilterOperator::Equals, "a!=b"),
        (
            "sys_created_on>=2026-01-01 00:00:00",
            FilterOperator::GreaterOrEqual,
            "2026-01-01 00:00:00",
        ),
    ] {
        let query = EncodedQuery::parse(raw);
        let clause = query.groups[0].rows[0].as_single_clause().unwrap();
        assert_eq!(
            (clause.operator, clause.value.as_str()),
            (operator, value),
            "{raw}"
        );
    }
    // An escaped caret is one literal caret in the value.
    let query = EncodedQuery::parse("short_descriptionLIKEa^^b");
    assert_eq!(
        query.groups[0].rows[0].as_single_clause().unwrap().value,
        "a^b"
    );
}

#[test]
fn extra_conditions_and_delta_watermarks_apply_to_every_branch() {
    let mut query = EncodedQuery::parse("priority=1^NQpriority=2^ORDERBYDESCnumber");
    query.and_clause(FilterClause::new(
        "short_description",
        FilterOperator::Contains,
        "vpn",
    ));
    assert_eq!(
        query.to_encoded_string(),
        "priority=1^short_descriptionLIKEvpn^NQpriority=2^short_descriptionLIKEvpn^ORDERBYDESCnumber"
    );
    // Rows with OR alternatives are not touched by clause removal.
    let mut query =
        EncodedQuery::parse("short_descriptionLIKEvpn^short_descriptionLIKEa^ORnameLIKEb");
    query.remove_clauses(|clause| clause.field == "short_description");
    assert_eq!(
        query.to_encoded_string(),
        "short_descriptionLIKEa^ORnameLIKEb"
    );
    query.remove_clauses(|_| true);
    assert_eq!(
        query.to_encoded_string(),
        "short_descriptionLIKEa^ORnameLIKEb"
    );

    assert_eq!(
        EncodedQuery::parse("active=true^NQpriority=1^ORDERBYDESCnumber")
            .with_updated_after("2026-11-11 10:00:00"),
        "active=true^sys_updated_on>2026-11-11 10:00:00^NQpriority=1^sys_updated_on>2026-11-11 10:00:00^ORDERBYsys_updated_on"
    );
    assert_eq!(
        EncodedQuery::parse("").with_updated_after("2026-11-11 10:00:00"),
        "sys_updated_on>2026-11-11 10:00:00^ORDERBYsys_updated_on"
    );
    assert!(EncodedQuery::parse("ORDERBYnumber").is_empty());
}

#[test]
fn operators_offered_depend_on_the_field_type() {
    let offered = |field_type: FieldType| FilterOperator::for_field_type(&field_type).to_vec();
    assert!(offered(FieldType::String).contains(&FilterOperator::Contains));
    assert!(!offered(FieldType::Boolean).contains(&FilterOperator::Contains));
    assert!(offered(FieldType::DateTime).contains(&FilterOperator::On));
    assert!(!offered(FieldType::DateTime).contains(&FilterOperator::StartsWith));
    assert!(offered(FieldType::Choice).contains(&FilterOperator::In));
    assert!(offered(FieldType::Integer).contains(&FilterOperator::Between));
    // Every operator serializes to a token the parser reads back as the same operator.
    for operator in FilterOperator::all() {
        let clause = FilterClause::new(
            "field",
            *operator,
            if operator.takes_value() { "v" } else { "" },
        );
        let parsed = EncodedQuery::parse(&clause.to_sn_segment());
        assert_eq!(
            parsed.groups[0].rows[0].as_single_clause(),
            Some(&clause),
            "{operator:?}"
        );
    }
}

#[test]
fn flat_form_round_trips_and_supports_builder_edits() {
    for raw in ROUND_TRIPS {
        let query = EncodedQuery::parse(raw);
        assert_eq!(
            EncodedQuery::from_flat(query.flatten(), query.sorts.clone()).to_encoded_string(),
            *raw
        );
    }
    let query = EncodedQuery::parse("active=true^priority=1^ORpriority=2^NQstate=6^ORDERBYnumber");
    let joiners: Vec<Joiner> = query
        .flatten()
        .into_iter()
        .map(|(joiner, _)| joiner)
        .collect();
    assert_eq!(
        joiners,
        [Joiner::And, Joiner::And, Joiner::Or, Joiner::NewQuery]
    );

    // Breadcrumb truncation keeps the leading conditions and the sort.
    assert_eq!(
        query.truncated(2).to_encoded_string(),
        "active=true^priority=1^ORDERBYnumber"
    );
    assert_eq!(query.truncated(0).to_encoded_string(), "ORDERBYnumber");

    // Deleting the condition an OR hung from leaves a valid query.
    let mut flat = query.flatten();
    flat.remove(1);
    assert_eq!(
        EncodedQuery::from_flat(flat, Vec::new()).to_encoded_string(),
        "active=true^ORpriority=2^NQstate=6"
    );
    let orphan = vec![(
        Joiner::Or,
        Condition::Clause(FilterClause::new("a", FilterOperator::Equals, "1")),
    )];
    assert_eq!(
        EncodedQuery::from_flat(orphan, Vec::new()).to_encoded_string(),
        "a=1"
    );
}

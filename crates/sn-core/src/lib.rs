//! Core domain models, schema & inheritance resolution, encoded query parser,
//! rearrangeable 2-column form auto-grid layout, and dirty-safe live form session state.

pub mod error;
pub mod form_session;
pub mod instance;
pub mod layout;
pub mod model;
pub mod query;
pub mod schema;

pub use error::{SnError, SnResult};
pub use form_session::{ApplyDeltaOutcome, FieldConflict, FormSessionState, LiveFieldPulse};
pub use instance::{InstanceId, InstanceProfile};
pub use layout::{ColSpan, FormGridLayout, GridItem, GridVisualRow};
pub use model::{FieldValue, JournalEntry, ListPage, ReferenceCandidate, SnRecord};
pub use query::{EncodedQuery, FilterClause, FilterOperator, SortSpec};
pub use schema::{
    ChoiceOption, DictionaryEntry, FieldType, TableDescriptor, TableHierarchy, TableSchema,
};

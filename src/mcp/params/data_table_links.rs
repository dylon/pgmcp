//! Parameter structs for the data-table ⇄ experiment/work-item link tools
//! (ADR-023, v44).

use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DataTableLinkParams {
    /// Table name (or use table_id).
    #[serde(default)]
    pub table: Option<String>,
    /// Numeric table id (or use table).
    #[serde(default)]
    pub table_id: Option<i64>,
    /// Project scope for table-name resolution.
    #[serde(default)]
    pub project: Option<String>,
    /// What the table is linked to: experiment | work_item.
    pub target_type: String,
    /// Numeric id of the experiment / work-item.
    pub target_id: i64,
    /// Optional role of the table for this target (e.g. "measurements", "benchmark").
    #[serde(default)]
    pub role: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DataTableUnlinkParams {
    /// Table name (or use table_id).
    #[serde(default)]
    pub table: Option<String>,
    /// Numeric table id (or use table).
    #[serde(default)]
    pub table_id: Option<i64>,
    /// Project scope for table-name resolution.
    #[serde(default)]
    pub project: Option<String>,
    /// What the table is linked to: experiment | work_item.
    pub target_type: String,
    /// Numeric id of the experiment / work-item.
    pub target_id: i64,
}

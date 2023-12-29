//! `SeaORM` Entity. Generated by sea-orm-codegen 0.12.10

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "chunked_content")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub chunk_id: String,
    pub content_id: String,
    #[sea_orm(column_type = "Text")]
    pub text: String,
    pub index_name: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

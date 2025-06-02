// SPDX-License-Identifier: Apache-2.0

pub use sea_orm_migration::prelude::*;

mod m20250601_000001_create_messages_table;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m20250601_000001_create_messages_table::Migration)]
    }
}

// SPDX-License-Identifier: Apache-2.0

use lupo_migrator::*;
use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use std::fs::{create_dir_all, exists};
use std::path::{Path, PathBuf};
use tracing::*;

fn build_sqlite_url(dir_path: impl AsRef<Path>) -> String {
    let path: PathBuf = [dir_path.as_ref(), "db.sqlite".as_ref()].iter().collect();
    let mut url = url::Url::parse("sqlite://").unwrap();
    url.set_path(&path.to_string_lossy());
    url.set_query(Some("mode=rwc"));
    url.into()
}

pub async fn setup_db(
    data_dir: impl AsRef<Path>,
) -> Result<DatabaseConnection, crate::error::Error> {
    if !exists(&data_dir)? {
        debug!("db dir not found, creating it...");
        create_dir_all(&data_dir)?;
    }
    let connection_string = build_sqlite_url(data_dir);
    debug!("connect do database: {}", connection_string);
    let opt = ConnectOptions::new(connection_string);
    let db = Database::connect(opt).await?;

    // check if connection is OK
    debug!("check connection");
    db.ping().await?;

    debug!("apply migrations");
    Migrator::up(&db, None).await?;

    debug!("check migrations");
    let schema_manager = SchemaManager::new(&db);
    let has_messages = schema_manager.has_table("messages").await?;
    debug!("has messages: {}", has_messages);

    Ok(db)
}

// SPDX-License-Identifier: Apache-2.0

use lupo_migrator::*;
use sea_orm::{ConnectOptions, Database};
use std::fs::{create_dir_all, exists};
use std::path::{Path, PathBuf};
use tauri::Manager;
use tracing::*;

fn build_sqlite_url<P: AsRef<Path>>(dir_path: P) -> Result<String, Box<dyn std::error::Error>> {
    let path: PathBuf = [dir_path.as_ref(), "db.sqlite".as_ref()].iter().collect();
    let mut url = tauri::Url::parse("sqlite://")?;
    url.set_path(&path.to_string_lossy());
    url.set_query(Some("mode=rwc"));
    Ok(url.into())
}

#[instrument(skip(app))]
pub async fn setup_db(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = app
        .path()
        .app_local_data_dir()
        .expect("couldn't find data dir");
    if !exists(&data_dir)? {
        debug!("db dir not found, creating it...");
        create_dir_all(&data_dir)?;
    }
    let connection_string = build_sqlite_url(data_dir)?;
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

    app.manage(db);
    debug!("db managed");
    Ok(())
}

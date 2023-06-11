use actix_files::NamedFile;
use anyhow::Context;
use core::fmt;
use serde;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

use actix_web::{
    dev::{fn_service, ServiceRequest, ServiceResponse},
    web, App, Error, HttpServer, ResponseError,
};
use rusqlite::{self, types::FromSql, OpenFlags, Params};

struct WrappedError<T>(T, actix_web::http::StatusCode);

impl<T> WrappedError<T> {
    fn user(err: T) -> Self {
        Self(err, actix_web::http::StatusCode::BAD_REQUEST)
    }

    fn internal(err: T) -> Self {
        Self(err, actix_web::http::StatusCode::INTERNAL_SERVER_ERROR)
    }
}

impl<T: fmt::Display> fmt::Display for WrappedError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: fmt::Debug> fmt::Debug for WrappedError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: fmt::Display + fmt::Debug> ResponseError for WrappedError<T> {
    fn status_code(&self) -> actix_web::http::StatusCode {
        self.1
    }
}

struct DbValue(serde_json::Value);

impl FromSql for DbValue {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        use rusqlite::types::ValueRef;
        Ok(Self(match value {
            ValueRef::Blob(bs) => serde_json::Value::String({
                let mut buf = String::new();
                for b in bs {
                    use std::fmt::Write;
                    write!(buf, "{:02x}", b).expect("Hex formatting cannot fail");
                }
                buf
            }),
            ValueRef::Real(f) => serde_json::Number::from_f64(f)
                .map(serde_json::Value::Number)
                .unwrap_or_else(|| serde_json::Value::String(format!("{}", f))),
            ValueRef::Integer(i) => serde_json::Value::Number(serde_json::Number::from(i)),
            ValueRef::Text(s) => serde_json::Value::String(String::from_utf8_lossy(s).to_string()),
            ValueRef::Null => serde_json::Value::Null,
        }))
    }
}

async fn perform_query<P: Params>(
    fetcher: Arc<Mutex<DataFetcher>>,
    sql: String,
    params: P,
) -> Result<web::Json<serde_json::Value>, Error> {
    fetcher
        .lock()
        .await
        .refresh()
        .await
        .map_err(WrappedError::internal)?;

    let conn = rusqlite::Connection::open_with_flags("db.sqlite", OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(WrappedError::internal)?;
    let mut stmt = conn.prepare(&sql).map_err(WrappedError::user)?;
    let cols = stmt
        .column_names()
        .iter()
        .map(|it| it.to_string())
        .collect::<Vec<_>>();
    let ret = stmt
        .query_map(params, |row| {
            let mut row_json = serde_json::Map::new();
            for (i, name) in (0..cols.len()).zip(cols.iter()) {
                let elt: DbValue = row.get(i)?;
                row_json.extend([(name.to_string(), elt.0)]);
            }
            Ok(serde_json::Value::Object(row_json))
        })
        .map_err(WrappedError::user)?
        .collect::<Result<_, _>>()
        .map_err(WrappedError::user)?;

    Ok(web::Json(serde_json::Value::Array(ret)))
}

struct DataFetcher {
    conn: rusqlite::Connection,
    client: reqwest::Client,
    req: reqwest::Request,
    last_ts: Instant,
}

const TIME_TO_REFRESH: Duration = Duration::from_secs(300);

impl DataFetcher {
    async fn refresh(&mut self) -> anyhow::Result<()> {
        let delta = Instant::now() - self.last_ts;
        if delta > TIME_TO_REFRESH {
            self.fetch_and_load_data().await?;
        }
        Ok(())
    }

    async fn fetch_and_load_data(&mut self) -> anyhow::Result<()> {
        #[derive(serde::Deserialize)]
        struct Entry {
            id: u64,
            #[serde(alias = "gameVersionTypeID")]
            game_version_type_id: u64,
            name: String,
            slug: String,
        }

        println!("Fetching API data");
        let result = self
            .client
            .execute(self.req.try_clone().expect("Clonable request"))
            .await?;
        self.last_ts = Instant::now();
        let json = result.json::<Vec<Entry>>().await?;

        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM versions", ())?;
        let mut stmt = tx.prepare_cached("INSERT INTO versions VALUES (?, ?, ?, ?)")?;
        for entry in json {
            stmt.execute((entry.id, entry.game_version_type_id, entry.name, entry.slug))?;
        }
        std::mem::drop(stmt);
        tx.commit()?;
        println!("Inserted into database");

        Ok(())
    }
}

async fn update_db() -> anyhow::Result<DataFetcher> {
    let conn = rusqlite::Connection::open_with_flags(
        "db.sqlite",
        OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS versions (\
         id INT PRIMARY KEY,\
         gameVersionTypeID INT,\
         name TEXT,\
         slug TEXT\
         )",
        (),
    )?;

    let api_token = std::env::var("API_TOKEN").context("API_TOKEN not set")?;

    let client = reqwest::Client::builder().build()?;
    let req = client
        .get("https://minecraft.curseforge.com/api/game/versions")
        .header("X-Api-Token", api_token)
        .build()?;

    let mut fetcher = DataFetcher {
        conn,
        client,
        req,
        last_ts: Instant::now(),
    };
    fetcher.fetch_and_load_data().await?;

    Ok(fetcher)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = "127.0.0.1";
    let port = std::env::var("PORT")
        .map(|s| s.parse::<u16>())
        .ok()
        .unwrap_or(Ok(8080))?;
    println!("Running on: http://{}:{}", addr, port);

    let fetcher = Arc::new(Mutex::new(update_db().await?));
    HttpServer::new(move || {
        let fetcher = fetcher.clone();
        App::new()
            .route(
                "/query",
                web::post().to(move |sql| query(fetcher.clone(), sql)),
            )
            .service(web::redirect("/", "index.html"))
            .service(
                actix_files::Files::new("/", "./static")
                    .use_last_modified(true)
                    .default_handler(fn_service(|req: ServiceRequest| async {
                        let (req, _) = req.into_parts();
                        let file = NamedFile::open_async("./static/404.html").await?;
                        let res = file.into_response(&req);
                        Ok(ServiceResponse::new(req, res))
                    })),
            )
    })
    .bind((addr, port))?
    .run()
    .await?;

    Ok(())
}

async fn query(
    fetcher: Arc<Mutex<DataFetcher>>,
    sql: String,
) -> Result<web::Json<serde_json::Value>, Error> {
    perform_query(fetcher, sql, []).await
}

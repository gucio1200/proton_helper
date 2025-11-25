use actix_web::{web, App, HttpServer, HttpResponse, Responder};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::collections::HashMap;

const DEFAULT_KODI_URL: &str = "http://192.168.0.5/jsonrpc";

#[derive(Serialize, Deserialize, Debug)]
struct OutputMovie {
    index: usize,
    title: String,
    year: String,
    movieid: Option<i64>,
}

async fn fetch_movies_from_kodi(movie_name: &str) -> Vec<OutputMovie> {
    let kodi_url = env::var("KODI_URL").unwrap_or_else(|_| DEFAULT_KODI_URL.to_string());

    let client = Client::new();

    let payload = json!({
        "jsonrpc": "2.0",
        "method": "VideoLibrary.GetMovies",
        "params": { "properties": ["title", "year"] },
        "id": 1
    });

    let resp = match client.post(&kodi_url).json(&payload).send().await {
        Ok(r) => r,
        Err(e) => {
            println!("[ERROR] Failed HTTP request to Kodi: {}", e);
            return vec![];
        }
    };

    let json_resp: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            println!("[ERROR] Failed to parse Kodi JSON response: {}", e);
            return vec![];
        }
    };

    let movies = json_resp
        .get("result")
        .and_then(|r| r.get("movies"))
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();

    let mut results = Vec::new();
    let mut index = 1usize;

    for movie in movies {
        let title = movie.get("title").and_then(|v| v.as_str()).unwrap_or("");

        if title.to_lowercase().contains(&movie_name.to_lowercase()) {
            let year = movie
                .get("year")
                .map(|v| v.to_string())
                .unwrap_or("Unknown".to_string());

            let movieid = movie.get("movieid").and_then(|v| v.as_i64());

            results.push(OutputMovie {
                index,
                title: title.to_string(),
                year,
                movieid,
            });

            index += 1;
        }
    }

    results
}

async fn movies_endpoint(query: web::Query<HashMap<String, String>>) -> impl Responder {
    let movie_name = query.get("name").cloned().unwrap_or_default();

    println!("[REQUEST] /movies?name={}", movie_name);

    let movies = fetch_movies_from_kodi(&movie_name).await;

    // Build number_to_id map and display options
    let mut number_to_id = serde_json::Map::new();
    let mut display_options = Vec::new();

    for movie in &movies {
        if let Some(mid) = movie.movieid {
            number_to_id.insert(movie.index.to_string(), json!(mid));
            display_options.push(format!("{}. {} ({})", movie.index, movie.title, movie.year));
        }
    }

    let out = json!({
        "state": "OK",
        "movies": movies,
        "number_to_id": number_to_id,
        "display_options": display_options
    });

    // Log response
    println!("[RESPONSE] {}", serde_json::to_string_pretty(&out).unwrap());

    HttpResponse::Ok().json(out)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port = env::var("PORT").unwrap_or_else(|_| "8080".to_string());

    println!("Kodi server running on http://0.0.0.0:{port}");

    HttpServer::new(|| {
        App::new()
            .route("/movies", web::get().to(movies_endpoint))
    })
    .bind(format!("0.0.0.0:{port}"))?
    .run()
    .await
}

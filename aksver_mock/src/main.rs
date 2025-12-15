use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use serde::Serialize;

// 1. Define the "Release" struct (Inner object)
#[derive(Serialize)]
struct Release {
    version: String,
}

// 2. Define the "Response" struct (Outer object)
// This matches exactly what Renovate looks for: { "releases": [...] }
#[derive(Serialize)]
struct VersionResponse {
    releases: Vec<Release>,
}

// 3. Updated Handler: Uses Path parameter instead of Query
#[get("/{location}")]
async fn get_versions(location: web::Path<String>) -> impl Responder {
    let location_name = location.into_inner();
    println!("Request for location: {}", location_name);

    // Mock logic: return slightly different lists based on location if needed
    let raw_versions = match location_name.as_str() {
        "westus" => vec!["1.27.3", "1.27.7", "1.28.3"],
        _ => vec!["1.26.6", "1.26.10", "1.27.3", "1.27.7", "1.28.3", "1.28.5"],
    };

    // Convert string list to the object list Renovate expects
    let releases_vec: Vec<Release> = raw_versions
        .into_iter()
        .map(|v| Release { version: v.to_string() })
        .collect();

    let response_obj = VersionResponse {
        releases: releases_vec,
    };

    HttpResponse::Ok().json(response_obj)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    println!("Server starting at http://0.0.0.0:8080");

    HttpServer::new(|| {
        App::new()
            .service(get_versions)
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}

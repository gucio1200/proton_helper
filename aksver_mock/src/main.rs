use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use serde::Serialize;

// 1. Define the Release struct (isStable removed)
#[derive(Serialize)]
struct Release {
    version: String,
    #[serde(rename = "changelogUrl")]
    changelog_url: String,
    #[serde(rename = "sourceUrl")]
    source_url: String,
}

// 2. Define the Top-Level Response
#[derive(Serialize)]
struct VersionResponse {
    releases: Vec<Release>,
    #[serde(rename = "sourceUrl")]
    source_url: String,
    homepage: String,
}

// 3. Helper to format k8s changelog URLs
fn generate_changelog_url(version: &str) -> String {
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() < 3 {
        return "".to_string();
    }
    
    let major = parts[0];
    let minor = parts[1];
    let patch = parts[2];

    format!(
        "https://github.com/kubernetes/kubernetes/blob/master/CHANGELOG/CHANGELOG-{}.{}.md#v{}{}{}",
        major, minor, major, minor, patch
    )
}

#[get("/{location}")]
async fn get_versions(path: web::Path<String>) -> impl Responder {
    let location = path.into_inner();
    println!("Request for location: {}", location);

    // Mock logic: different lists for different locations?
    // For now, returning the standard list
    let raw_versions = vec![
        "1.26.6", 
        "1.26.10", 
        "1.27.3", 
        "1.27.7", 
        "1.28.3", 
        "1.28.5"
    ];

    let releases_vec: Vec<Release> = raw_versions
        .into_iter()
        .map(|v| Release {
            version: v.to_string(),
            changelog_url: generate_changelog_url(v),
            source_url: "https://github.com/kubernetes/kubernetes".to_string(),
        })
        .collect();

    let response_obj = VersionResponse {
        releases: releases_vec,
        source_url: "https://github.com/kubernetes/kubernetes".to_string(),
        homepage: "https://kubernetes.io".to_string(),
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

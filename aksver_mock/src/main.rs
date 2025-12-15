use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use serde::{Deserialize, Serialize};

// 1. Define the structure for the JSON response
#[derive(Serialize)]
struct VersionResponse {
    versions: Vec<String>,
}

// 2. Define the structure for the Query parameters (e.g., ?location=eastus)
#[derive(Deserialize)]
struct VersionQuery {
    location: String,
}

// 3. Define the handler function
#[get("/versions")]
async fn get_versions(query: web::Query<VersionQuery>) -> impl Responder {
    // We can access query.location here if we need logic based on "eastus"
    println!("Received request for location: {}", query.location);

    let response_obj = VersionResponse {
        versions: vec![
            "1.26.6".to_string(),
            "1.26.10".to_string(),
            "1.27.3".to_string(),
            "1.27.7".to_string(),
            "1.28.3".to_string(),
            "1.28.5".to_string(),
        ],
    };

    // Return the struct as JSON with HTTP 200 OK
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

use bli_rs::create_app;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting Bli-RS desktop runner...");
    let app = create_app()?;
    app.run()?;
    Ok(())
}

// crates to use -- use "crate"::"item"

use actix_multipart::Multipart;
use actix_web::{App, HttpResponse, HttpServer, Result, error, middleware, web}; // Added middleware
use bytes::BytesMut;
use futures_util::StreamExt;
use image::GenericImageView;
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageOutputFormat};
use std::collections::HashMap;
use std::io::{Cursor, Write};
use tera::{Context, Tera};

// --- No vtracer needed if using potrace for SVG ---
use std::process::Command;
use tempfile::NamedTempFile;
// --- End Added ---

/// Render the index page with an optional flash messages vector.
async fn index(tmpl: web::Data<Tera>) -> Result<HttpResponse> {
    let mut ctx = Context::new();
    ctx.insert("messages", &Vec::<String>::new());
    let rendered = tmpl.render("index.html", &ctx).map_err(|e| {
        log::error!("Template rendering error: {}", e);
        error::ErrorInternalServerError("Failed to render page.")
    })?;
    Ok(HttpResponse::Ok().content_type("text/html").body(rendered))
}

/// Handles the POSTed multipart/form-data for image cropping, resizing, and vectorization.
async fn resize(mut payload: Multipart, tmpl: web::Data<Tera>) -> Result<HttpResponse> {
    // --- Initialization ---
    let mut messages: Vec<String> = Vec::new();
    let mut form_fields: HashMap<String, String> = HashMap::new();
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut original_filename: Option<String> = None;

    // --- Multipart Stream Processing ---
    while let Some(item) = payload.next().await {
        let mut field = item.map_err(|e| {
            log::error!("Multipart payload error: {}", e);
            error::ErrorInternalServerError("Error processing uploaded data.")
        })?;
        let field_name = field.name().to_string();

        if field_name == "file" {
            let cd = field.content_disposition();
            if let Some(fname) = cd.get_filename() {
                original_filename = Some(fname.to_owned());
            }
            let mut bytes = BytesMut::new();
            while let Some(chunk) = field.next().await {
                let data = chunk.map_err(|e| {
                    log::error!("Error reading file chunk: {}", e);
                    error::ErrorInternalServerError("Error reading uploaded file.")
                })?;
                bytes.extend_from_slice(&data);
            }
            if !bytes.is_empty() {
                file_bytes = Some(bytes.to_vec());
            }
        } else {
            let mut text_bytes = BytesMut::new();
            while let Some(chunk) = field.next().await {
                let data = chunk.map_err(|e| {
                    log::error!("Error reading form field chunk: {}", e);
                    error::ErrorInternalServerError("Error reading form data.")
                })?;
                text_bytes.extend_from_slice(&data);
            }
            match String::from_utf8(text_bytes.to_vec()) {
                Ok(text) => {
                    form_fields.insert(field_name, text);
                }
                Err(e) => {
                    log::warn!("Invalid UTF-8 in form field '{}': {}", field_name, e);
                    form_fields.insert(field_name, String::new());
                }
            }
        }
    }

    // --- Input Validation 1: File Presence ---
    if file_bytes.is_none() {
        messages.push("No file uploaded or file was empty.".to_string());
        return render_template_error(&tmpl, messages, actix_web::http::StatusCode::BAD_REQUEST);
    }
    let file_bytes = file_bytes.unwrap();

    // --- Input Parsing & Validation 2: Form Fields ---
    let target_width: u32 = form_fields
        .get("width")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let target_height: u32 = form_fields
        .get("height")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let crop_x: u32 = form_fields
        .get("cropX")
        .and_then(|s| s.parse::<f64>().ok())
        .map(|f| f.floor() as u32)
        .unwrap_or(0);
    let crop_y: u32 = form_fields
        .get("cropY")
        .and_then(|s| s.parse::<f64>().ok())
        .map(|f| f.floor() as u32)
        .unwrap_or(0);
    let crop_width: u32 = form_fields
        .get("cropWidth")
        .and_then(|s| s.parse::<f64>().ok())
        .map(|f| f.floor() as u32)
        .unwrap_or(0);
    let crop_height: u32 = form_fields
        .get("cropHeight")
        .and_then(|s| s.parse::<f64>().ok())
        .map(|f| f.floor() as u32)
        .unwrap_or(0);

    // --- Output Format Selection and Validation ---
    let mut output_format = "PNG".to_string();
    if let Some(fmt) = form_fields.get("outputFormat") {
        output_format = fmt.to_uppercase();
    }

    let allowed_formats = vec!["JPEG", "PNG", "WEBP", "SVG", "EPS"];
    if !allowed_formats.contains(&output_format.as_str()) {
        messages.push(format!(
            "Invalid output format selected: '{}'. Allowed: {}",
            output_format,
            allowed_formats.join(", ")
        ));
        return render_template_error(&tmpl, messages, actix_web::http::StatusCode::BAD_REQUEST);
    }

    // --- Dimension Validation ---
    if target_width == 0 || target_height == 0 {
        messages.push("Target dimensions (width and height) must be positive.".to_string());
        return render_template_error(&tmpl, messages, actix_web::http::StatusCode::BAD_REQUEST);
    }

    // --- Image Loading ---
    let img_result = image::load_from_memory(&file_bytes);
    let img: DynamicImage = match img_result {
        Ok(img) => img,
        Err(e) => {
            log::error!("Image loading error: {}", e);
            messages.push(format!(
                "Error loading image: {}. Please upload a valid image file (e.g., PNG, JPEG, GIF).",
                e
            ));
            return render_template_error(
                &tmpl,
                messages,
                actix_web::http::StatusCode::BAD_REQUEST,
            );
        }
    };

    // --- Crop Dimension Validation (against loaded image) ---
    let (img_width, img_height) = img.dimensions();
    if crop_width == 0 || crop_height == 0 {
        messages.push("Crop dimensions (cropWidth and cropHeight) must be positive.".to_string());
        return render_template_error(&tmpl, messages, actix_web::http::StatusCode::BAD_REQUEST);
    }
    if crop_x >= img_width
        || crop_y >= img_height
        || crop_x + crop_width > img_width
        || crop_y + crop_height > img_height
    {
        messages.push(format!(
            "Crop area (x:{}+{}, y:{}+{}) exceeds image boundaries ({}x{}).",
            crop_x, crop_width, crop_y, crop_height, img_width, img_height
        ));
        return render_template_error(&tmpl, messages, actix_web::http::StatusCode::BAD_REQUEST);
    }

    // --- Image Processing: Crop and Resize (Raster Operations) ---
    let cropped_img = img.crop_imm(crop_x, crop_y, crop_width, crop_height);
    let resized_img = cropped_img.resize_exact(target_width, target_height, FilterType::Lanczos3);

    // --- Output Generation: Raster or Vector ---
    let mut buffer: Vec<u8>;
    let content_type: &str;
    let final_extension = output_format.to_lowercase();

    match output_format.as_str() {
        // --- Raster Formats ---
        "JPEG" => {
            content_type = "image/jpeg";
            buffer = Vec::new();
            let jpeg_img = DynamicImage::ImageRgb8(resized_img.to_rgb8());
            let format = ImageOutputFormat::Jpeg(95);
            if let Err(e) = jpeg_img.write_to(&mut Cursor::new(&mut buffer), format) {
                log::error!("Error saving JPEG: {}", e);
                messages.push(format!("Error encoding image to JPEG: {}", e));
                return render_template_error(
                    &tmpl,
                    messages,
                    actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                );
            }
        }
        "PNG" => {
            content_type = "image/png";
            buffer = Vec::new();
            let format = ImageOutputFormat::Png;
            if let Err(e) = resized_img.write_to(&mut Cursor::new(&mut buffer), format) {
                log::error!("Error saving PNG: {}", e);
                messages.push(format!("Error encoding image to PNG: {}", e));
                return render_template_error(
                    &tmpl,
                    messages,
                    actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                );
            }
        }
        "WEBP" => {
            content_type = "image/webp";
            buffer = Vec::new();
            let format = ImageOutputFormat::WebP;
            if let Err(e) = resized_img.write_to(&mut Cursor::new(&mut buffer), format) {
                log::error!("Error saving WebP: {}", e);
                messages.push(format!("Error encoding image to WebP: {}", e));
                return render_template_error(
                    &tmpl,
                    messages,
                    actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                );
            }
        }

        // --- Vector Formats (Using potrace for both) ---
        "SVG" | "EPS" => {
            // Combine SVG and EPS logic using potrace

            // 1. Create temporary file for potrace input (PNM format recommended)
            let mut temp_file = match NamedTempFile::new() {
                Ok(f) => f,
                Err(e) => {
                    log::error!("Failed to create temp file for potrace: {}", e);
                    messages.push(
                        "Server error: Could not create temporary file for processing.".to_string(),
                    );
                    return render_template_error(
                        &tmpl,
                        messages,
                        actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                    );
                }
            };

            // 2. Save resized raster image to temp file (convert to RGB8 for PNM)
            let pnm_img = DynamicImage::ImageRgb8(resized_img.to_rgb8());
            if let Err(e) = pnm_img.save_with_format(temp_file.path(), ImageFormat::Bmp) {
                log::error!("Failed to save temporary PNM file for potrace: {}", e);
                messages.push(
                    "Server error: Could not save intermediate file for processing.".to_string(),
                );
                return render_template_error(
                    &tmpl,
                    messages,
                    actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                );
            }
            if let Err(e) = temp_file.flush() {
                // Ensure data is written
                log::error!("Failed to flush temp file buffer: {}", e);
                messages.push("Server error: Could not finalize intermediate file.".to_string());
                return render_template_error(
                    &tmpl,
                    messages,
                    actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                );
            }

            // 3. Determine potrace arguments and content type based on format
            let potrace_arg: &str;
            if output_format == "SVG" {
                content_type = "image/svg+xml";
                potrace_arg = "-s"; // SVG output flag
            } else {
                // EPS
                content_type = "application/postscript";
                potrace_arg = "-e"; // EPS output flag
            }

            // 4. Prepare and execute potrace command
            log::info!(
                "Running potrace ({}) on temp file: {:?}",
                potrace_arg,
                temp_file.path()
            );
            let potrace_output = Command::new("potrace")
                .arg(temp_file.path()) // Input file path
                .arg(potrace_arg) // Specify SVG (-s) or EPS (-e) output
                .arg("-o") // Specify output destination...
                .arg("-") // ...which is stdout
                .output(); // Execute and capture output

            // 5. Process potrace result
            match potrace_output {
                Ok(output) => {
                    if output.status.success() {
                        buffer = output.stdout; // Get vector data from stdout
                        log::info!("potrace ({}) executed successfully.", potrace_arg);
                    } else {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        log::error!(
                            "potrace ({}) execution failed. Status: {}. Stderr: {}",
                            potrace_arg,
                            output.status,
                            stderr
                        );
                        messages.push(format!(
                            "Error converting image to {} via potrace: {}",
                            output_format,
                            stderr.lines().next().unwrap_or("Unknown potrace error")
                        ));
                        if stderr.contains("No such file or directory")
                            || stderr.contains("not found")
                        {
                            messages.push("Server configuration error: 'potrace' command not found. Please ensure it is installed and in the system PATH.".to_string());
                        }
                        return render_template_error(
                            &tmpl,
                            messages,
                            actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                        );
                    }
                }
                Err(e) => {
                    log::error!("Failed to execute potrace command: {}", e);
                    messages.push(format!("Server configuration error: Failed to run 'potrace' command ({}). Is it installed and in the system PATH?", e));
                    return render_template_error(
                        &tmpl,
                        messages,
                        actix_web::http::StatusCode::INTERNAL_SERVER_ERROR,
                    );
                }
            }
            // Temp file automatically deleted when `temp_file` goes out of scope
        }
        _ => unreachable!("Invalid output format encountered after validation"),
    }

    // --- Prepare Download Response ---
    let orig_name = original_filename.unwrap_or_else(|| "image".to_string());
    let base_name = orig_name
        .rsplit('.')
        .nth(1)
        .unwrap_or(&orig_name)
        .to_owned();
    let download_filename = format!(
        "{}_processed_{}x{}.{}",
        base_name, target_width, target_height, final_extension
    );

    // --- Build and Return HTTP Response ---
    Ok(HttpResponse::Ok()
        .append_header(("Content-Type", content_type))
        .append_header((
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", download_filename),
        ))
        .body(buffer))
}

/// Helper function to render the index template with error messages and a specific status code.
fn render_template_error(
    tmpl: &web::Data<Tera>,
    messages: Vec<String>,
    status_code: actix_web::http::StatusCode,
) -> Result<HttpResponse> {
    let mut ctx = Context::new();
    ctx.insert("messages", &messages);
    log::warn!("Rendering error page with messages: {:?}", messages);

    let rendered = tmpl.render("index.html", &ctx).map_err(|e| {
        log::error!("Critical: Error rendering the error template itself: {}", e);
        error::ErrorInternalServerError("Failed to render page. Please try again.")
    })?;

    Ok(HttpResponse::build(status_code)
        .content_type("text/html; charset=utf-8")
        .body(rendered))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

    let tera = match Tera::new("templates/**/*") {
        Ok(t) => t,
        Err(e) => {
            log::error!("Failed to initialize Tera templates: {}", e);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Template initialization failed",
            ));
        }
    };
    log::info!("Tera templates initialized successfully.");

    let bind_address = "127.0.0.1:8080";
    log::info!("Starting server on http://{}", bind_address);

    HttpServer::new(move || {
        App::new()
            .wrap(middleware::Logger::default()) // Use middleware::Logger
            .app_data(web::Data::new(tera.clone()))
            .route("/", web::get().to(index))
            .route("/resize", web::post().to(resize))
    })
    .bind(bind_address)?
    .run()
    .await
}

use actix_multipart::Multipart;
use actix_web::{App, HttpResponse, HttpServer, Result, web};
use bytes::BytesMut;
use futures_util::StreamExt;
use image::GenericImageView;
use image::imageops::FilterType;
use image::{DynamicImage, ImageOutputFormat};
use std::collections::HashMap;
use std::io::Cursor;
use tera::{Context, Tera};

/// Render the index page with an optional flash messages vector.
async fn index(tmpl: web::Data<Tera>) -> Result<HttpResponse> {
    let mut ctx = Context::new();
    // Pass an empty vector of messages
    ctx.insert("messages", &Vec::<String>::new());
    let rendered = tmpl
        .render("index.html", &ctx)
        .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
    Ok(HttpResponse::Ok().content_type("text/html").body(rendered))
}

/// Handles the POSTed multipart/form-data for image cropping and resizing.
async fn resize(mut payload: Multipart, tmpl: web::Data<Tera>) -> Result<HttpResponse> {
    // To collect error messages (flash messages)
    let mut messages: Vec<String> = Vec::new();
    // A simple map to hold text fields from the form.
    let mut form_fields: HashMap<String, String> = HashMap::new();
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut original_filename: Option<String> = None;

    // Process each multipart field.
    while let Some(item) = payload.next().await {
        let mut field = item?;
        let field_name = field.name().to_string();
        if field_name == "file" {
            // Get original filename if provided.
            // Note: In the current actix-multipart version, content_disposition() returns
            // a reference to the ContentDisposition.
            let cd = field.content_disposition();
            if let Some(fname) = cd.get_filename() {
                original_filename = Some(fname.to_owned());
            }
            let mut bytes = BytesMut::new();
            while let Some(chunk) = field.next().await {
                let data = chunk?;
                bytes.extend_from_slice(&data);
            }
            file_bytes = Some(bytes.to_vec());
        } else {
            // For non-file fields, read as UTF-8 text.
            let mut text_bytes = BytesMut::new();
            while let Some(chunk) = field.next().await {
                let data = chunk?;
                text_bytes.extend_from_slice(&data);
            }
            let text = String::from_utf8(text_bytes.to_vec()).unwrap_or_default();
            form_fields.insert(field_name, text);
        }
    }

    if file_bytes.is_none() {
        messages.push("No file uploaded.".to_string());
        return render_template_error(&tmpl, messages);
    }

    // Parse expected fields.
    let target_width: u32 = form_fields
        .get("width")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let target_height: u32 = form_fields
        .get("height")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);

    // For crop values, parse as f64 then floor to convert to u32.
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

    let mut output_format = "PNG".to_string();
    if let Some(fmt) = form_fields.get("outputFormat") {
        output_format = fmt.to_uppercase();
    }

    let allowed_formats = vec!["JPEG", "PNG", "WEBP"];
    if !allowed_formats.contains(&output_format.as_str()) {
        messages.push(format!(
            "Invalid output format selected. Allowed: {}",
            allowed_formats.join(", ")
        ));
        return render_template_error(&tmpl, messages);
    }

    if target_width == 0 || target_height == 0 {
        messages.push("Target dimensions must be positive.".to_string());
        return render_template_error(&tmpl, messages);
    }
    if crop_width == 0 || crop_height == 0 {
        messages.push("Crop dimensions must be positive.".to_string());
        return render_template_error(&tmpl, messages);
    }

    // Load the image from the bytes.
    let img = image::load_from_memory(&file_bytes.unwrap());
    if img.is_err() {
        messages.push("Error processing image: invalid image data.".to_string());
        return render_template_error(&tmpl, messages);
    }
    let mut img: DynamicImage = img.unwrap();

    // For JPEG output, convert to RGB (remove transparency).
    if output_format == "JPEG" {
        img = DynamicImage::ImageRgb8(img.to_rgb8());
    }

    // Ensure that the crop area is within image boundaries.
    let (img_width, img_height) = img.dimensions();
    if crop_x + crop_width > img_width || crop_y + crop_height > img_height {
        messages.push("Crop area exceeds image boundaries.".to_string());
        return render_template_error(&tmpl, messages);
    }

    // Crop the image.
    let cropped_img = img.crop_imm(crop_x, crop_y, crop_width, crop_height);
    // Resize the cropped image using a Lanczos3 filter.
    let resized_img = cropped_img.resize_exact(target_width, target_height, FilterType::Lanczos3);

    // Write the resulting image to an in-memory buffer.
    let mut buffer = Vec::new();
    let format = match output_format.as_str() {
        "JPEG" => ImageOutputFormat::Jpeg(95),
        "WEBP" => ImageOutputFormat::WebP, // Note: WEBP is now a unit variant.
        _ => ImageOutputFormat::Png,
    };
    if let Err(e) = resized_img.write_to(&mut Cursor::new(&mut buffer), format) {
        messages.push(format!("Error saving image: {}", e));
        return render_template_error(&tmpl, messages);
    }

    // Construct the download filename.
    let orig_name = original_filename.unwrap_or_else(|| "image".to_string());
    // Try to extract the base name without extension.
    let base_name = orig_name
        .rsplit('.')
        .nth(1)
        .unwrap_or(&orig_name)
        .to_owned();
    let download_filename = format!(
        "{}_resized_{}x{}.{}",
        base_name,
        target_width,
        target_height,
        output_format.to_lowercase()
    );

    let content_type = match output_format.as_str() {
        "JPEG" => "image/jpeg",
        "WEBP" => "image/webp",
        _ => "image/png",
    };

    Ok(HttpResponse::Ok()
        .append_header(("Content-Type", content_type))
        .append_header((
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", download_filename),
        ))
        .body(buffer))
}

/// Helper function to render the index template along with flash messages.
fn render_template_error(tmpl: &web::Data<Tera>, messages: Vec<String>) -> Result<HttpResponse> {
    let mut ctx = Context::new();
    ctx.insert("messages", &messages);
    let rendered = tmpl
        .render("index.html", &ctx)
        .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
    Ok(HttpResponse::Ok().content_type("text/html").body(rendered))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Initialize Tera templates (all files in the templates/ directory).
    let tera = Tera::new("templates/**/*").expect("Error initializing Tera templates");

    // Start the HTTP server.
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(tera.clone()))
            .route("/", web::get().to(index))
            .route("/resize", web::post().to(resize))
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}

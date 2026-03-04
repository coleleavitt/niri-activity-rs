use std::fs;
use std::path::Path;

use chrono::Local;
use lettre::message::{Attachment, MultiPart, SinglePart, header::ContentType};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};

use crate::config::{Config, Email};
use crate::error::Error;
use crate::report::{self, App, TimeRange};

pub fn send_report(app: &App, range: TimeRange, period_name: &str) -> Result<(), Error> {
    let email_config = &app.config.email;

    if !email_config.enabled {
        return Err(Error::NiriError("Email is not enabled in config".into()));
    }

    if email_config.smtp_host.is_empty() {
        return Err(Error::NiriError("SMTP host not configured".into()));
    }

    if email_config.to_addresses.is_empty() {
        return Err(Error::NiriError("No recipient addresses configured".into()));
    }

    let bounds = range.resolve()?;
    let filename = format!(
        "activity_report_{}_{}.xlsx",
        bounds.start_date.format("%Y%m%d"),
        bounds.end_date.format("%Y%m%d")
    );
    let temp_file = tempfile::Builder::new()
        .prefix("niri_activity_")
        .suffix(".xlsx")
        .tempfile()
        .map_err(|e| Error::NiriError(format!("Failed to create temp file: {}", e)))?;
    let temp_path = temp_file.path().to_path_buf();

    report::export_xlsx_range(app, range, &temp_path.to_string_lossy())?;
    let xlsx_bytes = fs::read(&temp_path)
        .map_err(|e| Error::NiriError(format!("Failed to read generated XLSX: {}", e)))?;
    drop(temp_file); // Explicitly remove the temp file

    // Sanitize subject line: strip newlines to prevent header injection
    let raw_subject = if email_config.report_name.is_empty() {
        format!(
            "{} {}: {} to {}",
            email_config.subject_prefix, period_name, bounds.start_date, bounds.end_date
        )
    } else {
        format!(
            "{} {}: {} ({} to {})",
            email_config.subject_prefix,
            email_config.report_name,
            period_name,
            bounds.start_date,
            bounds.end_date
        )
    };
    let subject = raw_subject.replace(['\n', '\r'], " ");

    let html_body = build_html_summary(&bounds, period_name);
    let text_body = build_text_summary(&bounds, period_name);

    let attachment = Attachment::new(filename.clone()).body(
        xlsx_bytes,
        ContentType::parse("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
            .map_err(|e| Error::NiriError(format!("Invalid content type: {}", e)))?,
    );

    let multipart = MultiPart::mixed()
        .multipart(
            MultiPart::alternative()
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_PLAIN)
                        .body(text_body),
                )
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_HTML)
                        .body(html_body),
                ),
        )
        .singlepart(attachment);

    let mailer = build_mailer(email_config)?;

    let mut builder = Message::builder()
        .from(
            email_config
                .from_address
                .parse()
                .map_err(|e| Error::NiriError(format!("Invalid from address: {}", e)))?,
        )
        .subject(&subject);

    for recipient in &email_config.to_addresses {
        builder = builder.to(recipient
            .parse()
            .map_err(|e| Error::NiriError(format!("Invalid recipient {}: {}", recipient, e)))?);
    }

    for cc in &email_config.cc_addresses {
        builder = builder.cc(cc
            .parse()
            .map_err(|e| Error::NiriError(format!("Invalid CC {}: {}", cc, e)))?);
    }

    let email = builder
        .multipart(multipart)
        .map_err(|e| Error::NiriError(format!("Failed to build email: {}", e)))?;

    mailer
        .send(&email)
        .map_err(|e| Error::NiriError(format!("Failed to send email: {}", e)))?;

    let to_count = email_config.to_addresses.len();
    let cc_count = email_config.cc_addresses.len();
    let total_recipients = to_count + cc_count;

    println!(
        "Successfully sent {} report ({} to {}) to {} recipient(s){}{}",
        period_name,
        bounds.start_date,
        bounds.end_date,
        total_recipients,
        if to_count > 0 {
            format!(" [TO: {}]", email_config.to_addresses.join(", "))
        } else {
            String::new()
        },
        if cc_count > 0 {
            format!(" [CC: {}]", email_config.cc_addresses.join(", "))
        } else {
            String::new()
        },
    );

    Ok(())
}

fn build_mailer(config: &Email) -> Result<SmtpTransport, Error> {
    let creds = Credentials::new(config.smtp_user(), config.smtp_password());

    // Use implicit TLS for port 465 (more secure), STARTTLS for 587 (standard)
    let mailer = if config.smtp_port == 465 {
        SmtpTransport::relay(&config.smtp_host)
            .map_err(|e| Error::NiriError(format!("Failed to create SMTP relay (TLS): {}", e)))?
            .port(config.smtp_port)
            .credentials(creds)
            .timeout(Some(std::time::Duration::from_secs(30)))
            .build()
    } else {
        SmtpTransport::starttls_relay(&config.smtp_host)
            .map_err(|e| {
                Error::NiriError(format!("Failed to create SMTP relay (STARTTLS): {}", e))
            })?
            .port(config.smtp_port)
            .credentials(creds)
            .timeout(Some(std::time::Duration::from_secs(30)))
            .build()
    };

    Ok(mailer)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn build_html_summary(bounds: &report::TimeBounds, period_name: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <style>
        body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; color: #333; }}
        .header {{ background: #2563eb; color: white; padding: 20px; border-radius: 8px 8px 0 0; }}
        .content {{ padding: 20px; background: #f8fafc; border: 1px solid #e2e8f0; }}
        .footer {{ padding: 15px; background: #f1f5f9; border-radius: 0 0 8px 8px; font-size: 12px; color: #64748b; }}
    </style>
</head>
<body>
    <div class="header">
        <h2 style="margin: 0;">{} Activity Report</h2>
        <p style="margin: 5px 0 0 0; opacity: 0.9;">{} to {}</p>
    </div>
    <div class="content">
        <p>Your activity report for the period is attached as an Excel spreadsheet.</p>
        <p>The report includes:</p>
        <ul>
            <li>Daily summary with screen time, productive time, and ratios</li>
            <li>Weekly totals and averages (calculated using workdays only)</li>
        </ul>
    </div>
    <div class="footer">
        <p>Generated on {} &bull; <a href="https://github.com/coleleavitt/niri-activity-rs" style="color: #2563eb;">View on GitHub</a></p>
    </div>
</body>
</html>"#,
        html_escape(period_name),
        html_escape(&bounds.start_date.to_string()),
        html_escape(&bounds.end_date.to_string()),
        html_escape(&Local::now().format("%Y-%m-%d %H:%M:%S").to_string())
    )
}

fn build_text_summary(bounds: &report::TimeBounds, period_name: &str) -> String {
    format!(
        "{} Activity Report\n\
         Period: {} to {}\n\n\
         Your activity report is attached as an Excel spreadsheet.\n\n\
         The report includes:\n\
         - Daily summary with screen time, productive time, and ratios\n\
         - Weekly totals and averages (calculated using workdays only)\n\n\
         Generated on {}\n\
         View on GitHub: https://github.com/coleleavitt/niri-activity-rs",
        period_name,
        bounds.start_date,
        bounds.end_date,
        Local::now().format("%Y-%m-%d %H:%M:%S")
    )
}

pub fn send_weekly_report(app: &App) -> Result<(), Error> {
    send_report(app, TimeRange::LastWeek, "Weekly")
}

pub fn send_monthly_report(app: &App) -> Result<(), Error> {
    send_report(app, TimeRange::LastMonth, "Monthly")
}

pub fn test_email_config(config: &Config) -> Result<(), Error> {
    let email_config = &config.email;

    if !email_config.enabled {
        return Err(Error::NiriError("Email is not enabled in config".into()));
    }

    let mailer = build_mailer(email_config)?;

    let test_email = Message::builder()
        .from(
            email_config
                .from_address
                .parse()
                .map_err(|e| Error::NiriError(format!("Invalid from address: {}", e)))?,
        )
        .to(email_config
            .to_addresses
            .first()
            .ok_or_else(|| Error::NiriError("No recipient addresses configured".into()))?
            .parse()
            .map_err(|e| Error::NiriError(format!("Invalid recipient: {}", e)))?)
        .subject(format!(
            "{} Test Email",
            email_config.subject_prefix
        ))
        .body("This is a test email from niri-activity-rs. If you received this, email is configured correctly.".to_string())
        .map_err(|e| Error::NiriError(format!("Failed to build test email: {}", e)))?;

    mailer
        .send(&test_email)
        .map_err(|e| Error::NiriError(format!("Failed to send test email: {}", e)))?;

    println!(
        "Test email sent successfully to {}",
        email_config
            .to_addresses
            .first()
            .map(|s| s.as_str())
            .unwrap_or("<none>")
    );

    Ok(())
}

pub fn secure_config_permissions(config_path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(config_path)
            .map_err(|e| Error::NiriError(format!("Cannot read config metadata: {}", e)))?
            .permissions();
        perms.set_mode(0o600);
        fs::set_permissions(config_path, perms)
            .map_err(|e| Error::NiriError(format!("Failed to set config permissions: {}", e)))?;
        println!("Set {} permissions to 600", config_path.display());
    }
    Ok(())
}
